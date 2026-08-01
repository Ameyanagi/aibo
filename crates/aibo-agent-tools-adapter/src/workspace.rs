//! The tier-3 workspace adapter: pi's minimal tool surface over `aibo-tools`.
//!
//! Five tools — `read`, `ls`, `write`, `edit`, `bash` — modelled on the pi
//! coding agent's deliberately small vocabulary, which is enough to fix a
//! failing test or write a script without a tool catalogue the model has to
//! study. Where pi ships them with no guard rails at all, here every call
//! passes through `aibo-agent`'s permission gate first (§11): this adapter
//! only ever executes an [`AuthorizedToolInvocation`], and file operations
//! bind to the gate's canonical `resolved_paths`, never to the raw arguments
//! the model supplied.
//!
//! Everything genuinely dangerous is delegated to `aibo-tools`' audited
//! shell/fs layer: [`Scope`] containment, [`ShellExecutor`]'s process-group
//! spawning and pipe-drain, size caps on reads. What this module owns is the
//! argument schemas, the intent derivation the gate prompts from, and the
//! unified diffs that make `write`/`edit` reviewable in the task window.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aibo_agent::{
    AuthorizedToolInvocation, ToolExecutor, ToolIntent, ToolInvocation,
    ToolOutput as AgentToolOutput,
};
use aibo_core::error::Result;
use aibo_core::types::{ApprovalKind, ToolSchema, ToolTier};
use aibo_tools::shell::{
    CommandApproval, Scope, ShellExecutor, ShellRequest, list_dir, read_file, write_file,
};
use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Longest a `bash` call may run, model-overridable up to [`MAX_TIMEOUT`].
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Ceiling on the model-requested timeout.
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// Lines a `read` returns when the model does not say otherwise (pi's value).
const DEFAULT_READ_LIMIT: usize = 2000;
/// Characters of combined output fed back per call. Beyond this the result is
/// truncated with a marker — an unbounded `cargo build` log is a context bomb.
const MAX_RESULT_CHARS: usize = 30_000;

/// A fail-closed workspace invariant was violated.
///
/// Like [`crate::AdapterError`], values carry classes only — never paths,
/// arguments, or wrapped error strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// No usable root: the scope would contain nothing.
    #[error("the workspace adapter needs at least one root")]
    NoRoots,
    /// A root could not be canonicalised.
    #[error("a workspace root could not be resolved")]
    UnresolvableRoot,
}

/// The tier-3 executor over one scoped workspace.
#[derive(Clone)]
pub struct WorkspaceExecutor {
    scope: Scope,
    shell: ShellExecutor,
    /// Default working directory and the base relative paths resolve against:
    /// the first root, canonical.
    workspace: PathBuf,
}

impl std::fmt::Debug for WorkspaceExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceExecutor")
            .field("surface", &"read/ls/write/edit/bash")
            .field("roots", &self.scope.roots().len())
            .finish()
    }
}

impl WorkspaceExecutor {
    /// An executor scoped to `roots`. The **first configured** root is the
    /// working directory.
    ///
    /// Taken from the caller's list, not from the scope: [`Scope::new`] sorts
    /// its roots, and with the default trio that made `Desktop` the anchor —
    /// so `Documents/x.txt` was written to `~/Desktop/Documents/x.txt`
    /// (observed 2026-08-01). Configuration order is meaning: the Files
    /// settings list puts the primary folder first.
    pub fn new(
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> std::result::Result<Self, WorkspaceError> {
        let roots: Vec<PathBuf> = roots.into_iter().collect();
        let workspace = roots.first().ok_or(WorkspaceError::NoRoots)?;
        let workspace =
            std::fs::canonicalize(workspace).map_err(|_| WorkspaceError::UnresolvableRoot)?;
        let scope = Scope::new(roots).map_err(|_| WorkspaceError::UnresolvableRoot)?;
        let shell = ShellExecutor::new(scope.clone());
        Ok(Self {
            scope,
            shell,
            workspace,
        })
    }

    /// A model-supplied path, absolutised against the workspace.
    ///
    /// The gate canonicalises whatever it is given against the *process*
    /// working directory, which is meaningless to the model — so relative
    /// paths are anchored here first, before the gate sees them.
    fn anchored(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.workspace.join(p)
        }
    }

    fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        args.get(key).and_then(serde_json::Value::as_str)
    }
}

#[async_trait]
impl ToolExecutor for WorkspaceExecutor {
    fn schemas(&self) -> Vec<ToolSchema> {
        let object = |properties: serde_json::Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required });
        vec![
            ToolSchema {
                name: "read".into(),
                description: "Read a text file. Returns at most `limit` lines from `offset` (1-indexed).".into(),
                parameters: object(
                    json!({
                        "path": { "type": "string" },
                        "offset": { "type": "integer", "minimum": 1 },
                        "limit": { "type": "integer", "minimum": 1 },
                    }),
                    &["path"],
                ),
                tier: 3,
            },
            ToolSchema {
                name: "ls".into(),
                description: "List a directory. Directories end with `/`, symlinks with `@`.".into(),
                parameters: object(json!({ "path": { "type": "string" } }), &[]),
                tier: 3,
            },
            ToolSchema {
                name: "write".into(),
                description: "Create or overwrite a file with exactly `content`.".into(),
                parameters: object(
                    json!({
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                    }),
                    &["path", "content"],
                ),
                tier: 3,
            },
            ToolSchema {
                name: "edit".into(),
                description: "Replace one exact occurrence of `old_text` (whitespace included) with `new_text` in a file. Fails unless `old_text` matches exactly once.".into(),
                parameters: object(
                    json!({
                        "path": { "type": "string" },
                        "old_text": { "type": "string" },
                        "new_text": { "type": "string" },
                    }),
                    &["path", "old_text", "new_text"],
                ),
                tier: 3,
            },
            ToolSchema {
                name: "bash".into(),
                description: "Run a shell command in the workspace. Optional `timeout_secs` (default 60, max 600).".into(),
                parameters: object(
                    json!({
                        "command": { "type": "string" },
                        "timeout_secs": { "type": "integer", "minimum": 1 },
                    }),
                    &["command"],
                ),
                tier: 3,
            },
        ]
    }

    fn intent(&self, call: &ToolInvocation) -> Option<ToolIntent> {
        // Best-effort argument reads: a malformed call still gets an intent
        // (and therefore a gate verdict), and `execute` rejects it politely.
        // Returning `None` here would misreport it as "no such tool".
        let path_of = |key: &str| {
            Self::str_arg(&call.args, key)
                .map(|p| self.anchored(p))
                .into_iter()
                .collect::<Vec<_>>()
        };
        match call.name.as_str() {
            "read" => Some(ToolIntent {
                tier: ToolTier::ShellFs,
                kind: ApprovalKind::Command,
                summary: format!(
                    "Read {}",
                    Self::str_arg(&call.args, "path").unwrap_or("<missing path>")
                ),
                command: None,
                paths: path_of("path"),
            }),
            "ls" => Some(ToolIntent {
                tier: ToolTier::ShellFs,
                kind: ApprovalKind::Command,
                summary: format!(
                    "List {}",
                    Self::str_arg(&call.args, "path").unwrap_or("the workspace")
                ),
                command: None,
                paths: match Self::str_arg(&call.args, "path") {
                    Some(p) => vec![self.anchored(p)],
                    None => vec![self.workspace.clone()],
                },
            }),
            "write" => Some(ToolIntent {
                tier: ToolTier::ShellFs,
                kind: ApprovalKind::FileWrite,
                summary: format!(
                    "Write {} ({} bytes)",
                    Self::str_arg(&call.args, "path").unwrap_or("<missing path>"),
                    Self::str_arg(&call.args, "content").map_or(0, str::len),
                ),
                command: None,
                paths: path_of("path"),
            }),
            "edit" => Some(ToolIntent {
                tier: ToolTier::ShellFs,
                kind: ApprovalKind::FileWrite,
                summary: format!(
                    "Edit {}",
                    Self::str_arg(&call.args, "path").unwrap_or("<missing path>")
                ),
                command: None,
                paths: path_of("path"),
            }),
            "bash" => {
                let command = Self::str_arg(&call.args, "command").unwrap_or("<missing command>");
                Some(ToolIntent {
                    tier: ToolTier::ShellFs,
                    kind: ApprovalKind::Command,
                    summary: format!("Run `{command}`"),
                    command: Some(command.to_owned()),
                    // The command runs *in* the workspace; the gate proves the
                    // cwd is contained. What the command touches beyond that
                    // is exactly why §11 calls path checks advisory and puts
                    // the weight on the approval prompt.
                    paths: vec![self.workspace.clone()],
                })
            }
            _ => None,
        }
    }

    async fn execute(
        &self,
        call: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<AgentToolOutput> {
        let (invocation, resolved_paths) = call.into_parts();
        let result = match invocation.name.as_str() {
            "read" => run_read(&self.scope, &invocation.args, &resolved_paths),
            "ls" => run_ls(&self.scope, &resolved_paths),
            "write" => run_write(&self.scope, &invocation.args, &resolved_paths),
            "edit" => run_edit(&self.scope, &invocation.args, &resolved_paths),
            "bash" => {
                return run_bash(&self.shell, &self.workspace, &invocation.args, cancel).await;
            }
            other => Err(format!("no such tool: {other}")),
        };
        Ok(match result {
            Ok(output) => output,
            Err(message) => AgentToolOutput {
                content: message,
                is_error: true,
                diffs: Vec::new(),
            },
        })
    }
}

