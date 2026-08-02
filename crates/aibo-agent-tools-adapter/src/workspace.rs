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
    CommandApproval, Scope, ShellExecutor, ShellRequest, is_platform_shell_tool,
    platform_shell_tool_name, read_file, write_file,
};
use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Longest a shell call may run, model-overridable up to [`MAX_TIMEOUT`].
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
            .field("surface", &"read/shell/edit/write")
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
        // pi's tool surface, spelled pi's way (owner: "use the same harness
        // as pi coding agent"): read, shell, edit, write — no ls; the prompt
        // tells the model to use the platform shell for discovery. `edit`
        // takes an ARRAY of {oldText, newText} replacements.
        let object = |properties: serde_json::Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required });
        let shell_description = if cfg!(windows) {
            "Execute a PowerShell command in the current working directory. Use Windows/PowerShell syntax such as Get-ChildItem, Get-Content, and $env:USERPROFILE; do not use bash, sh, chcp, or POSIX $VARIABLE syntax. UTF-8 input and output are configured automatically. Returns stdout and stderr. Output is truncated when very large. Optionally provide a timeout in seconds (default 60, max 600). Commands run non-interactively (stdin is closed); for a simple prompt, pass its answer via `stdin`. Commands classified destructive are refused once — if genuinely required, state why and retry with `confirm_destructive: true`."
        } else {
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated when very large. Optionally provide a timeout in seconds (default 60, max 600). Commands run non-interactively (stdin is closed): prefer non-interactive flags (--yes, --no-edit); for a simple prompt, pass its answer via `stdin`. Commands classified destructive (recursive deletes, force pushes, privilege escalation, disk tools) are refused once — if genuinely required, state why and retry with `confirm_destructive: true`."
        };
        vec![
            ToolSchema {
                name: "read".into(),
                description: "Read the contents of a text file. Output is truncated to 2000 lines. Use offset/limit for large files. When you need the full file, continue with offset until complete.".into(),
                parameters: object(
                    json!({
                        "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                        "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
                        "limit": { "type": "integer", "description": "Maximum number of lines to read" },
                    }),
                    &["path"],
                ),
                tier: 3,
            },
            ToolSchema {
                name: platform_shell_tool_name().into(),
                description: shell_description.into(),
                parameters: object(
                    json!({
                        "command": { "type": "string", "description": "Platform-native shell command to execute" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds (optional)" },
                        "stdin": { "type": "string", "description": "Text written to the command's stdin before it is closed (for y/n prompts and heredocs)" },
                        "confirm_destructive": { "type": "boolean", "description": "Set true only after restating why a destructive command is required for the task" },
                    }),
                    &["command"],
                ),
                tier: 3,
            },
            ToolSchema {
                name: "edit".into(),
                description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".into(),
                parameters: object(
                    json!({
                        "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                        "edits": {
                            "type": "array",
                            "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
                                    "newText": { "type": "string", "description": "Replacement text for this targeted edit." },
                                },
                                "required": ["oldText", "newText"],
                            },
                        },
                    }),
                    &["path", "edits"],
                ),
                tier: 3,
            },
            ToolSchema {
                name: "write".into(),
                description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".into(),
                parameters: object(
                    json!({
                        "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                        "content": { "type": "string", "description": "Content to write to the file" },
                    }),
                    &["path", "content"],
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
                    "Edit {} ({} replacement{})",
                    Self::str_arg(&call.args, "path").unwrap_or("<missing path>"),
                    edit_count(&call.args),
                    if edit_count(&call.args) == 1 { "" } else { "s" },
                ),
                command: None,
                paths: path_of("path"),
            }),
            name if is_platform_shell_tool(name) => {
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
            "write" => run_write(&self.scope, &invocation.args, &resolved_paths),
            "edit" => run_edit(&self.scope, &invocation.args, &resolved_paths),
            name if is_platform_shell_tool(name) => {
                return run_shell(&self.shell, &self.workspace, &invocation.args, cancel).await;
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
    let edits = args
        .get("edits")
        .and_then(serde_json::Value::as_array)
        .ok_or("`edits` is required and must be an array")?;
    if edits.is_empty() {
        return Err("`edits` must contain at least one replacement".to_owned());
    }
    let before = read_file(scope, path).map_err(|e| e.to_string())?;
    // Every oldText is matched against the ORIGINAL file (pi's contract), so
    // ranges are found first and applied back-to-front — an earlier edit
    // must not shift or create a later match.
    let mut ranges: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let old_text = edit
            .get("oldText")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("edits[{index}].oldText is required"))?;
        let new_text = edit
            .get("newText")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("edits[{index}].newText is required"))?;
        if old_text.is_empty() {
            return Err(format!("edits[{index}].oldText must not be empty"));
        }
        let matches = before.matches(old_text).count();
        if matches == 0 {
            return Err(format!(
                "edits[{index}].oldText was not found; it must match exactly, whitespace included"
            ));
        }
        if matches > 1 {
            return Err(format!(
                "edits[{index}].oldText matches {matches} times; include more context so it matches exactly once"
            ));
        }
        let at = before.find(old_text).expect("counted above");
        ranges.push((at, at + old_text.len(), new_text));
    }
    ranges.sort_by_key(|(at, ..)| *at);
    if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("edits overlap; merge changes that touch the same block into one edit".into());
    }
    let mut after = before.clone();
    for (at, end, new_text) in ranges.into_iter().rev() {
        after.replace_range(at..end, new_text);
    }
    write_file(scope, path, &after, None).map_err(|e| e.to_string())?;
    Ok(AgentToolOutput {
        content: format!("edited {} ({} replacement(s))", path.display(), edits.len()),
        is_error: false,
        diffs: vec![(path.to_path_buf(), unified_diff(path, &before, &after))],
    })
}

