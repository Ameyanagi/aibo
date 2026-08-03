//! `codex app-server` over JSON-RPC/stdio. Protocol shape gated on spike S5 (§3).
//!
//! # What this is
//!
//! §3: `codex app-server` is "a documented JSON-RPC 2.0 protocol — the same
//! interface that powers OpenAI's own VS Code extension", with events,
//! approvals, skills, apps and threads. It already implements the approval
//! protocol, sandboxing, an MCP client, skills and thread persistence, which is
//! why §11 calls tier 4 delegating to Codex's sandbox **the strongest
//! configuration in the product, not the weakest**, and why §11 says to "map
//! aibo's permission UI onto Codex's approval requests rather than building a
//! parallel one". That mapping is [`CodexAppServer`]'s main job.
//!
//! # Three things the plan is emphatic about, encoded here
//!
//! 1. **NDJSON over stdio, and a strict JSON-RPC codec will not work.** The
//!    `"jsonrpc":"2.0"` field is deliberately omitted (§3), so the framing lives
//!    in [`protocol`] and is hand-rolled. The unix-socket transport carries
//!    *websocket* frames over an HTTP Upgrade, not NDJSON, and websocket is
//!    marked experimental — stdio is the default and the only one implemented.
//! 2. **Rate limits are a separate channel.** `account/read` carries none;
//!    `account/rateLimits/read` and `account/rateLimits/updated` do, and the
//!    notification is "a **sparse rolling update you must merge**, not a full
//!    snapshot" (§3). [`CodexAppServer::rate_limits`] holds the merged state.
//! 3. **Version handling needs a clear error, not a deserialisation failure.**
//!    §3: `initialize` capabilities "do not negotiate protocol versions", so a
//!    floor alone is insufficient; parse permissively and fail with "your
//!    `codex` is newer/older than this build supports".
//!
//! # Session identity
//!
//! §3b, chosen for v1: **Do is a separate, explicitly non-continuous surface.**
//! Every run starts a *new* Codex thread seeded with a replayable plain-text
//! summary of the aibo-side context — nothing else carries across, because the
//! two session models (aibo's encrypted SQLite vs Codex's `~/.codex` threads)
//! share no representation. The task window says so plainly; there is no
//! pretence of continuity here either. Cost accounting *does* stay unified:
//! Codex turns report usage through [`AgentStep::Done`] into the same ledger.
//!
//! # Process lifetime
//!
//! The child is spawned into a managed Unix process group or Windows Job Object
//! so cancellation and bounded shutdown include its delegated descendants. See
//! [`spawn_in_process_group`].
//!
//! SPIKE: S5 — every method name and payload shape used here is unverified
//! against a real binary. §20 defines S5 as: "Spawn `codex` over stdio,
//! `initialize`, `account/read`, run one thread. Does published protocol 0.63.0
//! deserialise today's binary? Minimum version floor?" Until it reports, the
//! constants in [`protocol::method`] are the interface, not a fact.

pub mod protocol;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use aibo_core::error::{AiboError, Result};
use aibo_core::traits::{AgentBackend, ChildProcessObserver, ChildProcessRegistration};
use aibo_core::types::{
    AgentFeatures, AgentLimits, AgentStatus, AgentStep, AgentTask, ApprovalDecision,
    ApprovalRequest, BoxStream, BudgetKind, SandboxKind, ToolTier,
};
use async_trait::async_trait;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
#[cfg(windows)]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::limits::LimitTracker;
use crate::native_loop::fenced_context;
use crate::permission_gate::{ApprovalResponse, ApprovalUi, verify_approval_response};
use crate::process_io::{
    BoundedLine, MAX_LOG_LINE_BYTES, MAX_PROTOCOL_LINE_BYTES, read_bounded_line,
    safe_child_environment,
};
use protocol::{
    AccountRead, ExecApprovalParams, Inbound, InitializeParams, InitializeResult, InterruptParams,
    OutboundNotification, OutboundRequest, OutboundResponse, PatchApprovalParams,
    RateLimitSnapshot, RawMessage, RequestId, RpcError, SendMessageParams, ThreadEvent, ThreadItem,
    ThreadStartParams, ThreadStartResult, Version, VersionVerdict, method,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures specific to the app-server transport.
///
/// `thiserror` because `aibo-agent` is a library (`anyhow` is confined to the
/// binary). These are converted to [`AiboError`] by [`CodexError::into_aibo`];
/// an orphan-rule-legal `From` impl is not possible from this crate, so the
/// conversion is an inherent method instead.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodexError {
    /// The `codex` binary is not installed or not on `PATH`.
    #[error("`{program}` is not installed or not on PATH")]
    NotInstalled {
        /// The program that could not be spawned.
        program: String,
    },

    /// The child died before the handshake finished.
    #[error("codex app-server exited before the handshake completed")]
    EarlyExit,

    /// §3: fail with a clear "your `codex` is newer/older than this build
    /// supports" rather than a deserialisation error.
    #[error(
        "your `codex` speaks app-server protocol {found}, and this build of aibo supports \
         {min}..={max_tested} — update {who}"
    )]
    VersionMismatch {
        /// What the binary reported.
        found: Version,
        /// Floor.
        min: Version,
        /// Highest tested.
        max_tested: Version,
        /// `codex` or `aibo`, whichever the user should update.
        who: &'static str,
    },

    /// The `initialize` result carried nothing version-shaped.
    #[error(
        "could not determine your `codex` app-server protocol version from its initialize response"
    )]
    VersionUnknown,

    /// The server answered a request with an error object.
    #[error("codex app-server rejected `{method}`: {source}")]
    Rpc {
        /// The method that failed.
        method: &'static str,
        /// The server's error object.
        #[source]
        source: RpcError,
    },

    /// The stdio transport closed underneath a pending request.
    #[error("the connection to codex app-server closed")]
    TransportClosed,

    /// A request did not come back in time.
    #[error("timed out waiting for `{method}` from codex app-server")]
    RequestTimeout {
        /// The method that timed out.
        method: &'static str,
    },

    /// The process tree did not terminate and reap in time.
    #[error("timed out shutting down the codex app-server process tree")]
    ShutdownTimeout,

    /// A response did not have the expected shape.
    #[error("unexpected response to `{method}` from codex app-server: {detail}")]
    Malformed {
        /// The method whose response could not be understood.
        method: &'static str,
        /// What was wrong. **Never** the raw payload — it can contain user
        /// content (§11 threat model, secrets in logs).
        detail: String,
    },

    /// Underlying I/O failure.
    #[error("codex app-server I/O failure")]
    Io(#[from] std::io::Error),
}

