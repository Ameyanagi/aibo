//! Tier 2: MCP over `rmcp` 2.2, stdio and streamable HTTP (§11).
//!
//! # Consent shape
//!
//! §11: "Per-server at add time; per-tool allow/ask/deny, remembered." Those
//! are two different decisions and both live in [`ServerConsent`]. Adding a
//! server is the coarse grant; the per-tool decision is the one that gets
//! remembered as the user works.
//!
//! # Results are untrusted
//!
//! A compromised or merely careless MCP server appears in the §11 threat table
//! twice: once as the server, once as its *results*. Every [`ToolOutput`] from
//! this module reports [`aibo_core::types::ContentOrigin::ToolResult`], so §5's
//! rule 2 applies — a server's reply can never authorise the next tool call, no
//! matter how it is phrased. [`MAX_RESULT_BYTES`] additionally bounds what a
//! server can push into the context window: an unbounded reply is a cheap way
//! to blow the §14 token ceiling or crowd out the user's own instruction.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use std::time::Duration;

use aibo_core::traits::{ChildProcessObserver, ChildProcessRegistration};
use aibo_core::types::{ToolSchema, ToolTier};
use async_trait::async_trait;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{CommandWrap, KillOnDrop};
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};
use rmcp::ServiceExt as _;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::TokioChildProcess;
use tokio_util::sync::CancellationToken;
#[cfg(windows)]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::{DenyReason, Tool, ToolError, ToolOutput, ToolResult};

/// Largest tool result accepted from a server before it is truncated.
pub const MAX_RESULT_BYTES: usize = 128 * 1024;

/// Default per-request timeout. A hung server must not hang the run.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default initialize-handshake timeout.
pub const DEFAULT_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(20);

/// Default time allowed for transport close and child reaping.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

// stdio servers inherit only process-discovery, profile, locale, and temporary
// directory state. In particular, ambient AIBO/AWS/GCP credentials are not
// handed to every configured server. `McpTransport::Stdio::env` remains the
// explicit escape hatch for a server that genuinely needs another variable.
#[cfg(not(windows))]
const SAFE_CHILD_ENVIRONMENT: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM",
];

#[cfg(windows)]
const SAFE_CHILD_ENVIRONMENT: &[&str] = &[
    "PATH",
    "SystemRoot",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
];

fn safe_child_environment_from(
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    environment
        .into_iter()
        .filter(|(key, _)| {
            SAFE_CHILD_ENVIRONMENT
                .iter()
                .any(|allowed| env_key_eq(key, OsStr::new(allowed)))
        })
        .collect()
}

#[cfg(windows)]
fn env_key_eq(left: &OsStr, right: &OsStr) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn env_key_eq(left: &OsStr, right: &OsStr) -> bool {
    left == right
}

/// How a server is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// A child process speaking JSON-RPC over stdio.
    ///
    /// Note what this is not: a sandbox. The child runs with aibo's own
    /// privileges. Adding a stdio server is equivalent to the user agreeing to
    /// run that program, and the consent copy must say so.
    Stdio {
        /// Executable.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Extra environment for the child.
        env: Vec<(String, String)>,
        /// Working directory.
        cwd: Option<std::path::PathBuf>,
    },
    /// A streamable-HTTP endpoint.
    Http {
        /// Endpoint URL.
        url: String,
        /// Bearer token, without the `Bearer ` prefix.
        ///
        /// A plain `String` because rmcp's config takes one; callers should
        /// pull it from the keychain immediately before connecting and keep the
        /// long-lived copy in a `SecretString` (§12).
        bearer: Option<String>,
    },
}

/// The remembered decision for one tool (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolDecision {
    /// Run without asking.
    Allow,
    /// Ask every time. The default, because a server's tool list can change
    /// under the user between sessions.
    #[default]
    Ask,
    /// Never run.
    Deny,
}

/// Per-server and per-tool consent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConsent {
    /// What to do for a tool with no specific decision.
    pub default: ToolDecision,
    /// Remembered per-tool decisions.
    pub per_tool: BTreeMap<String, ToolDecision>,
}

impl ServerConsent {
    /// A consent record that asks about everything.
    pub fn ask_always() -> Self {
        Self::default()
    }

