//! §14: *"Agent limits are mandatory, not advisory — a runaway loop on a
//! metered provider is a support incident."*
//!
//! > *"Exceeding one stops the run with `BudgetExceeded` and a 'continue
//! > anyway' button. Codex's own limits apply too, but **aibo must not depend
//! > on them**."*
//!
//! That last clause is the whole reason this module exists. `CodexAppServer`
//! enforces its own ceilings and `ClaudeCodeCli` enforces different ones; a run
//! that trusts the delegate has no ceiling at all the day the delegate changes
//! its defaults. So every [`AgentStep`] passes through
//! [`aibo_core::cost::AgentLimitTracker`] on the way out, whatever produced it.
//!
//! §13's other agent rule is honoured by the engine, not here: *"pressing the
//! hotkey during an agent run does not interrupt — the run continues in the
//! task window and a fresh panel opens."* Agent cancellation tokens live in
//! their own registry, which is why [`crate::Engine::cancel_task`] is a
//! separate call from [`crate::Engine::cancel`].

use std::sync::Arc;
use std::time::Instant;

use aibo_core::AiboError;
use aibo_core::cost::AgentLimitTracker;
use aibo_core::traits::AgentBackend;
use aibo_core::types::{AgentLimits, AgentOutcome, AgentStatus, AgentStep, AgentTask};
use futures::StreamExt as _;
use uuid::Uuid;

use crate::Engine;

/// The terminal payload for a run that ended without the backend supplying one.
///
/// `AgentLimitTracker` counts steps, tool calls, tokens and wall clock but does
/// not assemble an [`AgentOutcome`]; this is that projection, and it exists so
/// a cancelled run still reports how far it got.
fn outcome(tracker: &AgentLimitTracker, status: AgentStatus) -> AgentOutcome {
    AgentOutcome {
        status,
        usage: aibo_core::types::Usage {
            // The tracker keeps a running total rather than the five-way split,
            // and inventing a split here would make the spend meter lie. Output
            // is the honest bucket for "tokens this run produced".
            output_tokens: tracker.tokens(),
            ..aibo_core::types::Usage::default()
        },
        steps: tracker.steps(),
    }
}

/// What the engine tells its caller about a run.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AgentEvent {
    /// The run started.
    Started {
        /// Task id.
        task: Uuid,
        /// The instruction, shown as the task window's subject and as approval
        /// provenance (§5 rule 3).
        instruction: String,
    },
    /// One step, forwarded verbatim.
    Step(Box<AgentStep>),
    /// The run stopped.
    Finished {
        /// Task id.
        task: Uuid,
        /// How it ended.
        outcome: AgentOutcome,
    },
    /// The run failed, including on a §14 ceiling.
    Failed {
        /// Task id.
        task: Uuid,
        /// The failure. [`AiboError::BudgetExceeded`] is the one the UI pairs
        /// with a "continue anyway" button.
        error: Arc<AiboError>,
    },
}

/// Where [`AgentEvent`]s go.
#[derive(Debug, Clone)]
pub struct AgentSink {
    tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
}

impl AgentSink {
    /// Wrap an existing sender.
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }

    /// A sink and its receiver.
    pub fn channel() -> (Self, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self::new(tx), rx)
    }

    /// Emit one event.
    pub fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

impl Engine {
    /// Run an agent task with §14's limits enforced by aibo, not by the
    /// delegate.
    ///
    /// The run gets its **own** cancellation token, registered under the task
    /// id. §13: a new panel submission must not interrupt it.
    ///
    /// Returns the terminal [`AgentOutcome`], or the error that stopped the
    /// run. A ceiling produces [`AiboError::BudgetExceeded`]; the caller offers
    /// "continue anyway" and calls this again with a raised [`AgentLimits`].
    pub async fn run_agent(
        &self,
        task: AgentTask,
        limits: AgentLimits,
        backend: Arc<dyn AgentBackend>,
        events: &AgentSink,
    ) -> Result<AgentOutcome, Arc<AiboError>> {
        let task_id = task.id;
        let cancel = self.register_task(task_id);
        let result = self
            .run_agent_inner(task, limits, backend, events, &cancel)
            .await;
        self.retire_task(task_id);

        match &result {
            Ok(outcome) => events.emit(AgentEvent::Finished {
                task: task_id,
                outcome: outcome.clone(),
            }),
            Err(error) => events.emit(AgentEvent::Failed {
                task: task_id,
                error: error.clone(),
            }),
        }
        result
    }

    async fn run_agent_inner(
        &self,
        task: AgentTask,
        limits: AgentLimits,
        backend: Arc<dyn AgentBackend>,
        events: &AgentSink,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<AgentOutcome, Arc<AiboError>> {
        let task_id = task.id;
        events.emit(AgentEvent::Started {
            task: task_id,
            instruction: task.instruction.clone(),
        });

        let mut stream = backend
            .run(task, limits, cancel.clone())
            .await
            .map_err(Arc::new)?;

        let mut tracker = AgentLimitTracker::started_at(limits, Instant::now());

        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Ok(outcome(&tracker, AgentStatus::Cancelled));
                }
                next = stream.next() => next,
            };

            let Some(item) = next else {
                // The backend hung up without a terminal step. That is a
                // failure of the delegate, not a completed run — reporting it
                // as `Completed` would show a green tick over nothing.
                return Ok(outcome(
                    &tracker,
                    AgentStatus::Failed("the agent backend ended without a result".to_owned()),
                ));
            };

            let step = item.map_err(Arc::new)?;

            // §14: the wall clock is checked on every step, not only when one
            // finishes — a single tool call that runs for an hour must still
            // stop the run.
            tracker.check_now().map_err(Arc::new)?;
            tracker.on_step().map_err(Arc::new)?;
            if matches!(step, AgentStep::ToolUse { .. }) {
                tracker.on_tool_call().map_err(Arc::new)?;
            }
            if let AgentStep::Done(done) = &step {
                tracker.on_usage(&done.usage, None).map_err(Arc::new)?;
            }

            events.emit(AgentEvent::Step(Box::new(step.clone())));

            if let AgentStep::Done(done) = step {
                return Ok(done);
            }

            // A delegate that emits steps without ever awaiting I/O — a broken
            // `app-server`, or a `NativeLoop` spinning on a cached tool result
            // — would otherwise never return to the scheduler, and §13's
            // cancellation would not be observed until the run ended on its
            // own. One yield per step costs nothing at §14's 25-step default.
            tokio::task::yield_now().await;
        }
    }
}