impl CodexError {
    /// Convert to the error the rest of aibo speaks (§13).
    ///
    /// A missing binary is [`AiboError::AgentBackendMissing`], which §13 gives a
    /// specific inline treatment with an install action; everything else is
    /// [`AiboError::Internal`], which the UI renders as a generic message plus
    /// "copy diagnostics" — never raw.
    pub fn into_aibo(self) -> AiboError {
        match self {
            CodexError::NotInstalled { .. } => AiboError::AgentBackendMissing { which: "codex" },
            other => AiboError::Internal(Box::new(other)),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How to launch and talk to `codex app-server`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexConfig {
    /// The binary. Resolved through `PATH` unless absolute.
    pub program: PathBuf,
    /// Arguments. The default selects the app-server subcommand.
    pub args: Vec<String>,
    /// `CODEX_HOME`, when the user points aibo at a non-default Codex profile.
    ///
    /// Note §3a: aibo does **not** read `$CODEX_HOME/auth.json`. That design was
    /// superseded — refresh tokens are single-use and two processes sharing one
    /// file log the user out of their own Codex. This only tells the child which
    /// profile to use; aibo never opens it.
    pub codex_home: Option<PathBuf>,
    /// Extra environment for the child.
    pub env: Vec<(String, String)>,
    /// How long the handshake may take before the backend is declared missing.
    pub startup_timeout: Duration,
    /// Per-request timeout for ordinary RPCs.
    pub request_timeout: Duration,
    /// Time allowed to terminate and reap the app-server process tree.
    pub shutdown_timeout: Duration,
    /// Refuse to run against a protocol version newer than [`protocol::MAX_TESTED`].
    ///
    /// Off by default: §3 makes permissive parsing the strategy, and a hard
    /// upper bound would break aibo on every Codex release. Turn it on in CI so
    /// a version bump is caught by a test rather than by a user.
    pub strict_version: bool,
    /// Codex approval policy passed to `thread/start`.
    ///
    /// `None` leaves Codex's default, which is what §11 recommends: "use Codex's
    /// own approval protocol as the real gate". Setting `"never"` is only
    /// legitimate for the §3a outcome-2 fallback — a tools-disabled inference
    /// turn — where there is nothing to approve.
    pub approval_policy: Option<String>,
    /// Codex sandbox policy passed to `thread/start`. §11: the sandbox is the
    /// boundary; aibo's own path checks are defence in depth.
    pub sandbox_policy: Option<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from("codex"),
            args: vec!["app-server".to_owned()],
            codex_home: None,
            env: Vec::new(),
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(5),
            strict_version: false,
            approval_policy: None,
            sandbox_policy: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Process spawning
// ---------------------------------------------------------------------------

/// Spawn a child in a Unix process group or Windows Job Object.
///
/// `process-wrap` supplies the platform implementation without weakening this
/// crate's `unsafe_code = "forbid"` boundary. Its child wrapper kills and waits
/// for the entire group/job, including Codex-spawned MCP servers and commands.
fn spawn_in_process_group(
    config: &CodexConfig,
    cwd: Option<&PathBuf>,
) -> std::io::Result<Box<dyn ChildWrapper>> {
    let mut cmd = Command::new(&config.program);
    cmd.args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.env_clear();
    cmd.envs(safe_child_environment());

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Some(home) = &config.codex_home {
        cmd.env("CODEX_HOME", home);
    }
    for (k, v) in &config.env {
        cmd.env(k, v);
    }

    let mut cmd = CommandWrap::from(cmd);
    cmd.wrap(KillOnDrop);
    #[cfg(unix)]
    cmd.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    {
        cmd.wrap(CreationFlags(CREATE_NO_WINDOW));
        cmd.wrap(JobObject);
    }
    cmd.spawn()
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Capacity of the inbound broadcast.
///
/// Large because a Codex turn is chatty and a slow subscriber that lags loses
/// events irrecoverably. Lag is logged loudly rather than swallowed.
const INBOUND_CAPACITY: usize = 4096;
const INTERNAL_TRANSPORT_CLOSED: &str = "__aibo/transportClosed";
const INTERRUPT_TIMEOUT: Duration = Duration::from_millis(500);

/// In-flight requests, keyed by outgoing id.
type Pending = Arc<std::sync::Mutex<HashMap<i64, oneshot::Sender<RpcResult>>>>;

/// A response body or the server's error object.
type RpcResult = std::result::Result<Value, RpcError>;

/// Removes an in-flight request even when its future is cancelled by dropping.
struct PendingGuard {
    pending: Pending,
    id: i64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// One live `codex app-server` child and its framing.
struct Connection {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    next_id: AtomicI64,
    /// Shared with the reader task, which is the only other owner.
    pending: Pending,
    inbound: broadcast::Sender<Arc<Inbound>>,
    /// `None` after the first shutdown; keeping this behind its own lock lets
    /// shutdown terminate the process even while turns still hold an `Arc`.
    child: tokio::sync::Mutex<Option<Box<dyn ChildWrapper>>>,
    /// Keeps the crash-recovery ledger entry live exactly as long as the
    /// connection owns the child.
    _registration: ChildProcessRegistration,
    request_timeout: Duration,
    closed: Arc<AtomicBool>,
    protocol_version: Version,
}

impl Connection {
    /// Spawn the child and wire up the reader. Does **not** handshake — see
    /// [`Connection::handshake`].
    async fn open(
        config: &CodexConfig,
        cwd: Option<&PathBuf>,
        process_observer: Option<Arc<dyn ChildProcessObserver>>,
    ) -> std::result::Result<Self, CodexError> {
        let mut child = spawn_in_process_group(config, cwd).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CodexError::NotInstalled {
                    program: config.program.display().to_string(),
                }
            } else {
                CodexError::Io(e)
            }
        })?;
        let identity = config
            .program
            .file_name()
            .unwrap_or(config.program.as_os_str())
            .to_string_lossy();
        let registration = ChildProcessRegistration::new(process_observer, child.id(), &identity);

        let stdin = child.stdin().take().ok_or(CodexError::EarlyExit)?;
        let stdout = child.stdout().take().ok_or(CodexError::EarlyExit)?;
        let stderr = child.stderr().take().ok_or(CodexError::EarlyExit)?;

        let (inbound, _) = broadcast::channel(INBOUND_CAPACITY);
        let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));

        // Codex logs to stderr; drain it so the pipe never fills and blocks the
        // child, and so a crash leaves something in the diagnostics.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            loop {
                match read_bounded_line(&mut reader, MAX_LOG_LINE_BYTES).await {
                    Ok(BoundedLine::Line(line)) => {
                        tracing::debug!(target: "codex.stderr", bytes = line.len(), "codex emitted a diagnostic line");
                    }
                    Ok(BoundedLine::TooLong) => {
                        tracing::debug!(target: "codex.stderr", "[diagnostic line truncated by aibo]");
                    }
                    Ok(BoundedLine::Eof) | Err(_) => break,
                }
            }
        });

        let closed = Arc::new(AtomicBool::new(false));
        Self::spawn_reader(
            stdout,
            Arc::clone(&pending),
            inbound.clone(),
            Arc::clone(&closed),
        );

        Ok(Self {
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            next_id: AtomicI64::new(1),
            pending,
            inbound,
            child: tokio::sync::Mutex::new(Some(child)),
            _registration: registration,
            request_timeout: config.request_timeout,
            closed,
            // Replaced by `handshake`.
            protocol_version: Version::new(0, 0, 0),
        })
    }

    /// Read NDJSON frames forever: responses go to their waiter, everything else
    /// fans out to subscribers.
    fn spawn_reader(
        stdout: tokio::process::ChildStdout,
        shared: Pending,
        inbound: broadcast::Sender<Arc<Inbound>>,
        closed: Arc<AtomicBool>,
    ) {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_bounded_line(&mut reader, MAX_PROTOCOL_LINE_BYTES).await {
                    Ok(BoundedLine::Line(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let raw: RawMessage = match serde_json::from_str(&line) {
                            Ok(raw) => raw,
                            Err(e) => {
                                // §3: parse permissively. A frame we cannot read
                                // is dropped, never fatal. The payload is not
                                // logged — it can carry user content.
                                tracing::warn!(error = %e, "undecodable app-server frame dropped");
                                continue;
                            }
                        };
                        let Some(message) = raw.classify() else {
                            tracing::trace!("app-server frame matched no known shape");
                            continue;
                        };
                        if let Inbound::Response { id, outcome } = message {
                            let RequestId::Number(n) = id else {
                                tracing::warn!("app-server response carried a non-numeric id");
                                continue;
                            };
                            let waiter =
                                shared.lock().unwrap_or_else(|e| e.into_inner()).remove(&n);
                            match waiter {
                                Some(tx) => {
                                    let _ = tx.send(outcome);
                                }
                                None => tracing::warn!(id = n, "response with no pending request"),
                            }
                            continue;
                        }
                        // Notifications and server → client requests fan out.
                        let _ = inbound.send(Arc::new(message));
                    }
                    Ok(BoundedLine::TooLong) => {
                        tracing::warn!(
                            max_bytes = MAX_PROTOCOL_LINE_BYTES,
                            "oversized app-server frame closed the transport"
                        );
                        break;
                    }
                    Ok(BoundedLine::Eof) => {
                        tracing::info!("codex app-server closed stdout");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "codex app-server stdout read failed");
                        break;
                    }
                }
            }
            closed.store(true, Ordering::Release);
            let _ = inbound.send(Arc::new(Inbound::Notification {
                method: INTERNAL_TRANSPORT_CLOSED.to_owned(),
                params: Value::Null,
            }));
            // Release everyone still waiting, rather than letting them time out.
            let mut map = shared.lock().unwrap_or_else(|e| e.into_inner());
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(RpcError {
                    code: -1,
                    message: "connection closed".to_owned(),
                    data: None,
                }));
            }
        });
    }

    fn subscribe(&self) -> broadcast::Receiver<Arc<Inbound>> {
        self.inbound.subscribe()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    async fn write_line_unbounded(&self, line: String) -> std::result::Result<(), CodexError> {
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin.as_mut().ok_or(CodexError::TransportClosed)?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Issue a request and await its response.
    async fn request(
        &self,
        method_name: &'static str,
        params: Option<Value>,
        timeout: Duration,
    ) -> std::result::Result<Value, CodexError> {
        if self.is_closed() {
            return Err(CodexError::TransportClosed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let _pending_guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id,
        };

        let frame = serde_json::to_string(&OutboundRequest {
            id: RequestId::Number(id),
            method: method_name,
            params,
        })
        .map_err(|e| CodexError::Malformed {
            method: method_name,
            detail: format!("could not encode request: {e}"),
        })?;

        let operation = async {
            self.write_line_unbounded(frame).await?;
            match rx.await {
                Err(_) => Err(CodexError::TransportClosed),
                Ok(Err(source)) => Err(CodexError::Rpc {
                    method: method_name,
                    source,
                }),
                Ok(Ok(value)) => Ok(value),
            }
        };
        match tokio::time::timeout(timeout, operation).await {
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(CodexError::RequestTimeout {
                    method: method_name,
                })
            }
            Ok(result) => {
                if result.is_err() {
                    self.pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                }
                result
            }
        }
    }

    async fn notify_with_timeout(
        &self,
        method_name: &'static str,
        params: Option<Value>,
        timeout: Duration,
    ) -> std::result::Result<(), CodexError> {
        let frame = serde_json::to_string(&OutboundNotification {
            method: method_name,
            params,
        })
        .map_err(|e| CodexError::Malformed {
            method: method_name,
            detail: format!("could not encode notification: {e}"),
        })?;
        tokio::time::timeout(timeout, self.write_line_unbounded(frame))
            .await
            .map_err(|_| CodexError::RequestTimeout {
                method: method_name,
            })?
    }

    /// Answer a server → client request.
    async fn respond(
        &self,
        id: RequestId,
        result: Option<Value>,
        error: Option<RpcError>,
    ) -> std::result::Result<(), CodexError> {
        self.respond_with_timeout(id, result, error, self.request_timeout)
            .await
    }

    async fn respond_with_timeout(
        &self,
        id: RequestId,
        result: Option<Value>,
        error: Option<RpcError>,
        timeout: Duration,
    ) -> std::result::Result<(), CodexError> {
        let frame =
            serde_json::to_string(&OutboundResponse { id, result, error }).map_err(|e| {
                CodexError::Malformed {
                    method: "response",
                    detail: format!("could not encode response: {e}"),
                }
            })?;
        tokio::time::timeout(timeout, self.write_line_unbounded(frame))
            .await
            .map_err(|_| CodexError::RequestTimeout { method: "response" })?
    }

    /// Kill the full process group/job and wait for it to be reaped. Taking the
    /// child makes repeated or concurrent shutdown calls harmless.
    async fn shutdown(&self, timeout: Duration) -> std::result::Result<(), CodexError> {
        self.closed.store(true, Ordering::Release);
        self.stdin.lock().await.take();
        let child = self.child.lock().await.take();
        let Some(mut child) = child else {
            return Ok(());
        };

        if let Err(error) = child.start_kill()
            && child.try_wait()?.is_none()
        {
            return Err(CodexError::Io(error));
        }
        tokio::time::timeout(timeout, child.wait())
            .await
            .map_err(|_| CodexError::ShutdownTimeout)??;
        Ok(())
    }

    /// The handshake, with version-floor detection (§3, S5).
    async fn handshake(&mut self, config: &CodexConfig) -> std::result::Result<(), CodexError> {
        let params = serde_json::to_value(InitializeParams::default()).map_err(|e| {
            CodexError::Malformed {
                method: method::INITIALIZE,
                detail: format!("{e}"),
            }
        })?;
        let raw = self
            .request(method::INITIALIZE, Some(params), config.startup_timeout)
            .await?;

        let result: InitializeResult =
            serde_json::from_value(raw).map_err(|e| CodexError::Malformed {
                method: method::INITIALIZE,
                detail: format!("{e}"),
            })?;

        let found = result
            .detected_version()
            .ok_or(CodexError::VersionUnknown)?;
        match protocol::classify(found) {
            VersionVerdict::TooOld => {
                return Err(CodexError::VersionMismatch {
                    found,
                    min: protocol::MIN_SUPPORTED,
                    max_tested: protocol::MAX_TESTED,
                    who: "codex",
                });
            }
            VersionVerdict::NewerThanTested if config.strict_version => {
                return Err(CodexError::VersionMismatch {
                    found,
                    min: protocol::MIN_SUPPORTED,
                    max_tested: protocol::MAX_TESTED,
                    who: "aibo",
                });
            }
            VersionVerdict::NewerThanTested => {
                // §3: parse permissively. A newer binary usually works, but a
                // shape change surfaces as a *dropped event*, not an error, so
                // this must be loud.
                tracing::warn!(
                    %found,
                    max_tested = %protocol::MAX_TESTED,
                    "codex app-server is newer than this build of aibo was tested against"
                );
            }
            VersionVerdict::Supported => {}
        }
        self.protocol_version = found;

        self.notify_with_timeout(method::INITIALIZED, None, config.startup_timeout)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// The `codex app-server` delegate (§3, §7, §11 tier 4).
pub struct CodexAppServer {
    config: CodexConfig,
    approvals: Arc<dyn ApprovalUi>,
    connection: tokio::sync::Mutex<Option<Arc<Connection>>>,
    rate_limits: Arc<std::sync::Mutex<RateLimitSnapshot>>,
    /// ID-less protocol frames cannot be routed safely between concurrent
    /// turns. Keep one active turn per connection until the protocol guarantees
    /// a thread id on every event and request.
    run_serial: Arc<tokio::sync::Mutex<()>>,
    process_observer: Option<Arc<dyn ChildProcessObserver>>,
}

impl std::fmt::Debug for CodexAppServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAppServer")
            .field("program", &self.config.program)
            .finish_non_exhaustive()
    }
}

impl CodexAppServer {
    /// Build a backend. Nothing is spawned until the first use.
    pub fn new(config: CodexConfig, approvals: Arc<dyn ApprovalUi>) -> Self {
        Self {
            config,
            approvals,
            connection: tokio::sync::Mutex::new(None),
            rate_limits: Arc::new(std::sync::Mutex::new(RateLimitSnapshot::default())),
            run_serial: Arc::new(tokio::sync::Mutex::new(())),
            process_observer: None,
        }
    }

    /// Report the persistent app-server child to the application-owned crash
    /// recovery ledger.
    #[must_use]
    pub fn with_process_observer(mut self, observer: Arc<dyn ChildProcessObserver>) -> Self {
        self.process_observer = Some(observer);
        self
    }

    /// The merged quota state (§3).
    ///
    /// Merged, not replaced: `account/rateLimits/updated` is sparse, and taking
    /// each notification as a snapshot blanks whichever window it omitted.
    pub fn rate_limits(&self) -> RateLimitSnapshot {
        *self.rate_limits.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read the account state (§3). Carries **no** rate limits by design.
    pub async fn account(&self) -> Result<AccountRead> {
        let conn = self.connect(None).await?;
        let raw = conn
            .request(method::ACCOUNT_READ, None, self.config.request_timeout)
            .await
            .map_err(CodexError::into_aibo)?;
        serde_json::from_value(raw).map_err(|e| {
            CodexError::Malformed {
                method: method::ACCOUNT_READ,
                detail: format!("{e}"),
            }
            .into_aibo()
        })
    }

    /// Fetch the quota snapshot and fold it into the merged state (§3).
    pub async fn refresh_rate_limits(&self) -> Result<RateLimitSnapshot> {
        let conn = self.connect(None).await?;
        let raw = conn
            .request(
                method::ACCOUNT_RATE_LIMITS_READ,
                None,
                self.config.request_timeout,
            )
            .await
            .map_err(CodexError::into_aibo)?;
        let snapshot: RateLimitSnapshot = serde_json::from_value(raw).map_err(|e| {
            CodexError::Malformed {
                method: method::ACCOUNT_RATE_LIMITS_READ,
                detail: format!("{e}"),
            }
            .into_aibo()
        })?;
        let mut guard = self.rate_limits.lock().unwrap_or_else(|e| e.into_inner());
        guard.merge(snapshot);
        Ok(*guard)
    }

    /// The negotiated protocol version, once connected.
    pub async fn protocol_version(&self) -> Option<Version> {
        self.connection
            .lock()
            .await
            .as_ref()
            .map(|c| c.protocol_version)
    }

    /// Shut the child down. Idempotent.
    ///
    /// The connection may still be held by an in-flight turn; its internal
    /// child slot is drained here so shutdown remains deterministic.
    pub async fn shutdown(&self) {
        let taken = self.connection.lock().await.take();
        if let Some(connection) = taken
            && let Err(error) = connection.shutdown(self.config.shutdown_timeout).await
        {
            tracing::warn!(%error, "codex app-server shutdown was incomplete");
        }
    }

    /// Get or open the shared connection.
    async fn connect(&self, cwd: Option<&PathBuf>) -> Result<Arc<Connection>> {
        let mut guard = self.connection.lock().await;
        if let Some(existing) = guard.as_ref() {
            if !existing.is_closed() {
                return Ok(Arc::clone(existing));
            }
            guard.take();
        }
        let mut conn = Connection::open(&self.config, cwd, self.process_observer.clone())
            .await
            .map_err(CodexError::into_aibo)?;
        if let Err(error) = conn.handshake(&self.config).await {
            if let Err(shutdown_error) = conn.shutdown(self.config.shutdown_timeout).await {
                tracing::warn!(%shutdown_error, "could not reap codex after failed initialization");
            }
            return Err(error.into_aibo());
        }
        let conn = Arc::new(conn);
        *guard = Some(Arc::clone(&conn));
        Ok(conn)
    }
}

#[async_trait]
impl AgentBackend for CodexAppServer {
    async fn run(
        &self,
        task: AgentTask,
        limits: AgentLimits,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<AgentStep>>> {
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(cancelled_stream(limits)),
            permit = Arc::clone(&self.run_serial).lock_owned() => permit,
        };
        let conn = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(cancelled_stream(limits)),
            result = self.connect(task.workspace.as_ref()) => result?,
        };
        let inbound = conn.subscribe();

        // §3b: Do always starts a **new** thread, seeded with a replayable
        // plain-text summary. There is no continuity with aibo's own session.
        let start_params = ThreadStartParams {
            cwd: task.workspace.as_ref().map(|p| p.display().to_string()),
            model: task.binding.as_ref().map(|b| b.model.clone()),
            approval_policy: self.config.approval_policy.clone(),
            sandbox_policy: self.config.sandbox_policy.clone(),
        };
        let params = serde_json::to_value(start_params).map_err(|e| {
            CodexError::Malformed {
                method: method::THREAD_START,
                detail: format!("{e}"),
            }
            .into_aibo()
        })?;
        let start = conn.request(
            method::THREAD_START,
            Some(params),
            self.config.request_timeout,
        );
        let raw = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = conn.shutdown(self.config.shutdown_timeout).await;
                return Ok(cancelled_stream(limits));
            }
            result = start => result.map_err(CodexError::into_aibo)?,
        };
        let started: ThreadStartResult = serde_json::from_value(raw).map_err(|e| {
            CodexError::Malformed {
                method: method::THREAD_START,
                detail: format!("{e}"),
            }
            .into_aibo()
        })?;
        let thread_id = started.thread_id.ok_or_else(|| {
            CodexError::Malformed {
                method: method::THREAD_START,
                detail: "no thread id in the response".to_owned(),
            }
            .into_aibo()
        })?;

        let (tx, rx) = mpsc::channel::<Result<AgentStep>>(64);
        let turn = Turn {
            conn: Arc::clone(&conn),
            approvals: Arc::clone(&self.approvals),
            rate_limits: Arc::clone(&self.rate_limits),
            request_timeout: self.config.request_timeout,
            shutdown_timeout: self.config.shutdown_timeout,
            thread_id,
            instruction: task.instruction.clone(),
            tracker: LimitTracker::new(limits),
            tx,
            _run_permit: permit,
        };
        let seed = seed_message(&task);
        tokio::spawn(async move { turn.drive(seed, inbound, cancel).await });

        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })))
    }

    fn supports(&self) -> AgentFeatures {
        AgentFeatures {
            file_edits: true,
            shell: true,
            // §3: app-server already implements the MCP client (`rmcp-client`).
            mcp: true,
            // §11: Codex's approval protocol *is* the pre-write gate. This is
            // the reason to prefer this backend.
            pre_write_approval: true,
            streaming_diffs: true,
            model_selection: true,
            // Codex persists threads in `~/.codex`, but §3b chose a
            // non-continuous Do surface for v1: every run is a new thread and
            // nothing is resumed. Flip this only alongside that decision.
            resume: false,
            // §11: "tier 4 delegating to Codex's sandbox is the strongest
            // configuration in the product, not the weakest".
            sandbox: SandboxKind::Delegated,
        }
    }
}

