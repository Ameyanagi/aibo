//! A scripted [`Provider`] and [`AgentBackend`] for the orchestration tests.
//!
//! §18: the orchestration layer is the part of aibo that must be testable
//! without a network, and this is what makes that true. Nothing here opens a
//! socket, reads the keychain or touches SQLite.

#![allow(dead_code)] // Each integration test binary uses a different subset.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aibo_core::error::{AiboError, Result};
use aibo_core::traits::{AgentBackend, Provider};
use aibo_core::types::{
    AgentFeatures, AgentLimits, AgentStep, AgentTask, BoxStream, Capabilities, ChatRequest, Health,
    ModelInfo, ProviderId, SandboxKind, StopReason, StreamEvent, Usage,
};
use async_trait::async_trait;
use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

/// What one `chat` call should do.
pub enum Script {
    /// Yield these, in order, then end.
    Events(Vec<Result<StreamEvent>>),
    /// Fail before returning a stream at all — a connect failure, a 5xx from
    /// the initial POST, a 400.
    Reject(AiboError),
    /// Yield these, then never terminate. The only way to observe
    /// cancellation without a timing race.
    Hang(Vec<StreamEvent>),
}

impl Script {
    /// The ordinary success: some text, a usage report and a clean stop.
    pub fn ok(text: &str) -> Self {
        Self::Events(vec![
            Ok(StreamEvent::Text(text.to_owned())),
            Ok(StreamEvent::Usage(Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Usage::default()
            })),
            Ok(StreamEvent::Done(StopReason::EndTurn)),
        ])
    }

    /// Text, then a mid-stream failure. §13's "never retry after a partial
    /// stream" case.
    pub fn breaks_after(text: &str, error: AiboError) -> Self {
        Self::Events(vec![Ok(StreamEvent::Text(text.to_owned())), Err(error)])
    }
}

enum Fallback {
    Text(String),
    Error(Box<dyn Fn() -> AiboError + Send + Sync>),
}

struct Inner {
    id: ProviderId,
    capabilities: Capabilities,
    scripts: Mutex<VecDeque<Script>>,
    healths: Mutex<VecDeque<Result<Health>>>,
    fallback: Fallback,
    chat_calls: AtomicUsize,
    health_calls: AtomicUsize,
    requests: Mutex<Vec<ChatRequest>>,
}

/// A provider whose every response is scripted. Cheap to clone; clones share
/// one script queue and one call counter.
#[derive(Clone)]
pub struct Mock {
    inner: Arc<Inner>,
}

impl Mock {
    /// A provider that answers `"ok"` to anything not explicitly scripted.
    pub fn new(id: ProviderId) -> Self {
        Self::with_fallback(id, Fallback::Text("ok".to_owned()))
    }

    /// A provider that fails every unscripted call with a fresh error.
    pub fn always_failing(
        id: ProviderId,
        factory: impl Fn() -> AiboError + Send + Sync + 'static,
    ) -> Self {
        Self::with_fallback(id, Fallback::Error(Box::new(factory)))
    }

    fn with_fallback(id: ProviderId, fallback: Fallback) -> Self {
        Self {
            inner: Arc::new(Inner {
                id,
                capabilities: Capabilities {
                    max_context: 128_000,
                    max_output: Some(4_096),
                    ..Capabilities::default()
                },
                scripts: Mutex::new(VecDeque::new()),
                healths: Mutex::new(VecDeque::new()),
                fallback,
                chat_calls: AtomicUsize::new(0),
                health_calls: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The provider id, without needing [`Provider`] in scope.
    pub fn id(&self) -> ProviderId {
        self.inner.id.clone()
    }

    /// Queue one scripted `chat` response.
    pub fn push(&self, script: Script) -> &Self {
        self.inner.scripts.lock().unwrap().push_back(script);
        self
    }

    /// Queue one scripted `health` response.
    pub fn push_health(&self, health: Result<Health>) -> &Self {
        self.inner.healths.lock().unwrap().push_back(health);
        self
    }

    /// How many times `chat` was called.
    pub fn chat_calls(&self) -> usize {
        self.inner.chat_calls.load(Ordering::SeqCst)
    }

    /// How many times `health` was called.
    pub fn health_calls(&self) -> usize {
        self.inner.health_calls.load(Ordering::SeqCst)
    }

    /// Every request that reached the wire, in order.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.inner.requests.lock().unwrap().clone()
    }

    /// As a registry entry.
    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[async_trait]
impl Provider for Mock {
    fn id(&self) -> ProviderId {
        self.inner.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities.clone()
    }

    async fn chat(
        &self,
        req: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        self.inner.chat_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.requests.lock().unwrap().push(req);

        let script = self.inner.scripts.lock().unwrap().pop_front();
        let script = match script {
            Some(script) => script,
            None => match &self.inner.fallback {
                Fallback::Text(text) => Script::ok(text),
                Fallback::Error(factory) => Script::Reject(factory()),
            },
        };

        match script {
            Script::Events(items) => Ok(Box::pin(futures::stream::iter(items))),
            Script::Reject(error) => Err(error),
            Script::Hang(items) => Ok(Box::pin(
                futures::stream::iter(items.into_iter().map(Ok)).chain(futures::stream::pending()),
            )),
        }
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    async fn health(&self) -> Result<Health> {
        self.inner.health_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .healths
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(Health::Ok {
                latency: Duration::from_millis(10),
            }))
    }
}

// ---------------------------------------------------------------------------
// Agent backend
// ---------------------------------------------------------------------------

/// An agent backend that emits a fixed script of steps, or repeats one step
/// forever so §14's ceilings have something to stop.
pub struct MockAgent {
    steps: Mutex<Option<Vec<Result<AgentStep>>>>,
    repeat: Option<AgentStep>,
}

impl MockAgent {
    /// Emit these steps and stop.
    pub fn scripted(steps: Vec<Result<AgentStep>>) -> Arc<dyn AgentBackend> {
        Arc::new(Self {
            steps: Mutex::new(Some(steps)),
            repeat: None,
        })
    }

    /// Emit this step forever. A runaway loop, which is exactly what §14's
    /// limits exist to stop.
    pub fn runaway(step: AgentStep) -> Arc<dyn AgentBackend> {
        Arc::new(Self {
            steps: Mutex::new(None),
            repeat: Some(step),
        })
    }
}

#[async_trait]
impl AgentBackend for MockAgent {
    async fn run(
        &self,
        _task: AgentTask,
        _limits: AgentLimits,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<AgentStep>>> {
        if let Some(step) = self.repeat.clone() {
            return Ok(Box::pin(futures::stream::repeat_with(move || {
                Ok(step.clone())
            })));
        }
        let steps = self.steps.lock().unwrap().take().unwrap_or_default();
        Ok(Box::pin(futures::stream::iter(steps)))
    }

    fn supports(&self) -> AgentFeatures {
        AgentFeatures {
            file_edits: false,
            shell: false,
            mcp: false,
            pre_write_approval: false,
            streaming_diffs: false,
            model_selection: false,
            resume: false,
            sandbox: SandboxKind::None,
        }
    }
}