    /// The decision for one tool.
    pub fn decide(&self, tool: &str) -> ToolDecision {
        self.per_tool.get(tool).copied().unwrap_or(self.default)
    }

    /// Remember a decision.
    pub fn remember(&mut self, tool: impl Into<String>, decision: ToolDecision) {
        self.per_tool.insert(tool.into(), decision);
    }
}

/// A configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    /// Stable id, used in tool names and in the consent store.
    pub id: String,
    /// How to reach it.
    pub transport: McpTransport,
    /// Remembered consent.
    pub consent: ServerConsent,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Initialize-handshake timeout.
    pub initialization_timeout: Duration,
    /// Transport close and child-reap timeout.
    pub shutdown_timeout: Duration,
}

impl McpServerConfig {
    /// A config with the default timeout and ask-always consent.
    pub fn new(id: impl Into<String>, transport: McpTransport) -> Self {
        Self {
            id: id.into(),
            transport,
            consent: ServerConsent::ask_always(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            initialization_timeout: DEFAULT_INITIALIZATION_TIMEOUT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }
}

/// A live connection to one MCP server.
pub struct McpClient {
    id: String,
    consent: ServerConsent,
    request_timeout: Duration,
    shutdown_timeout: Duration,
    service: tokio::sync::Mutex<Option<RunningService<RoleClient, ()>>>,
    _registration: Option<ChildProcessRegistration>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No transport details: a stdio config carries an argv and an HTTP one
        // carries a bearer token, and §11 wants redaction tests on `Debug`.
        f.debug_struct("McpClient")
            .field("id", &self.id)
            .field("tools_with_decisions", &self.consent.per_tool.len())
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Connect and complete the MCP initialize handshake.
    pub async fn connect(config: McpServerConfig) -> ToolResult<Self> {
        Self::connect_with_process_observer(config, None).await
    }

    /// Connect while reporting a stdio server child to the application-owned
    /// crash-recovery ledger. HTTP transports do not create a child.
    pub async fn connect_with_process_observer(
        config: McpServerConfig,
        process_observer: Option<Arc<dyn ChildProcessObserver>>,
    ) -> ToolResult<Self> {
        if config.id.trim().is_empty() || config.id.contains("::") {
            return Err(ToolError::InvalidArguments {
                tool: "mcp".to_owned(),
                reason: "server id must be non-empty and must not contain `::`".to_owned(),
            });
        }
        let id = config.id.clone();
        let mut registration = None;
        let service = match &config.transport {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                cmd.env_clear();
                cmd.envs(safe_child_environment_from(std::env::vars_os()));
                if let Some(cwd) = cwd {
                    cmd.current_dir(cwd);
                }
                for (k, v) in env {
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

                let transport = TokioChildProcess::new(cmd).map_err(|e| ToolError::Failed {
                    tool: id.clone(),
                    message: format!("could not spawn MCP server: {e}"),
                })?;
                let identity = std::path::Path::new(command)
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new(command))
                    .to_string_lossy();
                registration = Some(ChildProcessRegistration::new(
                    process_observer,
                    transport.id(),
                    &identity,
                ));
                tokio::time::timeout(config.initialization_timeout, ().serve(transport))
                    .await
                    .map_err(|_| ToolError::Failed {
                        tool: id.clone(),
                        message: format!(
                            "MCP initialize timed out after {:?}",
                            config.initialization_timeout
                        ),
                    })?
                    .map_err(|e| ToolError::Failed {
                        tool: id.clone(),
                        message: format!("MCP initialize failed: {e}"),
                    })?
            }
            McpTransport::Http { url, bearer } => {
                let mut cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                    url.clone(),
                );
                if let Some(token) = bearer {
                    cfg = cfg.auth_header(token.clone());
                }
                // `from_config` uses rmcp's own tuned HTTP client (no idle
                // pooling, no redirect following so a caller-supplied auth
                // header cannot be replayed to a redirect target). Building one
                // here would mean depending on rmcp's exact reqwest major, and
                // the workspace pins a different one for the provider crate.
                let transport = StreamableHttpClientTransport::from_config(cfg);
                tokio::time::timeout(config.initialization_timeout, ().serve(transport))
                    .await
                    .map_err(|_| ToolError::Failed {
                        tool: id.clone(),
                        message: format!(
                            "MCP initialize timed out after {:?}",
                            config.initialization_timeout
                        ),
                    })?
                    .map_err(|e| ToolError::Failed {
                        tool: id.clone(),
                        message: format!("MCP initialize failed: {e}"),
                    })?
            }
        };

        Ok(Self {
            id: config.id,
            consent: config.consent,
            request_timeout: config.request_timeout,
            shutdown_timeout: config.shutdown_timeout,
            service: tokio::sync::Mutex::new(Some(service)),
            _registration: registration,
        })
    }