fn cancelled_stream(limits: AgentLimits) -> BoxStream<'static, Result<AgentStep>> {
    let outcome = LimitTracker::new(limits).outcome(AgentStatus::Cancelled);
    Box::pin(futures::stream::once(async move {
        Ok(AgentStep::Done(outcome))
    }))
}

/// Build the text seeded into a fresh Codex thread (§3b).
fn seed_message(task: &AgentTask) -> String {
    let mut out = task.instruction.clone();
    if !task.context.is_empty() {
        out.push_str("\n\n");
        out.push_str(&fenced_context(&task.context));
    }
    out
}

// ---------------------------------------------------------------------------
// One turn
// ---------------------------------------------------------------------------

struct Turn {
    conn: Arc<Connection>,
    approvals: Arc<dyn ApprovalUi>,
    rate_limits: Arc<std::sync::Mutex<RateLimitSnapshot>>,
    request_timeout: Duration,
    shutdown_timeout: Duration,
    thread_id: String,
    /// The user's own typed instruction, carried so every approval prompt can
    /// show what the action traces back to (§5 rule 3).
    instruction: String,
    tracker: LimitTracker,
    tx: mpsc::Sender<Result<AgentStep>>,
    _run_permit: tokio::sync::OwnedMutexGuard<()>,
}