/// The single canonical path the gate bound, or a polite refusal.
fn bound_path(resolved: &[PathBuf]) -> std::result::Result<&Path, String> {
    match resolved {
        [one] => Ok(one),
        _ => Err("the call did not bind exactly one canonical path".to_owned()),
    }
}

fn run_read(
    scope: &Scope,
    args: &serde_json::Value,
    resolved: &[PathBuf],
) -> std::result::Result<AgentToolOutput, String> {
    let path = bound_path(resolved)?;
    let text = read_file(scope, path).map_err(|e| e.to_string())?;
    let offset = args
        .get("offset")
        .and_then(serde_json::Value::as_u64)
        .map_or(0, |o| (o as usize).saturating_sub(1));
    let limit = args
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_READ_LIMIT, |l| l as usize);
    let total = text.lines().count();
    let body: String = text
        .lines()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>()
        .join("\n");
    let mut content = body;
    if offset + limit < total {
        content.push_str(&format!(
            "\n… truncated: showing lines {}–{} of {total}",
            offset + 1,
            offset + limit
        ));
    }
    Ok(AgentToolOutput {
        content: clamp(content),
        is_error: false,
        diffs: Vec::new(),
    })
}

fn run_ls(scope: &Scope, resolved: &[PathBuf]) -> std::result::Result<AgentToolOutput, String> {
    let path = bound_path(resolved)?;
    let entries = list_dir(scope, path).map_err(|e| e.to_string())?;
    let content = entries
        .iter()
        .map(|e| {
            let marker = if e.is_dir {
                "/"
            } else if e.is_symlink {
                "@"
            } else {
                ""
            };
            format!("{}{marker}", e.name)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(AgentToolOutput {
        content: clamp(content),
        is_error: false,
        diffs: Vec::new(),
    })
}

fn run_write(
    scope: &Scope,
    args: &serde_json::Value,
    resolved: &[PathBuf],
) -> std::result::Result<AgentToolOutput, String> {
    let path = bound_path(resolved)?;
    let content = WorkspaceExecutor::str_arg(args, "content").ok_or("`content` is required")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let before = std::fs::read_to_string(path).unwrap_or_default();
    write_file(scope, path, content, None).map_err(|e| e.to_string())?;
    Ok(AgentToolOutput {
        content: format!("wrote {} bytes to {}", content.len(), path.display()),
        is_error: false,
        diffs: vec![(path.to_path_buf(), unified_diff(path, &before, content))],
    })
}

fn run_edit(
    scope: &Scope,
    args: &serde_json::Value,
    resolved: &[PathBuf],
) -> std::result::Result<AgentToolOutput, String> {
    let path = bound_path(resolved)?;
    let old_text = WorkspaceExecutor::str_arg(args, "old_text").ok_or("`old_text` is required")?;
    let new_text = WorkspaceExecutor::str_arg(args, "new_text").ok_or("`new_text` is required")?;
    if old_text.is_empty() {
        return Err("`old_text` must not be empty".to_owned());
    }
    let before = read_file(scope, path).map_err(|e| e.to_string())?;
    let matches = before.matches(old_text).count();
    if matches == 0 {
        return Err("`old_text` was not found; it must match exactly, whitespace included".into());
    }
    if matches > 1 {
        return Err(format!(
            "`old_text` matches {matches} times; include more context so it matches exactly once"
        ));
    }
    let after = before.replacen(old_text, new_text, 1);
    write_file(scope, path, &after, None).map_err(|e| e.to_string())?;
    Ok(AgentToolOutput {
        content: format!("edited {}", path.display()),
        is_error: false,
        diffs: vec![(path.to_path_buf(), unified_diff(path, &before, &after))],
    })
}

async fn run_bash(
    shell: &ShellExecutor,
    workspace: &Path,
    args: &serde_json::Value,
    cancel: CancellationToken,
) -> Result<AgentToolOutput> {
    let Some(command) = WorkspaceExecutor::str_arg(args, "command") else {
        return Ok(AgentToolOutput {
            content: "`command` is required".to_owned(),
            is_error: true,
            diffs: Vec::new(),
        });
    };
    let mut request = ShellRequest::new(command, workspace);
    if let Some(secs) = args.get("timeout_secs").and_then(serde_json::Value::as_u64) {
        request.timeout = Duration::from_secs(secs).min(MAX_TIMEOUT);
    } else {
        request.timeout = DEFAULT_TIMEOUT;
    }
    // The user's consent already happened at the gate, against this exact
    // command and summary. This approval token re-binds the same text so the
    // executor's own TOCTOU check (`revalidate`) has something to hold it to.
    let approval = CommandApproval::granted(&request);
    let outcome = match shell.run(&request, &approval, cancel).await {
        Ok(outcome) => outcome,
        Err(error) => {
            return Ok(AgentToolOutput {
                content: error.to_string(),
                is_error: true,
                diffs: Vec::new(),
            });
        }
    };
    let mut content = String::new();
    if !outcome.stdout.is_empty() {
        content.push_str(&outcome.stdout);
    }
    if !outcome.stderr.is_empty() {
        if !content.is_empty() {
            content.push_str("\n--- stderr ---\n");
        }
        content.push_str(&outcome.stderr);
    }
    if outcome.truncated {
        content.push_str("\n… output truncated");
    }
    let exit = outcome
        .exit_code
        .map_or("signal".to_owned(), |code| code.to_string());
    content.push_str(&format!("\n(exit {exit}, {:?})", outcome.duration));
    Ok(AgentToolOutput {
        content: clamp(content),
        is_error: !outcome.succeeded(),
        diffs: Vec::new(),
    })
}

/// A unified diff for the task window's review pane.
fn unified_diff(path: &Path, before: &str, after: &str) -> String {
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(
            &format!("a/{}", path.display()),
            &format!("b/{}", path.display()),
        )
        .to_string()
}

/// Bound what a single result feeds back into the model's context.
fn clamp(mut content: String) -> String {
    if content.chars().count() > MAX_RESULT_CHARS {
        content = content.chars().take(MAX_RESULT_CHARS).collect();
        content.push_str("\n… truncated");
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn executor(root: &Path) -> WorkspaceExecutor {
        WorkspaceExecutor::new([root.to_path_buf()]).expect("scope")
    }

    fn authorized(
        exec: &WorkspaceExecutor,
        name: &str,
        args: serde_json::Value,
    ) -> AuthorizedToolInvocation {
        let call = ToolInvocation {
            id: "c1".into(),
            name: name.into(),
            args,
        };
        let paths = exec
            .intent(&call)
            .map(|i| i.paths)
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                // Stand in for the gate: canonicalise the deepest existing
                // ancestor the way `resolve_for_containment` does.
                aibo_agent::resolve_for_containment(&p).expect("resolvable")
            })
            .collect();
        AuthorizedToolInvocation::preauthorized_for_tests(call, paths)
    }

    #[tokio::test]
    async fn write_read_edit_round_trip_with_diffs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        let exec = executor(&root);

        let write = authorized(
            &exec,
            "write",
            json!({ "path": "notes/hello.txt", "content": "hello\nworld\n" }),
        );
        let out = exec.execute(write, CancellationToken::new()).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.diffs.len(), 1);
        assert!(out.diffs[0].1.contains("+hello"), "{}", out.diffs[0].1);

        let read = authorized(&exec, "read", json!({ "path": "notes/hello.txt" }));
        let out = exec.execute(read, CancellationToken::new()).await.unwrap();
        assert_eq!(out.content, "hello\nworld");

        let edit = authorized(
            &exec,
            "edit",
            json!({ "path": "notes/hello.txt", "old_text": "world", "new_text": "aibo" }),
        );
        let out = exec.execute(edit, CancellationToken::new()).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.diffs[0].1.contains("-world"), "{}", out.diffs[0].1);
        assert!(out.diffs[0].1.contains("+aibo"));

        let ambiguous = authorized(
            &exec,
            "edit",
            json!({ "path": "notes/hello.txt", "old_text": "l", "new_text": "L" }),
        );
        let out = exec
            .execute(ambiguous, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("matches"), "{}", out.content);
    }

    /// The Desktop/Documents regression: `Scope` sorts its roots, and the
    /// workspace must follow the *configured* order, not the sorted one.
    #[test]
    fn the_workspace_is_the_first_configured_root_not_the_first_sorted_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = dir.path().join("b-documents");
        let a = dir.path().join("a-desktop");
        std::fs::create_dir_all(&a).expect("mkdir");
        std::fs::create_dir_all(&b).expect("mkdir");
        let exec = WorkspaceExecutor::new([b.clone(), a]).expect("scope");
        let anchored = exec.anchored("x.txt");
        assert!(
            anchored.ends_with("b-documents/x.txt"),
            "{}",
            anchored.display()
        );
    }

    #[tokio::test]
    async fn relative_paths_anchor_to_the_workspace_not_the_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        let exec = executor(&root);
        let intent = exec
            .intent(&ToolInvocation {
                id: "c1".into(),
                name: "read".into(),
                args: json!({ "path": "src/main.rs" }),
            })
            .expect("intent");
        assert!(intent.paths[0].starts_with(&root), "{:?}", intent.paths);
    }

    #[tokio::test]
    async fn bash_runs_in_the_workspace_and_reports_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        let exec = executor(&root);
        let call = authorized(&exec, "bash", json!({ "command": "pwd && printf x >&2" }));
        let out = exec.execute(call, CancellationToken::new()).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains(root.to_str().unwrap()),
            "{}",
            out.content
        );
        assert!(out.content.contains("stderr"), "{}", out.content);
        assert!(out.content.contains("(exit 0"), "{}", out.content);
    }

    #[test]
    fn the_surface_is_exactly_pi_plus_ls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = executor(&dir.path().canonicalize().expect("canonical"));
        let names: Vec<String> = exec.schemas().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["read", "ls", "write", "edit", "bash"]);
        assert!(exec.schemas().iter().all(|s| s.tier == 3));
    }
}