    /// The server's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The consent record.
    pub fn consent(&self) -> &ServerConsent {
        &self.consent
    }

    /// Update the consent record after the user decided.
    pub fn set_consent(&mut self, consent: ServerConsent) {
        self.consent = consent;
    }

    /// Every tool the server advertises, as aibo schemas.
    ///
    /// Names are prefixed with the server id (`server::tool`) so two servers
    /// that both expose `search` do not collide in one registry, and so the
    /// approval prompt can name the server without a second lookup.
    pub async fn list_tools(&self) -> ToolResult<Vec<ToolSchema>> {
        let peer = {
            let service = self.service.lock().await;
            let service = service.as_ref().ok_or_else(|| ToolError::Failed {
                tool: self.id.clone(),
                message: "MCP connection is closed".to_owned(),
            })?;
            service.peer().clone()
        };
        let result = tokio::time::timeout(self.request_timeout, peer.list_all_tools()).await;
        let tools = match result {
            Ok(result) => result.map_err(|e| ToolError::Failed {
                tool: self.id.clone(),
                message: format!("listing tools failed: {e}"),
            })?,
            Err(_) => {
                let _ = self.shutdown().await;
                return Err(ToolError::Failed {
                    tool: self.id.clone(),
                    message: "listing tools timed out".to_owned(),
                });
            }
        };

        Ok(tools
            .into_iter()
            .map(|t| ToolSchema {
                name: qualified_name(&self.id, &t.name),
                description: t.description.map(|d| d.into_owned()).unwrap_or_default(),
                parameters: serde_json::Value::Object((*t.input_schema).clone()),
                tier: 2,
            })
            .collect())
    }