impl Turn {
    async fn drive(
        mut self,
        seed: String,
        mut inbound: broadcast::Receiver<Arc<Inbound>>,
        cancel: CancellationToken,
    ) {
        let send = {
            let conn = Arc::clone(&self.conn);
            let params = serde_json::to_value(SendMessageParams {
                thread_id: self.thread_id.clone(),
                text: seed,
            })
            .unwrap_or(Value::Null);
            let timeout = self.request_timeout;
            async move {
                conn.request(method::THREAD_SEND_MESSAGE, Some(params), timeout)
                    .await
            }
        };
        tokio::pin!(send);
        let mut send_done = false;

        loop {
            tokio::select! {
                biased;

                () = cancel.cancelled() => {
                    self.interrupt().await;
                    if let Err(error) = self.conn.shutdown(self.shutdown_timeout).await {
                        tracing::warn!(%error, "could not reap codex after cancellation");
                    }
                    self.finish(AgentStatus::Cancelled).await;
                    return;
                }

                () = self.tx.closed() => {
                    self.interrupt().await;
                    return;
                }

                () = tokio::time::sleep_until(tokio::time::Instant::from_std(self.tracker.deadline())) => {
                    // §14: mandatory, not advisory. Codex's own limits apply
                    // too, but aibo must not depend on them.
                    self.interrupt().await;
                    if let Err(error) = self.conn.shutdown(self.shutdown_timeout).await {
                        tracing::warn!(%error, "could not reap codex after wall-clock timeout");
                    }
                    self.budget_stop(BudgetKind::Steps).await;
                    return;
                }

                result = &mut send, if !send_done => {
                    send_done = true;
                    match result {
                        Ok(_) => {
                            // Some builds answer immediately and stream events
                            // afterwards; others answer at turn end. Either way
                            // the terminal event is what ends the run.
                        }
                        Err(e) => { self.fail(e.into_aibo()).await; return; }
                    }
                }

                received = inbound.recv() => {
                    match received {
                        Ok(message) => {
                            if self.handle(&message, &cancel).await == Flow::Stop {
                                if self.tx.is_closed() {
                                    self.interrupt().await;
                                }
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Events were dropped. A silent gap in an agent run
                            // is worse than a loud one — say so rather than
                            // pretending the run is intact.
                            tracing::error!(dropped = n, "app-server events dropped: subscriber lagged");
                            self.interrupt().await;
                            self.fail(CodexError::Malformed {
                                method: method::THREAD_EVENT,
                                detail: "the client lagged and lost one or more events".to_owned(),
                            }.into_aibo()).await;
                            return;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            self.fail(CodexError::TransportClosed.into_aibo()).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Dispatch one inbound message. Returns whether the run continues.
    async fn handle(&mut self, message: &Inbound, cancel: &CancellationToken) -> Flow {
        match message {
            Inbound::Notification { method: m, params } => {
                self.handle_notification(m, params).await
            }
            Inbound::Request {
                id,
                method: m,
                params,
            } => {
                self.handle_server_request(id.clone(), m, params, cancel)
                    .await
            }
            // Responses never reach here — the reader routes them.
            Inbound::Response { .. } => Flow::Continue,
        }
    }

    async fn handle_notification(&mut self, method_name: &str, params: &Value) -> Flow {
        match method_name {
            method::ACCOUNT_RATE_LIMITS_UPDATED => {
                // §3: sparse rolling update — merge, never replace.
                if let Ok(update) = serde_json::from_value::<RateLimitSnapshot>(params.clone()) {
                    self.rate_limits
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .merge(update);
                }
                Flow::Continue
            }
            INTERNAL_TRANSPORT_CLOSED => {
                if let Err(error) = self.conn.shutdown(self.shutdown_timeout).await {
                    tracing::warn!(%error, "could not reap codex after transport closure");
                }
                self.fail(CodexError::TransportClosed.into_aibo()).await;
                Flow::Stop
            }
            method::THREAD_EVENT => {
                let Ok(event) = serde_json::from_value::<ThreadEvent>(params.clone()) else {
                    tracing::warn!("undecodable thread event dropped");
                    return Flow::Continue;
                };
                if !self.is_ours(event.thread_id.as_deref()) {
                    return Flow::Continue;
                }
                self.handle_thread_event(event).await
            }
            other => {
                tracing::trace!(method = other, "unhandled app-server notification");
                Flow::Continue
            }
        }
    }

    async fn handle_thread_event(&mut self, event: ThreadEvent) -> Flow {
        if let Some(usage) = event.usage
            && let Err(kind) = self.tracker.record_usage(usage.to_usage())
        {
            self.interrupt().await;
            self.budget_stop(kind).await;
            return Flow::Stop;
        }

        if let Some(item) = &event.item {
            let started = event
                .kind
                .as_deref()
                .is_some_and(|k| k.ends_with(".started"));
            if let Some(step) = map_item(item, started) {
                let is_tool = matches!(step, AgentStep::ToolUse { .. });
                if is_tool {
                    if let Err(kind) = self.tracker.record_tool_call() {
                        self.interrupt().await;
                        self.budget_stop(kind).await;
                        return Flow::Stop;
                    }
                } else if let Err(kind) = self.tracker.record_step() {
                    self.interrupt().await;
                    self.budget_stop(kind).await;
                    return Flow::Stop;
                }
                if !self.emit(step).await {
                    return Flow::Stop;
                }
            }
        }

        if event.is_turn_end() {
            let status = if event.is_turn_aborted() {
                AgentStatus::Cancelled
            } else if event.is_turn_failure() {
                AgentStatus::Failed(
                    event
                        .error
                        .unwrap_or_else(|| "the agent run failed".to_owned()),
                )
            } else {
                AgentStatus::Completed
            };
            self.finish(status).await;
            return Flow::Stop;
        }

        Flow::Continue
    }

    /// Codex asking aibo something. Approvals are the important case (§11).
    async fn handle_server_request(
        &mut self,
        id: RequestId,
        method_name: &str,
        params: &Value,
        cancel: &CancellationToken,
    ) -> Flow {
        let reply_id = id.to_string();
        let request: ApprovalRequest = match method_name {
            method::EXEC_COMMAND_APPROVAL => {
                let parsed: ExecApprovalParams =
                    serde_json::from_value(params.clone()).unwrap_or_default();
                if !self.is_ours(parsed.thread_id.as_deref()) {
                    return Flow::Continue;
                }
                parsed.to_request(reply_id, self.instruction.clone())
            }
            method::APPLY_PATCH_APPROVAL => {
                let parsed: PatchApprovalParams =
                    serde_json::from_value(params.clone()).unwrap_or_default();
                if !self.is_ours(parsed.thread_id.as_deref()) {
                    return Flow::Continue;
                }
                parsed.to_request(reply_id, self.instruction.clone())
            }
            method::ATTESTATION_GENERATE => {
                // §3a: app-server does not generate `x-oai-attestation`; it asks
                // the connected client for one, and that implementation lives in
                // OpenAI's own VS Code extension, not in the OSS tree. aibo
                // cannot produce one — answer with an error immediately rather
                // than letting the child block forever waiting.
                let _ = self
                    .conn
                    .respond(
                        id,
                        None,
                        Some(RpcError {
                            code: -32601,
                            message: "attestation generation is not supported by this client"
                                .to_owned(),
                            data: None,
                        }),
                    )
                    .await;
                return Flow::Continue;
            }
            other => {
                tracing::warn!(method = other, "unanswerable app-server request");
                let _ = self
                    .conn
                    .respond(
                        id,
                        None,
                        Some(RpcError {
                            code: -32601,
                            message: "method not supported by this client".to_owned(),
                            data: None,
                        }),
                    )
                    .await;
                return Flow::Continue;
            }
        };

        // §11: surface Codex's approval in aibo's UI rather than building a
        // parallel permission model. The step goes out *before* the decision is
        // awaited, so the panel can render the prompt.
        if !self
            .emit(AgentStep::AwaitingApproval(request.clone()))
            .await
        {
            return Flow::Stop;
        }

        // §13: `esc` must abort in-flight work. A pending approval that is
        // cancelled resolves to Deny — the safe answer, and the only one that
        // does not leave the child blocked.
        let approval_started = std::time::Instant::now();
        let (response, was_cancelled, receiver_closed) = tokio::select! {
            () = cancel.cancelled() => (ApprovalResponse::deny(), true, false),
            () = self.tx.closed() => (ApprovalResponse::deny(), false, true),
            answered = self.approvals.request(request.clone()) => match answered {
                Ok(d) => (d, false, false),
                Err(e) => {
                    tracing::warn!(error = %e, "approval UI failed; denying");
                    (ApprovalResponse::deny(), false, false)
                }
            },
        };
        self.tracker.credit_wait(approval_started.elapsed());
        let decision = match verify_approval_response(&request, response) {
            Ok(decision) => decision,
            Err(reason) => {
                tracing::warn!(%reason, "invalid destructive approval; denying");
                ApprovalDecision::Deny
            }
        };

        let response_timeout = if was_cancelled || receiver_closed {
            INTERRUPT_TIMEOUT
        } else {
            self.request_timeout
        };
        let response = self.conn.respond_with_timeout(
            id,
            Some(protocol::approval_reply(decision)),
            None,
            response_timeout,
        );
        let response_result = tokio::select! {
            biased;
            () = self.tx.closed(), if !receiver_closed => {
                self.interrupt().await;
                return Flow::Stop;
            }
            result = response => result,
        };
        if receiver_closed {
            self.interrupt().await;
            return Flow::Stop;
        }
        if let Err(e) = response_result {
            self.fail(e.into_aibo()).await;
            return Flow::Stop;
        }
        Flow::Continue
    }

    /// Events on one connection may belong to another concurrent run.
    ///
    /// SPIKE: S5 — whether every event and approval carries a thread id is
    /// unverified. An event with no id is accepted, which is right for the
    /// single-run case and wrong for concurrent runs; confirm before allowing
    /// two Do runs to share a connection.
    fn is_ours(&self, thread_id: Option<&str>) -> bool {
        thread_id.is_none_or(|id| id == self.thread_id)
    }

    async fn interrupt(&self) {
        let params = serde_json::to_value(InterruptParams {
            thread_id: self.thread_id.clone(),
        })
        .unwrap_or(Value::Null);
        if let Err(e) = self
            .conn
            .notify_with_timeout(method::THREAD_INTERRUPT, Some(params), INTERRUPT_TIMEOUT)
            .await
        {
            tracing::warn!(error = %e, "could not interrupt the codex thread");
        }
    }

    async fn emit(&self, step: AgentStep) -> bool {
        self.tx.send(Ok(step)).await.is_ok()
    }

    async fn finish(&self, status: AgentStatus) {
        let outcome = self.tracker.outcome(status);
        let _ = self.tx.send(Ok(AgentStep::Done(outcome))).await;
    }

    async fn budget_stop(&self, kind: BudgetKind) {
        let outcome = self.tracker.budget_outcome(kind);
        let _ = self.tx.send(Err(crate::limits::budget_error(kind))).await;
        let _ = self.tx.send(Ok(AgentStep::Done(outcome))).await;
    }

    async fn fail(&self, error: AiboError) {
        let status = AgentStatus::Failed(error.to_string());
        let _ = self.tx.send(Err(error)).await;
        let outcome = self.tracker.outcome(status);
        let _ = self.tx.send(Ok(AgentStep::Done(outcome))).await;
    }
}

/// Whether the turn continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Map one app-server thread item onto an [`AgentStep`] (§7).
///
/// `started` distinguishes `item.started` from `item.completed`: a tool use is
/// reported when it *starts* (that is when the side effect happens), while text
/// and diffs are reported when they complete, so a partial message is never
/// shown as final.
///
/// SPIKE: S5 — the item kind strings are unverified. Matching is `contains`-based
/// on purpose, so a rename from `agent_message` to `assistant_message` degrades
/// rather than breaking.
fn map_item(item: &ThreadItem, started: bool) -> Option<AgentStep> {
    let kind = item.kind.as_deref().unwrap_or_default();

    if kind.contains("reason") || kind.contains("thinking") {
        return (!started).then(|| AgentStep::Thought(item.text.clone().unwrap_or_default()));
    }

    if kind.contains("command") || kind.contains("exec") || kind.contains("shell") {
        return started.then(|| AgentStep::ToolUse {
            id: item.id.clone().unwrap_or_default(),
            name: "shell".to_owned(),
            args: item.command.clone().unwrap_or(Value::Null),
            tier: ToolTier::Delegate,
        });
    }

    if kind.contains("mcp") {
        return started.then(|| AgentStep::ToolUse {
            id: item.id.clone().unwrap_or_default(),
            name: kind.to_owned(),
            args: Value::Object(item.extra.clone()),
            tier: ToolTier::Delegate,
        });
    }

    if kind.contains("patch") || kind.contains("file_change") || kind.contains("fileChange") {
        if started {
            return None;
        }
        // §11: this is "revert these file changes", never an undo for the whole
        // operation — it cannot reverse processes started or network calls made.
        return Some(AgentStep::FileDiff {
            path: PathBuf::from(item.path.clone().unwrap_or_default()),
            unified_diff: item.unified_diff.clone().unwrap_or_default(),
        });
    }

    if kind.contains("message") || kind.is_empty() {
        let text = item.text.clone()?;
        return (!started).then_some(AgentStep::Message(text));
    }

    tracing::trace!(kind, "unmapped app-server thread item");
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::types::{ContentOrigin, UntrustedBlock};
    #[cfg(unix)]
    use futures::StreamExt;
    use uuid::Uuid;

    #[cfg(unix)]
    fn process_is_alive(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }

    fn item(kind: &str, text: Option<&str>) -> ThreadItem {
        ThreadItem {
            id: Some("i1".into()),
            kind: Some(kind.into()),
            text: text.map(ToOwned::to_owned),
            ..ThreadItem::default()
        }
    }

    #[test]
    fn reasoning_is_a_thought_not_a_message() {
        // §7: reasoning is a separate channel — render collapsed, never insert.
        let step = map_item(&item("reasoning", Some("hmm")), false).unwrap();
        assert_eq!(step, AgentStep::Thought("hmm".into()));
    }

    #[test]
    fn tool_use_is_reported_when_it_starts() {
        assert!(map_item(&item("command_execution", None), true).is_some());
        assert!(map_item(&item("command_execution", None), false).is_none());
    }

    #[test]
    fn messages_are_reported_when_they_complete() {
        assert!(map_item(&item("agent_message", Some("done")), true).is_none());
        assert_eq!(
            map_item(&item("agent_message", Some("done")), false),
            Some(AgentStep::Message("done".into()))
        );
    }

    #[test]
    fn seed_fences_captured_context() {
        // §3b: a new thread seeded with a replayable plain-text summary; §5:
        // the captured half is fenced as data, not instructions.
        let task = AgentTask {
            id: Uuid::now_v7(),
            instruction: "fix the failing test".into(),
            workspace: None,
            context: vec![UntrustedBlock {
                origin: ContentOrigin::Selection,
                label: "selection from Terminal".into(),
                content: "assertion failed".into(),
                truncated: false,
            }],
            binding: None,
            conversation_id: None,
        };
        let seed = seed_message(&task);
        assert!(seed.starts_with("fix the failing test"));
        assert!(seed.contains("data, not instructions"));
    }

    #[test]
    fn missing_binary_becomes_agent_backend_missing() {
        let err = CodexError::NotInstalled {
            program: "codex".into(),
        }
        .into_aibo();
        assert!(matches!(
            err,
            AiboError::AgentBackendMissing { which: "codex" }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initialization_timeout_terminates_the_app_server_tree() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pid_file = temp.path().join("grandchild.pid");
        let config = CodexConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                "sleep 120 & echo $! > \"$PID_FILE\"; wait".to_owned(),
            ],
            env: vec![("PID_FILE".to_owned(), pid_file.display().to_string())],
            startup_timeout: Duration::from_millis(100),
            shutdown_timeout: Duration::from_secs(2),
            ..CodexConfig::default()
        };
        let backend = CodexAppServer::new(config, Arc::new(crate::permission_gate::DenyAll));

        let started = std::time::Instant::now();
        assert!(backend.account().await.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));

        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("grandchild pid")
            .trim()
            .parse()
            .expect("numeric pid");
        for _ in 0..100 {
            if !process_is_alive(pid) {
                backend.shutdown().await;
                backend.shutdown().await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("grandchild {pid} survived Codex initialization timeout");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ordinary_rpc_timeout_covers_the_entire_request() {
        let config = CodexConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                concat!(
                    "IFS= read -r initialize\n",
                    "printf '%s\\n' '{\"id\":1,\"result\":",
                    "{\"protocolVersion\":\"0.63.0\"}}'\n",
                    "IFS= read -r initialized\n",
                    "IFS= read -r account\n",
                    "sleep 120"
                )
                .to_owned(),
            ],
            startup_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_millis(100),
            shutdown_timeout: Duration::from_secs(2),
            ..CodexConfig::default()
        };
        let backend = CodexAppServer::new(config, Arc::new(crate::permission_gate::DenyAll));

        let started = std::time::Instant::now();
        assert!(backend.account().await.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        backend.shutdown().await;
        // The second call exercises both the outer connection slot and the
        // inner process slot's idempotent shutdown path.
        tokio::time::timeout(Duration::from_millis(100), backend.shutdown())
            .await
            .expect("second shutdown must return immediately");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_eof_after_send_ack_fails_the_turn_promptly() {
        let config = CodexConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                concat!(
                    "IFS= read -r initialize\n",
                    "printf '%s\\n' '{\"id\":1,\"result\":",
                    "{\"protocolVersion\":\"0.63.0\"}}'\n",
                    "IFS= read -r initialized\n",
                    "IFS= read -r thread_start\n",
                    "printf '%s\\n' '{\"id\":2,\"result\":{\"threadId\":\"t1\"}}'\n",
                    "IFS= read -r send_message\n",
                    "printf '%s\\n' '{\"id\":3,\"result\":{}}'\n",
                    "exit 0"
                )
                .to_owned(),
            ],
            startup_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(2),
            ..CodexConfig::default()
        };
        let backend = CodexAppServer::new(config, Arc::new(crate::permission_gate::DenyAll));
        let task = AgentTask {
            id: Uuid::now_v7(),
            instruction: "run".to_owned(),
            workspace: None,
            context: Vec::new(),
            binding: None,
            conversation_id: None,
        };
        let mut stream = backend
            .run(task, AgentLimits::default(), CancellationToken::new())
            .await
            .expect("start turn");
        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("EOF must wake the turn")
            .expect("error item");
        assert!(first.is_err(), "expected transport error, got {first:?}");
        let done = stream.next().await.unwrap().unwrap();
        assert!(matches!(done, AgentStep::Done(_)));
        assert!(
            backend
                .connection
                .lock()
                .await
                .as_ref()
                .is_some_and(|connection| connection.is_closed())
        );
        backend.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_turn_stream_sends_an_interrupt() {
        let temp = tempfile::tempdir().expect("temp dir");
        let marker = temp.path().join("interrupted");
        let config = CodexConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                concat!(
                    "IFS= read -r initialize\n",
                    "printf '%s\\n' '{\"id\":1,\"result\":",
                    "{\"protocolVersion\":\"0.63.0\"}}'\n",
                    "IFS= read -r initialized\n",
                    "IFS= read -r thread_start\n",
                    "printf '%s\\n' '{\"id\":2,\"result\":{\"threadId\":\"t1\"}}'\n",
                    "while IFS= read -r line; do\n",
                    "  case \"$line\" in *thread/interrupt*) echo yes > \"$MARKER\"; exit 0;; esac\n",
                    "done"
                )
                .to_owned(),
            ],
            env: vec![("MARKER".to_owned(), marker.display().to_string())],
            startup_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(2),
            ..CodexConfig::default()
        };
        let backend = CodexAppServer::new(config, Arc::new(crate::permission_gate::DenyAll));
        let task = AgentTask {
            id: Uuid::now_v7(),
            instruction: "run".to_owned(),
            workspace: None,
            context: Vec::new(),
            binding: None,
            conversation_id: None,
        };
        let stream = backend
            .run(task, AgentLimits::default(), CancellationToken::new())
            .await
            .expect("start turn");
        drop(stream);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the receiver should interrupt Codex");
        backend.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_before_start_does_not_spawn_codex() {
        let config = CodexConfig {
            program: PathBuf::from("/nonexistent/aibo/codex"),
            ..CodexConfig::default()
        };
        let backend = CodexAppServer::new(config, Arc::new(crate::permission_gate::DenyAll));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let task = AgentTask {
            id: Uuid::now_v7(),
            instruction: "run".to_owned(),
            workspace: None,
            context: Vec::new(),
            binding: None,
            conversation_id: None,
        };
        let mut stream = backend
            .run(task, AgentLimits::default(), cancel)
            .await
            .expect("pre-cancelled runs return a terminal stream");
        let step = stream.next().await.unwrap().unwrap();
        assert!(matches!(
            step,
            AgentStep::Done(ref outcome) if outcome.status == AgentStatus::Cancelled
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn turn_cancellation_terminates_the_delegated_process_tree() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pid_file = temp.path().join("grandchild.pid");
        let config = CodexConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                format!(
                    concat!(
                        "IFS= read -r initialize\n",
                        "printf '%s\\n' '{{\"id\":1,\"result\":",
                        "{{\"protocolVersion\":\"0.63.0\"}}}}'\n",
                        "IFS= read -r initialized\n",
                        "IFS= read -r thread_start\n",
                        "printf '%s\\n' '{{\"id\":2,\"result\":",
                        "{{\"threadId\":\"t1\"}}}}'\n",
                        "IFS= read -r send_message\n",
                        "sleep 120 & echo $! > '{}'\n",
                        "wait"
                    ),
                    pid_file.display()
                ),
            ],
            startup_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(2),
            ..CodexConfig::default()
        };
        let backend = CodexAppServer::new(config, Arc::new(crate::permission_gate::DenyAll));
        let cancel = CancellationToken::new();
        let task = AgentTask {
            id: Uuid::now_v7(),
            instruction: "run".to_owned(),
            workspace: None,
            context: Vec::new(),
            binding: None,
            conversation_id: None,
        };
        let mut stream = backend
            .run(task, AgentLimits::default(), cancel.clone())
            .await
            .expect("start turn");
        for _ in 0..500 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(pid_file.exists(), "fake Codex never spawned its child");

        cancel.cancel();
        let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("cancellation must be bounded")
            .expect("terminal item")
            .expect("terminal step");
        let AgentStep::Done(outcome) = item else {
            panic!("expected Done after cancellation, got {item:?}");
        };
        assert_eq!(outcome.status, AgentStatus::Cancelled);

        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("grandchild pid")
            .trim()
            .parse()
            .expect("numeric pid");
        for _ in 0..100 {
            if !process_is_alive(pid) {
                backend.shutdown().await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("grandchild {pid} survived Codex turn cancellation");
    }
}