/// How many replacements an `edit` call carries, for its intent summary.
fn edit_count(args: &serde_json::Value) -> usize {
    args.get("edits")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len)
}

async fn run_shell(
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
    // Agent self-confirmation for the destructive class (owner, 2026-08-01:
    // no user prompt — the model confirms its own intent). The first attempt
    // is refused with instructions; the retry carries an explicit flag, and
    // the model's restated reasoning lands in the transcript where a human
    // can audit it afterwards.
    if aibo_agent::is_destructive_command(command)
        && !args
            .get("confirm_destructive")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    {
        return Ok(AgentToolOutput {
            content: format!(
                "`{command}` is classified as destructive (deletes, force-push, \
                 privilege escalation, or disk-level). If it is genuinely required \
                 for the task, state why and call {} again with \
                 \"confirm_destructive\": true. Otherwise choose a safer command.",
                platform_shell_tool_name()
            ),
            is_error: true,
            diffs: Vec::new(),
        });
    }

    let mut request = ShellRequest::new(command, workspace);
    request.stdin = WorkspaceExecutor::str_arg(args, "stdin").map(str::to_owned);
    if let Some(secs) = args.get("timeout").and_then(serde_json::Value::as_u64) {
        request.timeout = Duration::from_secs(secs).min(MAX_TIMEOUT);
    } else {
        request.timeout = DEFAULT_TIMEOUT;
    }
    // Consent is the agent's own under YOLO (owner, 2026-08-01): a
    // destructive command got here only through the confirm_destructive
    // retry above, so the adapter supplies the executor's typed-confirmation
    // binding itself. The token still binds the exact command text and cwd,
    // so the TOCTOU revalidation keeps its teeth.
    let approval = if aibo_agent::is_destructive_command(command) {
        CommandApproval::typed(&request, command)
    } else {
        CommandApproval::granted(&request)
    };
    let outcome = match shell.run(&request, &approval, cancel).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let mut content = error.to_string();
            if content.contains("Timeout") || content.contains("timed out") {
                content.push_str(
                    "\nThe command may have been waiting for input; pass its answer \
                     via `stdin` or use a non-interactive flag.",
                );
            }
            return Ok(AgentToolOutput {
                content,
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
///
/// Plain absolute paths in the header: the `a/`-`b/` convention is for
/// repo-relative paths, and prefixing an absolute one rendered as
/// `a//Users/…` (owner screenshot, 2026-08-01).
fn unified_diff(path: &Path, before: &str, after: &str) -> String {
    let label = path.display().to_string();
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(&label, &label)
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
            json!({ "path": "notes/hello.txt", "edits": [{ "oldText": "world", "newText": "aibo" }] }),
        );
        let out = exec.execute(edit, CancellationToken::new()).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.diffs[0].1.contains("-world"), "{}", out.diffs[0].1);
        assert!(out.diffs[0].1.contains("+aibo"));

        let ambiguous = authorized(
            &exec,
            "edit",
            json!({ "path": "notes/hello.txt", "edits": [{ "oldText": "l", "newText": "L" }] }),
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

    /// Unix-only: the command spelling (`pwd`, `printf`, `>&2`) is /bin/sh's.
    /// Windows process spawning itself is covered by `aibo-tools`' own
    /// executor tests; this one asserts the adapter's plumbing.
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runs_in_the_workspace_and_reports_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        let exec = executor(&root);
        let call = authorized(
            &exec,
            platform_shell_tool_name(),
            json!({ "command": "pwd && printf x >&2" }),
        );
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

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_runs_windows_native_commands_and_environment_syntax() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        std::fs::write(root.join("日本語.txt"), "hello").expect("write fixture");
        let exec = executor(&root);
        let call = authorized(
            &exec,
            platform_shell_tool_name(),
            json!({
                "command": "Get-ChildItem -Force | Select-Object -ExpandProperty Name; Write-Output $env:USERPROFILE"
            }),
        );

        let out = exec.execute(call, CancellationToken::new()).await.unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("日本語.txt"), "{}", out.content);
        assert!(
            std::env::var("USERPROFILE").is_ok_and(|profile| out.content.contains(&profile)),
            "{}",
            out.content
        );
        assert!(!out.content.contains('\u{fffd}'), "{}", out.content);
    }

    /// pi's multi-edit contract: all oldText matched against the ORIGINAL,
    /// applied without earlier edits shifting later ones, overlaps refused.
    #[tokio::test]
    async fn multi_edits_apply_against_the_original_and_refuse_overlap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        let exec = executor(&root);
        std::fs::write(root.join("f.txt"), "alpha beta gamma\n").expect("write");

        let multi = authorized(
            &exec,
            "edit",
            json!({ "path": "f.txt", "edits": [
                { "oldText": "gamma", "newText": "delta" },
                { "oldText": "alpha", "newText": "omega" },
            ]}),
        );
        let out = exec.execute(multi, CancellationToken::new()).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "omega beta delta\n"
        );

        let overlapping = authorized(
            &exec,
            "edit",
            json!({ "path": "f.txt", "edits": [
                { "oldText": "omega beta", "newText": "x" },
                { "oldText": "beta delta", "newText": "y" },
            ]}),
        );
        let out = exec
            .execute(overlapping, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("overlap"), "{}", out.content);
    }

    /// The self-confirmation dance: a destructive command is refused once
    /// with instructions, and the confirmed retry runs — no user in the loop.
    #[tokio::test]
    async fn destructive_shell_requires_the_agents_own_confirmation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        std::fs::create_dir(root.join("junk")).expect("mkdir");
        let exec = executor(&root);
        let command = if cfg!(windows) {
            "Remove-Item -Recurse -Force junk"
        } else {
            "rm -rf junk"
        };

        let first = authorized(
            &exec,
            platform_shell_tool_name(),
            json!({ "command": command }),
        );
        let out = exec.execute(first, CancellationToken::new()).await.unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("confirm_destructive"),
            "{}",
            out.content
        );
        assert!(root.join("junk").exists(), "nothing ran");

        let confirmed = authorized(
            &exec,
            platform_shell_tool_name(),
            json!({ "command": command, "confirm_destructive": true }),
        );
        let out = exec
            .execute(confirmed, CancellationToken::new())
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(!root.join("junk").exists(), "the confirmed retry ran");
    }

    /// Interactive commands: stdin text is delivered and then closed, so a
    /// prompt reads its answer instead of hanging.
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_stdin_feeds_a_prompt_and_then_eof() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = executor(&dir.path().canonicalize().expect("canonical"));
        let call = authorized(
            &exec,
            platform_shell_tool_name(),
            json!({ "command": "read answer && echo got:$answer", "stdin": "yes\n" }),
        );
        let out = exec.execute(call, CancellationToken::new()).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("got:yes"), "{}", out.content);
    }

    #[test]
    fn the_surface_is_exactly_pis() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = executor(&dir.path().canonicalize().expect("canonical"));
        let names: Vec<String> = exec.schemas().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            ["read", platform_shell_tool_name(), "edit", "write"],
            "pi's order, pi's set"
        );
        assert!(exec.schemas().iter().all(|s| s.tier == 3));
    }
}