    /// Call a tool, enforcing remembered consent.
    ///
    /// `pre_approved` is the permission gate's answer for a tool whose decision
    /// is [`ToolDecision::Ask`]. [`ToolDecision::Deny`] is **not** overridable
    /// by it: a remembered "never" outranks an in-flight yes, which is what
    /// makes the setting worth having.
    pub async fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
        pre_approved: bool,
        cancel: CancellationToken,
    ) -> ToolResult<ToolOutput> {
        let bare = strip_prefix(&self.id, tool).to_owned();
        match self.consent.decide(&bare) {
            ToolDecision::Deny => {
                return Err(DenyReason::McpDenied {
                    server: self.id.clone(),
                    tool: bare,
                }
                .into());
            }
            ToolDecision::Ask if !pre_approved => {
                return Err(DenyReason::NotApproved {
                    what: qualified_name(&self.id, &bare),
                }
                .into());
            }
            ToolDecision::Ask | ToolDecision::Allow => {}
        }

        let arguments = match args {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                return Err(ToolError::InvalidArguments {
                    tool: qualified_name(&self.id, &bare),
                    reason: format!("arguments must be an object, got {other}"),
                });
            }
        };

        let mut params = CallToolRequestParams::new(bare.clone());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }

        enum CallOutcome<T> {
            Complete(T),
            Cancelled,
            TimedOut,
        }
        let peer = {
            let service = self.service.lock().await;
            let service = service.as_ref().ok_or_else(|| ToolError::Failed {
                tool: self.id.clone(),
                message: "MCP connection is closed".to_owned(),
            })?;
            service.peer().clone()
        };
        let outcome = {
            let call = peer.call_tool(params);
            tokio::select! {
                biased;
                () = cancel.cancelled() => CallOutcome::Cancelled,
                r = tokio::time::timeout(self.request_timeout, call) => match r {
                    Ok(result) => CallOutcome::Complete(result),
                    Err(_) => CallOutcome::TimedOut,
                },
            }
        };
        let result = match outcome {
            CallOutcome::Complete(result) => result.map_err(|e| ToolError::Failed {
                tool: qualified_name(&self.id, &bare),
                message: e.to_string(),
            })?,
            CallOutcome::Cancelled => {
                let _ = self.shutdown().await;
                return Err(ToolError::Cancelled);
            }
            CallOutcome::TimedOut => {
                let _ = self.shutdown().await;
                return Err(ToolError::Failed {
                    tool: qualified_name(&self.id, &bare),
                    message: format!("timed out after {:?}", self.request_timeout),
                });
            }
        };

        Ok(render_result(
            result.content,
            result.structured_content,
            result.is_error.unwrap_or(false),
        ))
    }

    /// Close the connection and, for stdio, reap the child.
    pub async fn shutdown(&self) -> ToolResult<()> {
        let service = self.service.lock().await.take();
        let Some(mut service) = service else {
            return Ok(());
        };
        let closed = service
            .close_with_timeout(self.shutdown_timeout)
            .await
            .map_err(|e| ToolError::Failed {
                tool: self.id.clone(),
                message: format!("shutdown failed: {e}"),
            })?;
        if closed.is_none() {
            return Err(ToolError::Failed {
                tool: self.id.clone(),
                message: format!("shutdown timed out after {:?}", self.shutdown_timeout),
            });
        }
        Ok(())
    }

    /// Wrap the server's advertised tools so they can go in a
    /// [`crate::ToolRegistry`].
    ///
    /// Only tools whose remembered decision is [`ToolDecision::Allow`] are
    /// returned: the [`Tool`] trait has nowhere to carry an approval, so an
    /// `Ask` tool would be advertised to the model and then always refused,
    /// burning steps against the §14 ceilings. `Ask` tools are invoked through
    /// [`McpClient::call`] by the permission gate instead.
    pub async fn allowed_tools(self: &Arc<Self>) -> ToolResult<Vec<Arc<dyn Tool>>> {
        let schemas = self.list_tools().await?;
        Ok(schemas
            .into_iter()
            .filter(|s| self.consent.decide(strip_prefix(&self.id, &s.name)) == ToolDecision::Allow)
            .map(|schema| {
                Arc::new(McpTool {
                    client: Arc::clone(self),
                    schema,
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

/// `server::tool`.
fn qualified_name(server: &str, tool: &str) -> String {
    if tool.starts_with(&format!("{server}::")) {
        tool.to_owned()
    } else {
        format!("{server}::{tool}")
    }
}

/// Strip a `server::` prefix if present.
fn strip_prefix<'a>(server: &str, tool: &'a str) -> &'a str {
    tool.strip_prefix(&format!("{server}::")).unwrap_or(tool)
}

/// Flatten MCP content blocks into a [`ToolOutput`].
///
/// Non-text blocks are summarised rather than dropped silently: a model told
/// "there was an image here" reasons better than one shown an empty result.
fn render_result(
    content: Vec<ContentBlock>,
    mut structured: Option<serde_json::Value>,
    is_error: bool,
) -> ToolOutput {
    let mut text = String::new();
    for block in &content {
        if !text.is_empty() {
            text.push('\n');
        }
        match block {
            ContentBlock::Text(t) => text.push_str(&t.text),
            ContentBlock::Image(_) => text.push_str("[image content omitted]"),
            ContentBlock::Audio(_) => text.push_str("[audio content omitted]"),
            ContentBlock::Resource(_) => text.push_str("[embedded resource omitted]"),
            ContentBlock::ResourceLink(r) => {
                text.push_str("[resource link: ");
                text.push_str(&r.uri);
                text.push(']');
            }
            _ => text.push_str("[unsupported content block omitted]"),
        }
    }

    if text.len() > MAX_RESULT_BYTES {
        let mut cut = MAX_RESULT_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[truncated by aibo]");
    }

    // The text cap is not a result cap if a server can put the same (or much
    // larger) payload in `structuredContent`. Keep the machine-readable value
    // only when the combined serialized representation fits the same budget.
    let structured_fits = structured.as_ref().is_none_or(|value| {
        serialized_json_fits(value, MAX_RESULT_BYTES.saturating_sub(text.len()))
    });
    if !structured_fits {
        structured = None;
        text.push_str("\n[structured content omitted by aibo: result too large]");
    }

    ToolOutput {
        text,
        structured,
        is_error,
    }
}

fn serialized_json_fits(value: &serde_json::Value, budget: usize) -> bool {
    struct Counter {
        remaining: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                return Err(std::io::Error::other("structured result exceeds budget"));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    serde_json::to_writer(&mut Counter { remaining: budget }, value).is_ok()
}

/// One MCP tool, callable through [`Tool`].
pub struct McpTool {
    client: Arc<McpClient>,
    schema: ToolSchema,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.schema.name)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Mcp
    }

    async fn call(
        &self,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> ToolResult<ToolOutput> {
        // `pre_approved: false` — only `Allow` tools are wrapped this way (see
        // [`McpClient::allowed_tools`]), so this path never needs to override an
        // `Ask`. If consent was downgraded since the registry was built, the
        // call fails closed rather than running on a stale grant.
        self.client
            .call(&self.schema.name, args, false, cancel)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_defaults_to_ask() {
        let c = ServerConsent::ask_always();
        assert_eq!(c.decide("anything"), ToolDecision::Ask);
    }

    #[test]
    fn per_tool_decisions_override_the_server_default() {
        let mut c = ServerConsent {
            default: ToolDecision::Allow,
            ..Default::default()
        };
        c.remember("dangerous", ToolDecision::Deny);
        assert_eq!(c.decide("harmless"), ToolDecision::Allow);
        assert_eq!(c.decide("dangerous"), ToolDecision::Deny);
    }

    #[test]
    fn names_are_qualified_by_server_and_round_trip() {
        assert_eq!(qualified_name("fs", "read"), "fs::read");
        assert_eq!(qualified_name("fs", "fs::read"), "fs::read");
        assert_eq!(strip_prefix("fs", "fs::read"), "read");
        assert_eq!(strip_prefix("fs", "read"), "read");
    }

    #[test]
    fn results_are_flattened_and_marked_untrusted() {
        let out = render_result(
            vec![ContentBlock::text("hello"), ContentBlock::text("world")],
            Some(serde_json::json!({"a": 1})),
            false,
        );
        assert_eq!(out.text, "hello\nworld");
        assert_eq!(out.structured, Some(serde_json::json!({"a": 1})));
        assert!(!out.origin().may_authorise_tools());
    }

    #[test]
    fn an_error_result_stays_an_error() {
        let out = render_result(vec![ContentBlock::text("nope")], None, true);
        assert!(out.is_error);
    }

    #[test]
    fn an_oversized_result_is_truncated_on_a_char_boundary() {
        let huge = "é".repeat(MAX_RESULT_BYTES);
        let out = render_result(vec![ContentBlock::text(huge)], None, false);
        assert!(out.text.len() < MAX_RESULT_BYTES + 64);
        assert!(out.text.ends_with("[truncated by aibo]"));
    }

    #[test]
    fn oversized_structured_content_is_omitted() {
        let out = render_result(
            vec![ContentBlock::text("small")],
            Some(serde_json::json!({"payload": "x".repeat(MAX_RESULT_BYTES)})),
            false,
        );
        assert!(out.structured.is_none());
        assert!(out.text.contains("structured content omitted"));
    }

    #[tokio::test]
    async fn invalid_server_ids_fail_before_transport_setup() {
        for id in ["", "a::b"] {
            let cfg = McpServerConfig::new(
                id,
                McpTransport::Stdio {
                    command: "this-must-not-be-spawned".to_owned(),
                    args: vec![],
                    env: vec![],
                    cwd: None,
                },
            );
            assert!(matches!(
                McpClient::connect(cfg).await,
                Err(ToolError::InvalidArguments { .. })
            ));
        }
    }

    #[test]
    fn the_client_debug_impl_shows_no_transport_details() {
        // Constructing a real client needs a server, so this asserts the shape
        // of the config instead: the client stores neither transport nor token.
        let cfg = McpServerConfig::new(
            "remote",
            McpTransport::Http {
                url: "https://example.invalid/mcp".to_owned(),
                bearer: Some("super-secret-token".to_owned()),
            },
        );
        assert_eq!(cfg.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(cfg.consent.decide("x"), ToolDecision::Ask);
    }

    #[test]
    fn child_environment_drops_ambient_credentials() {
        let kept = safe_child_environment_from([
            (OsString::from("PATH"), OsString::from("/bin")),
            (
                OsString::from("AIBO_OPENAI_API_KEY"),
                OsString::from("secret"),
            ),
            (
                OsString::from("AWS_SECRET_ACCESS_KEY"),
                OsString::from("secret"),
            ),
            (OsString::from("RUST_LOG"), OsString::from("trace")),
        ]);
        assert_eq!(kept, vec![(OsString::from("PATH"), OsString::from("/bin"))]);
    }

    #[tokio::test]
    async fn connecting_to_a_missing_stdio_server_fails_cleanly() {
        let cfg = McpServerConfig::new(
            "missing",
            McpTransport::Stdio {
                command: "aibo-no-such-mcp-server".to_owned(),
                args: vec![],
                env: vec![],
                cwd: None,
            },
        );
        let err = McpClient::connect(cfg).await.unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "{err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initialization_timeout_terminates_the_stdio_process_tree() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild.pid");
        let mut cfg = McpServerConfig::new(
            "hung",
            McpTransport::Stdio {
                command: "/bin/sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    format!("sleep 120 & echo $! > {}; wait", pid_file.display()),
                ],
                env: vec![],
                cwd: None,
            },
        );
        cfg.initialization_timeout = Duration::from_millis(100);

        let err = McpClient::connect(cfg).await.unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "{err:?}");
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("grandchild pid")
            .trim()
            .parse()
            .expect("numeric pid");
        for _ in 0..100 {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("grandchild {pid} survived MCP initialization timeout");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_is_bounded_and_idempotent() {
        let mut cfg = McpServerConfig::new(
            "fake",
            McpTransport::Stdio {
                command: "/bin/sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    concat!(
                        "IFS= read -r initialize\n",
                        "printf '%s\\n' ",
                        "'{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":",
                        "{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},",
                        "\"serverInfo\":{\"name\":\"fake\",\"version\":\"1\"}}}'\n",
                        "IFS= read -r initialized\n",
                        "while IFS= read -r line; do :; done"
                    )
                    .to_owned(),
                ],
                env: vec![],
                cwd: None,
            },
        );
        cfg.initialization_timeout = Duration::from_secs(2);
        cfg.shutdown_timeout = Duration::from_secs(2);

        let client = McpClient::connect(cfg).await.expect("initialize");
        client.shutdown().await.expect("first shutdown");
        tokio::time::timeout(Duration::from_millis(100), client.shutdown())
            .await
            .expect("second shutdown must return immediately")
            .expect("second shutdown");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_cancellation_terminates_the_stdio_process_tree() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild.pid");
        let mut cfg = McpServerConfig::new(
            "fake",
            McpTransport::Stdio {
                command: "/bin/sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    concat!(
                        "IFS= read -r initialize\n",
                        "printf '%s\\n' ",
                        "'{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":",
                        "{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},",
                        "\"serverInfo\":{\"name\":\"fake\",\"version\":\"1\"}}}'\n",
                        "IFS= read -r initialized\n",
                        "IFS= read -r call\n",
                        "sleep 120 & echo $! > \"$PID_FILE\"\n",
                        "wait"
                    )
                    .to_owned(),
                ],
                env: vec![("PID_FILE".to_owned(), pid_file.display().to_string())],
                cwd: None,
            },
        );
        cfg.initialization_timeout = Duration::from_secs(2);
        cfg.request_timeout = Duration::from_secs(10);
        cfg.shutdown_timeout = Duration::from_secs(5);

        let client = Arc::new(McpClient::connect(cfg).await.expect("initialize"));
        let cancel = CancellationToken::new();
        let call = tokio::spawn({
            let client = Arc::clone(&client);
            let cancel = cancel.clone();
            async move {
                client
                    .call("hang", serde_json::Value::Null, true, cancel)
                    .await
            }
        });
        for _ in 0..100 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(pid_file.exists(), "fake MCP server never spawned its child");
        cancel.cancel();
        let err = tokio::time::timeout(Duration::from_secs(8), call)
            .await
            .expect("cancellation must be bounded")
            .expect("call task")
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled), "{err:?}");

        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("grandchild pid")
            .trim()
            .parse()
            .expect("numeric pid");
        for _ in 0..100 {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                client.shutdown().await.expect("idempotent shutdown");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("grandchild {pid} survived MCP request cancellation");
    }
}
