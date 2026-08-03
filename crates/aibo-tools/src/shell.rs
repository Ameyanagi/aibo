//! Tier 3: shell and filesystem, path-scoped (§11).
//!
//! # What this module claims, and what it does not
//!
//! §11 is explicit that the permission tiers are a UX pattern, not a security
//! boundary, and that "subprocess writes outside allowed roots" is mitigated by
//! *the sandbox*, not by path checks — "path checks alone are advisory". This
//! module implements the advisory checks carefully and claims nothing more:
//!
//! * [`Scope`] resolves to a canonical path and re-checks containment **after**
//!   resolution. Checking before resolution is the classic symlink-escape bug,
//!   so the API deliberately has no "check this string" entry point.
//! * [`ShellExecutor`] re-validates the path *and* the command at execution
//!   time, not just at approval time (§11 TOCTOU row).
//! * [`classify_command`] refuses `rm -rf` and force-push class commands
//!   without a typed confirmation, per §11's non-negotiables.
//! * [`SnapshotSet`] is "revert these file changes". It is **not** an undo, and
//!   [`SnapshotSet::LIMITATIONS`] is the copy that must appear next to the
//!   button.
//!
//! Once a shell command is running, none of the above constrains it. A command
//! is free to write anywhere the user can write. The honest statement for the
//! UI is "aibo will not knowingly act outside these folders", not "aibo cannot".

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use aibo_core::traits::{ChildProcessObserver, ChildProcessRegistration};
use aibo_core::types::{ToolSchema, ToolTier};
use async_trait::async_trait;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;
#[cfg(windows)]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::args::{invalid, str_arg};
use crate::{DenyReason, Tool, ToolError, ToolOutput, ToolResult};

/// Largest file [`read_file`] will return.
pub const MAX_READ_BYTES: usize = 1 << 20;

/// Largest file [`SnapshotSet`] will keep a before-image of.
pub const MAX_SNAPSHOT_BYTES: usize = 8 << 20;

/// Largest stdout/stderr capture kept from a command.
pub const MAX_CAPTURE_BYTES: usize = 256 << 10;

/// Largest stdin payload accepted for a command.
pub const MAX_STDIN_BYTES: usize = 1 << 20;

/// Default wall-clock limit for one command.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// The shell tool name exposed to the model on this platform.
///
/// Calling a `cmd.exe` executor "bash" taught the model to emit POSIX syntax
/// on Windows. Give the model the actual interpreter name so commands match
/// the machine they run on.
pub const fn platform_shell_tool_name() -> &'static str {
    if cfg!(windows) { "powershell" } else { "bash" }
}

/// Whether `name` is executable by this platform's workspace shell.
///
/// Windows accepts the old `bash` spelling for in-flight conversations from
/// before the schema changed, but new schemas advertise only `powershell`.
pub fn is_platform_shell_tool(name: &str) -> bool {
    name == platform_shell_tool_name() || (cfg!(windows) && name == "bash")
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// The set of directories the user added, resolved to canonical form.
///
/// Construction canonicalises the roots themselves. That matters on macOS,
/// where `/tmp` is a symlink to `/private/tmp` and an uncanonicalised root
/// would reject every path inside it, and on Windows, where canonicalisation
/// yields `\\?\`-prefixed paths that never compare equal to the user's input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    roots: Vec<PathBuf>,
}

impl Scope {
    /// Canonicalise and store the roots.
    ///
    /// A root that does not exist is an error rather than a silently ignored
    /// entry: a scope that quietly contains nothing would deny everything and
    /// look like a bug elsewhere.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> ToolResult<Self> {
        let mut canonical = Vec::new();
        for root in roots {
            let root = std::fs::canonicalize(&root)?;
            if !root.is_dir() {
                return Err(invalid("scope", "a scope root must be a directory"));
            }
            canonical.push(root);
        }
        canonical.sort();
        canonical.dedup();
        Ok(Self { roots: canonical })
    }

    /// The canonical roots.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether an **already canonical** path is inside a root.
    ///
    /// Takes a canonical path on purpose. Calling this with a raw user path is
    /// the bug §11 names; there is no overload that accepts one.
    pub fn contains(&self, canonical: &Path) -> bool {
        self.roots.iter().any(|root| canonical.starts_with(root))
    }

    /// Resolve a path that must already exist, then check containment.
    ///
    /// Order matters and is the whole point: `canonicalize` follows every
    /// symlink and junction in the path, and only the result is tested.
    pub fn resolve_existing(&self, path: &Path) -> ToolResult<PathBuf> {
        let canonical = std::fs::canonicalize(path)?;
        self.check(path, canonical)
    }

    /// Resolve a path that may not exist yet, then check containment.
    ///
    /// If the final component already exists — including as a symlink — this
    /// defers to [`Scope::resolve_existing`], so writing *through* a symlink
    /// that leaves the scope is refused rather than followed. Otherwise the
    /// parent directory is canonicalised and the name appended, which is what
    /// makes "create a file here" possible at all.
    pub fn resolve_for_create(&self, path: &Path) -> ToolResult<PathBuf> {
        if std::fs::symlink_metadata(path).is_ok() {
            return self.resolve_existing(path);
        }
        let parent = path.parent().ok_or_else(|| DenyReason::OutsideScope {
            path: path.display().to_string(),
        })?;
        let name = path.file_name().ok_or_else(|| DenyReason::OutsideScope {
            path: path.display().to_string(),
        })?;
        // `..` as the final component has no file_name, so it cannot reach
        // here; any other component is a plain name.
        if matches!(
            path.components().next_back(),
            Some(Component::ParentDir) | Some(Component::RootDir)
        ) {
            return Err(DenyReason::OutsideScope {
                path: path.display().to_string(),
            }
            .into());
        }
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let canonical_parent = std::fs::canonicalize(parent)?;
        self.check(path, canonical_parent.join(name))
    }

    fn check(&self, requested: &Path, canonical: PathBuf) -> ToolResult<PathBuf> {
        if self.contains(&canonical) {
            return Ok(canonical);
        }
        // Distinguish the two failures for the message only — both deny.
        // "Your link points out of the folder" is actionable; "outside the
        // allowed roots" for a path that *looks* inside is baffling.
        let looks_inside = self
            .roots
            .iter()
            .any(|root| lexically_normalise(requested).starts_with(root));
        Err(if looks_inside {
            DenyReason::SymlinkEscape {
                requested: requested.display().to_string(),
                resolved: canonical.display().to_string(),
            }
        } else {
            DenyReason::OutsideScope {
                path: canonical.display().to_string(),
            }
        }
        .into())
    }
}

/// Remove `.` and collapse `..` textually. Used **only** to phrase the error
/// message; never as a containment check, because it does not follow links.
fn lexically_normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Command classification
// ---------------------------------------------------------------------------

/// How dangerous a command is judged to be (§11 non-negotiables).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRisk {
    /// Runs after the ordinary tier-3 approval.
    Ordinary,
    /// Needs the user to type the command out before it runs. The payload
    /// names the class, for the prompt.
    TypedConfirmation(&'static str),
}

/// Classify a command line.
///
/// Deliberately crude and deliberately over-eager. This is a **speed bump in
/// front of the irreversible**, not a parser: it cannot see through `$VAR`,
/// aliases, `eval`, or a script that does the same thing one level down. A
/// false positive costs the user one confirmation; a false negative costs them
/// a repository.
pub fn classify_command(command: &str) -> CommandRisk {
    let lower = command.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let original_tokens: Vec<&str> = command.split_whitespace().collect();
    let has = |t: &str| tokens.contains(&t);
    let starts_with_word = |w: &str| tokens.first().is_some_and(|t| *t == w);

    // Recursive, forced delete.
    if tokens.contains(&"rm") {
        let flags: String = tokens
            .iter()
            .filter(|t| t.starts_with('-') && !t.starts_with("--"))
            .flat_map(|t| t.chars())
            .collect();
        let recursive = flags.contains('r') || flags.contains('R') || has("--recursive");
        let forced = flags.contains('f') || has("--force");
        if recursive || forced {
            return CommandRisk::TypedConfirmation("recursive or forced delete");
        }
    }
    if lower.contains("rmdir /s")
        || lower.contains("rmdir -recurse")
        || lower.contains("rmdir -force")
        || lower.contains("del /f")
        || lower.contains("del /q")
    {
        return CommandRisk::TypedConfirmation("recursive or forced delete");
    }
    if (has("remove-item") || starts_with_word("ri"))
        && tokens
            .iter()
            .any(|t| matches!(*t, "-recurse" | "-force" | "-r"))
    {
        return CommandRisk::TypedConfirmation("recursive or forced delete");
    }

    // History rewrites and destructive git.
    if starts_with_word("git") || has("git") {
        let force_push = has("push")
            && (has("--force") || has("-f"))
            && !tokens.iter().any(|t| t.starts_with("--force-with-lease"));
        if force_push {
            return CommandRisk::TypedConfirmation("force push");
        }
        if has("reset") && has("--hard") {
            return CommandRisk::TypedConfirmation("discards uncommitted work");
        }
        if has("clean") && tokens.iter().any(|t| t.starts_with('-') && t.contains('f')) {
            return CommandRisk::TypedConfirmation("deletes untracked files");
        }
        if has("filter-branch") || has("filter-repo") {
            return CommandRisk::TypedConfirmation("rewrites history");
        }
        if has("branch")
            && original_tokens
                .iter()
                .any(|token| matches!(*token, "-D" | "--delete"))
        {
            return CommandRisk::TypedConfirmation("deletes a branch");
        }
    }

    // `dd` is destructive when it has an output sink. Checking `if=` instead
    // mistakes the harmless input operand for the destination and misses such
    // common forms as `dd of=/dev/disk0 if=image.img`.
    if has("dd") && tokens.iter().any(|token| token.starts_with("of=")) {
        return CommandRisk::TypedConfirmation("raw device write");
    }

    // Whole-device and system-level operations.
    for (needle, why) in [
        ("mkfs", "formats a filesystem"),
        ("diskutil erase", "erases a disk"),
        ("format ", "formats a volume"),
        ("shutdown", "shuts the machine down"),
        ("reboot", "restarts the machine"),
        ("chmod -r", "recursive permission change"),
        ("chown -r", "recursive ownership change"),
        ("takeown", "seizes file ownership"),
        (":(){", "fork bomb"),
    ] {
        if lower.contains(needle) {
            return CommandRisk::TypedConfirmation(why);
        }
    }

    // Privilege escalation, and the pipe-to-shell idiom.
    if starts_with_word("sudo") || starts_with_word("doas") || starts_with_word("runas") {
        return CommandRisk::TypedConfirmation("runs as another user");
    }
    if (lower.contains("curl ") || lower.contains("wget ") || lower.contains("iwr "))
        && (lower.contains("| sh")
            || lower.contains("|sh")
            || lower.contains("| bash")
            || lower.contains("|bash")
            || lower.contains("iex"))
    {
        return CommandRisk::TypedConfirmation("downloads and executes a script");
    }

    CommandRisk::Ordinary
}

/// Characters that give the shell control-flow, redirection or substitution.
///
/// Their presence does not block anything; it is what the approval UI must
/// highlight, because a command containing them can reach outside the scope no
/// matter what [`Scope`] says.
pub fn shell_metacharacters(command: &str) -> bool {
    command.contains(['|', ';', '&', '>', '<', '`', '$', '(', ')', '\n'])
}

// ---------------------------------------------------------------------------
// Approval and execution
// ---------------------------------------------------------------------------

/// A command the user was asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRequest {
    /// The exact command line, as it will be run and as it was shown.
    pub command: String,
    /// Working directory, as requested (not yet resolved).
    pub cwd: PathBuf,
    /// Wall-clock limit.
    pub timeout: Duration,
    /// Text written to the command's stdin, which is then closed.
    ///
    /// `None` closes stdin immediately — commands run non-interactively by
    /// default, and one that prompts will read EOF rather than hang. `Some`
    /// covers the `y`-confirmation class without opening a real TTY.
    pub stdin: Option<String>,
}

impl ShellRequest {
    /// A request with the default timeout.
    pub fn new(command: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            cwd: cwd.into(),
            timeout: DEFAULT_COMMAND_TIMEOUT,
            stdin: None,
        }
    }

    /// Whether the command can reach the shell's own features (§11: path
    /// checks are advisory, and this is why).
    pub fn uses_shell_features(&self) -> bool {
        shell_metacharacters(&self.command)
    }

    /// What the user must be asked.
    pub fn risk(&self) -> CommandRisk {
        classify_command(&self.command)
    }
}

/// The user's decision about one [`ShellRequest`].
///
/// Bound to the exact command string and the exact working directory. It is
/// **not** a capability for "commands like this one": §11 requires the exact
/// command to be shown, and an approval that outlived its text would make that
/// display a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandApproval {
    /// The command that was displayed and approved.
    pub command: String,
    /// The working directory that was displayed and approved.
    pub cwd: PathBuf,
    /// What the user typed, when the class required typed confirmation.
    pub typed_confirmation: Option<String>,
    /// When consent was given.
    pub granted_at: SystemTime,
}

impl CommandApproval {
    /// Record an ordinary approval.
    pub fn granted(request: &ShellRequest) -> Self {
        Self {
            command: request.command.clone(),
            cwd: request.cwd.clone(),
            typed_confirmation: None,
            granted_at: SystemTime::now(),
        }
    }

    /// Record an approval where the user typed the command out.
    pub fn typed(request: &ShellRequest, typed: impl Into<String>) -> Self {
        Self {
            command: request.command.clone(),
            cwd: request.cwd.clone(),
            typed_confirmation: Some(typed.into()),
            granted_at: SystemTime::now(),
        }
    }
}

/// What a command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    /// Exit code, or `None` when the process was signalled.
    pub exit_code: Option<i32>,
    /// Captured stdout, possibly truncated.
    pub stdout: String,
    /// Captured stderr, possibly truncated.
    pub stderr: String,
    /// Whether either capture was truncated.
    pub truncated: bool,
    /// How long it ran.
    pub duration: Duration,
}

impl ShellOutcome {
    /// Whether the command reported success.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Runs approved commands inside a [`Scope`].
#[derive(Debug, Clone)]
pub struct ShellExecutor {
    scope: Scope,
    process_observer: Option<Arc<dyn ChildProcessObserver>>,
}

impl ShellExecutor {
    /// An executor bound to a scope.
    pub const fn new(scope: Scope) -> Self {
        Self {
            scope,
            process_observer: None,
        }
    }

    /// Report spawned shells to the application-owned crash-recovery ledger.
    #[must_use]
    pub fn with_process_observer(mut self, observer: Arc<dyn ChildProcessObserver>) -> Self {
        self.process_observer = Some(observer);
        self
    }

    /// The scope in force.
    pub const fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Re-validate an approval against the request **and against the world as
    /// it is now**, without running anything.
    ///
    /// This is §11's TOCTOU row in one function. Between the approval prompt
    /// and this moment, three things can have changed: the command text (a bug
    /// or an injection), the working directory's identity (someone replaced it
    /// with a symlink), and the command's risk class (it was re-read). All
    /// three are checked here, at execution time.
    pub fn revalidate(
        &self,
        request: &ShellRequest,
        approval: &CommandApproval,
    ) -> ToolResult<PathBuf> {
        if approval.command != request.command {
            return Err(DenyReason::Stale {
                detail: "the command changed after it was approved".to_owned(),
            }
            .into());
        }
        if approval.cwd != request.cwd {
            return Err(DenyReason::Stale {
                detail: "the working directory changed after it was approved".to_owned(),
            }
            .into());
        }

        // Resolved now, not at approval time: the directory may have become a
        // symlink out of the scope in between.
        let cwd = self.scope.resolve_existing(&request.cwd)?;
        if !cwd.is_dir() {
            return Err(DenyReason::Stale {
                detail: "the working directory is no longer a directory".to_owned(),
            }
            .into());
        }

        // Re-classified now, so an approval taken under one classification
        // cannot be replayed against a command that reads differently today.
        if let CommandRisk::TypedConfirmation(why) = classify_command(&request.command) {
            let typed_matches = approval
                .typed_confirmation
                .as_deref()
                .is_some_and(|typed| typed.trim() == request.command.trim());
            if !typed_matches {
                return Err(DenyReason::NeedsTypedConfirmation {
                    command: request.command.clone(),
                    why,
                }
                .into());
            }
        }

        Ok(cwd)
    }

    /// Re-validate and run.
    ///
    /// The command goes to the platform shell (`sh -c` / PowerShell `-Command`) because §11
    /// specifies "the exact command" is what the user sees and approves, and
    /// splitting it into an argv would run something different from what was
    /// displayed. The cost is stated plainly in the module docs: the shell can
    /// reach outside the scope, and only a real sandbox stops it.
    pub async fn run(
        &self,
        request: &ShellRequest,
        approval: &CommandApproval,
        cancel: CancellationToken,
    ) -> ToolResult<ShellOutcome> {
        if request
            .stdin
            .as_ref()
            .is_some_and(|input| input.len() > MAX_STDIN_BYTES)
        {
            return Err(invalid(
                "shell",
                format!("`stdin` must be at most {MAX_STDIN_BYTES} bytes"),
            ));
        }
        let cwd = self.revalidate(request, approval)?;
        let started = Instant::now();

        let mut command = platform_command(&request.command);
        command
            .current_dir(&cwd)
            .stdin(if request.stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Without this a cancelled run leaves an orphan holding the pipes.
            .kill_on_drop(true);

        let mut command = CommandWrap::from(command);
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        {
            command.wrap(CreationFlags(CREATE_NO_WINDOW));
            command.wrap(JobObject);
        }

        let mut child = command.spawn()?;
        let _registration = ChildProcessRegistration::new(
            self.process_observer.clone(),
            child.id(),
            platform_shell_identity(),
        );
        let stdin = if request.stdin.is_some() {
            Some(child.stdin().take().ok_or_else(|| {
                std::io::Error::other("spawned command did not expose its stdin pipe")
            })?)
        } else {
            None
        };
        let stdout = child.stdout().take().ok_or_else(|| {
            std::io::Error::other("spawned command did not expose its stdout pipe")
        })?;
        let stderr = child.stderr().take().ok_or_else(|| {
            std::io::Error::other("spawned command did not expose its stderr pipe")
        })?;

        // Both pipes must be drained while the process runs. Waiting first can
        // deadlock as soon as either OS pipe buffer fills; collecting the whole
        // body fixes the deadlock but lets an untrusted command exhaust memory.
        enum WaitOutcome {
            Complete(std::io::Result<(std::process::ExitStatus, CapturedOutput, CapturedOutput)>),
            Cancelled,
            TimedOut,
        }

        // Keep the future in this block so its mutable borrow of `child` is
        // definitely released before the cancellation cleanup below.
        let outcome = {
            let wait_and_drain = async {
                let write_stdin = async {
                    if let (Some(mut handle), Some(input)) = (stdin, request.stdin.as_ref()) {
                        use tokio::io::AsyncWriteExt;
                        // Broken-pipe is not itself a command failure: a child
                        // may deliberately consume only part of its input.
                        let _ = handle.write_all(input.as_bytes()).await;
                        let _ = handle.shutdown().await;
                    }
                    Ok::<_, std::io::Error>(())
                };
                let (status, stdout, stderr, ()) = tokio::try_join!(
                    child.wait(),
                    drain_capture(stdout),
                    drain_capture(stderr),
                    write_stdin,
                )?;
                Ok::<_, std::io::Error>((status, stdout, stderr))
            };

            tokio::select! {
                biased;
                () = cancel.cancelled() => WaitOutcome::Cancelled,
                result = tokio::time::timeout(request.timeout, wait_and_drain) => match result {
                    Ok(result) => WaitOutcome::Complete(result),
                    Err(_) => WaitOutcome::TimedOut,
                },
            }
        };

        let output = match outcome {
            WaitOutcome::Complete(result) => result?,
            WaitOutcome::Cancelled => {
                kill_and_reap(&mut *child, Duration::from_secs(5)).await;
                return Err(ToolError::Cancelled);
            }
            WaitOutcome::TimedOut => {
                kill_and_reap(&mut *child, Duration::from_secs(5)).await;
                return Err(ToolError::Sandbox {
                    tier: 3,
                    reason: aibo_core::error::SandboxFailure::Timeout,
                });
            }
        };

        let (status, stdout, stderr) = output;
        let out_cut = stdout.truncated;
        let err_cut = stderr.truncated;
        Ok(ShellOutcome {
            exit_code: status.code(),
            stdout: stdout.text,
            stderr: stderr.text,
            truncated: out_cut || err_cut,
            duration: started.elapsed(),
        })
    }
}

const fn platform_shell_identity() -> &'static str {
    if cfg!(windows) {
        "powershell.exe"
    } else {
        "/bin/sh"
    }
}

#[cfg(windows)]
fn platform_command(command: &str) -> tokio::process::Command {
    // Windows PowerShell otherwise inherits a legacy console code page when
    // stdout is redirected and reads BOM-less files using the system code page,
    // while aibo's tool protocol and workspace files are UTF-8. Configure both
    // PowerShell and common child runtimes before running the exact model command.
    // `-WindowStyle Hidden` complements CREATE_NO_WINDOW below so the host stays
    // invisible even if a Windows launcher changes the process creation context.
    // The trailing exit preserves native-command failures instead of reporting
    // every PowerShell process as successful.
    let script = format!(
        "$utf8 = New-Object System.Text.UTF8Encoding($false); \
         [Console]::InputEncoding = $utf8; \
         [Console]::OutputEncoding = $utf8; \
         $OutputEncoding = $utf8; \
         $PSDefaultParameterValues['*:Encoding'] = 'utf8'; \
         $global:LASTEXITCODE = 0;\n{command}\n\
         if (-not $?) {{ exit 1 }}; exit $LASTEXITCODE"
    );
    let mut process = tokio::process::Command::new("powershell.exe");
    process
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
        ])
        .arg(script)
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8");
    process
}

#[cfg(not(windows))]
fn platform_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("/bin/sh");
    process.arg("-c").arg(command);
    process
}

/// Terminate the process group/job and reap its direct child. The
/// `process-wrap` implementation also waits for every member visible to the
/// operating system, so a shell command cannot leave descendants running after
/// cancellation or timeout.
async fn kill_and_reap(child: &mut dyn ChildWrapper, timeout: Duration) {
    if let Err(error) = child.start_kill()
        && child.try_wait().ok().flatten().is_none()
    {
        tracing::warn!(%error, "could not terminate shell process tree");
    }
    if tokio::time::timeout(timeout, child.wait()).await.is_err() {
        tracing::warn!("shell process tree did not exit before the reap timeout");
    }
}

const TRUNCATION_MARKER: &str = "\n[truncated by aibo]";

#[derive(Debug)]
struct CapturedOutput {
    text: String,
    truncated: bool,
}

async fn drain_capture<R>(mut reader: R) -> std::io::Result<CapturedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::with_capacity(MAX_CAPTURE_BYTES);
    let mut buffer = [0_u8; 8 << 10];
    let mut truncated = false;

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(retained.len());
        let take = remaining.min(read);
        retained.extend_from_slice(&buffer[..take]);
        truncated |= take < read;
        // Keep draining after the capture fills so the child can never block
        // trying to write the bytes we deliberately decline to retain.
    }

    let mut text = String::from_utf8_lossy(&retained).into_owned();
    truncated |= text.len() > MAX_CAPTURE_BYTES;
    if truncated {
        let content_limit = MAX_CAPTURE_BYTES.saturating_sub(TRUNCATION_MARKER.len());
        truncate_utf8(&mut text, content_limit);
        text.push_str(TRUNCATION_MARKER);
    }
    Ok(CapturedOutput { text, truncated })
}

fn truncate_utf8(text: &mut String, limit: usize) {
    let mut boundary = limit.min(text.len());
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
}

// ---------------------------------------------------------------------------
// Filesystem operations
// ---------------------------------------------------------------------------

/// Read a UTF-8 file inside the scope.
pub fn read_file(scope: &Scope, path: &Path) -> ToolResult<String> {
    let resolved = scope.resolve_existing(path)?;
    let meta = std::fs::metadata(&resolved)?;
    if meta.len() as usize > MAX_READ_BYTES {
        return Err(invalid(
            "read_file",
            format!("file is {} bytes, limit {MAX_READ_BYTES}", meta.len()),
        ));
    }
    let bytes = std::fs::read(&resolved)?;
    String::from_utf8(bytes).map_err(|_| invalid("read_file", "file is not valid UTF-8"))
}

/// List a directory inside the scope.
///
/// Entries that are symlinks are labelled, because the next operation on one
/// may resolve outside the scope and be refused — the user should be able to
/// see why before it happens.
pub fn list_dir(scope: &Scope, path: &Path) -> ToolResult<Vec<DirEntry>> {
    let resolved = scope.resolve_existing(path)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&resolved)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: file_type.is_dir(),
            is_symlink: file_type.is_symlink(),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// One entry from [`list_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// File name.
    pub name: String,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Whether it is a symlink (which may resolve out of the scope).
    pub is_symlink: bool,
}

/// Write a file inside the scope, snapshotting it first when asked.
///
/// The snapshot is taken **before** the write and only after the path has been
/// resolved and contained — snapshotting a path that turns out to be denied
/// would copy a file the user never scoped in.
pub fn write_file(
    scope: &Scope,
    path: &Path,
    contents: &str,
    snapshots: Option<&mut SnapshotSet>,
) -> ToolResult<PathBuf> {
    let resolved = scope.resolve_for_create(path)?;
    if let Some(snapshots) = snapshots {
        snapshots.capture(&resolved)?;
    }
    std::fs::write(&resolved, contents)?;
    Ok(resolved)
}

/// Delete a file inside the scope, snapshotting it first when asked.
pub fn delete_file(
    scope: &Scope,
    path: &Path,
    snapshots: Option<&mut SnapshotSet>,
) -> ToolResult<PathBuf> {
    let resolved = scope.resolve_existing(path)?;
    if resolved.is_dir() {
        return Err(invalid(
            "delete_file",
            "refusing to delete a directory; this tool deletes single files",
        ));
    }
    if let Some(snapshots) = snapshots {
        snapshots.capture(&resolved)?;
    }
    std::fs::remove_file(&resolved)?;
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// What was recorded for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSnapshot {
    /// The file did not exist; reverting deletes it.
    Absent,
    /// The file's contents before the change.
    Contents(Vec<u8>),
    /// The file was too large to keep a before-image of. Reverting cannot
    /// restore it, and the UI must say so rather than implying it can.
    TooLarge {
        /// Size at capture time.
        bytes: u64,
    },
}

/// Before-images of files aibo is about to change.
///
/// # This is not transactionality
///
/// §11, verbatim in spirit: a before-image of files aibo knew about does not
/// cover commands that delete other files, follow links out of the scope,
/// mutate a database, change git state, or make network calls. The feature is
/// called "revert these file changes" and [`SnapshotSet::LIMITATIONS`] is the
/// text to show beside it.
#[derive(Debug, Clone, Default)]
pub struct SnapshotSet {
    entries: BTreeMap<PathBuf, FileSnapshot>,
}

impl SnapshotSet {
    /// The honest scope of the feature, for the UI.
    pub const LIMITATIONS: &'static str = "Reverts changes to files aibo wrote. \
It cannot undo commands that ran, files they deleted or created on their own, \
git operations, database changes, or anything sent over the network.";

    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the current state of a **already-resolved** path.
    ///
    /// The first capture for a path wins: reverting is meant to return to the
    /// state before the operation, not before the last of several writes.
    pub fn capture(&mut self, resolved: &Path) -> ToolResult<()> {
        if self.entries.contains_key(resolved) {
            return Ok(());
        }
        let snapshot = match std::fs::symlink_metadata(resolved) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileSnapshot::Absent,
            Err(e) => return Err(e.into()),
            Ok(meta) if meta.len() as usize > MAX_SNAPSHOT_BYTES => {
                FileSnapshot::TooLarge { bytes: meta.len() }
            }
            Ok(_) => FileSnapshot::Contents(std::fs::read(resolved)?),
        };
        self.entries.insert(resolved.to_path_buf(), snapshot);
        Ok(())
    }

    /// Paths with a recorded before-image.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.entries.keys().map(PathBuf::as_path)
    }

    /// Whether anything was captured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Restore every captured file.
    ///
    /// Never stops at the first failure: a partial revert that abandoned the
    /// remaining files would leave a state nobody chose. Failures are collected
    /// and reported.
    pub fn revert(&self) -> RevertReport {
        let mut report = RevertReport::default();
        for (path, snapshot) in &self.entries {
            let outcome = match snapshot {
                FileSnapshot::Contents(bytes) => {
                    std::fs::write(path, bytes).map(|()| Restored::Wrote)
                }
                FileSnapshot::Absent => match std::fs::remove_file(path) {
                    Ok(()) => Ok(Restored::Removed),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Restored::Removed),
                    Err(e) => Err(e),
                },
                FileSnapshot::TooLarge { bytes } => {
                    report.unrecoverable.push((
                        path.clone(),
                        format!("no before-image: file was {bytes} bytes at capture time"),
                    ));
                    continue;
                }
            };
            match outcome {
                Ok(Restored::Wrote) => report.restored.push(path.clone()),
                Ok(Restored::Removed) => report.removed.push(path.clone()),
                Err(e) => report.failed.push((path.clone(), e.to_string())),
            }
        }
        report
    }
}

enum Restored {
    Wrote,
    Removed,
}

/// What [`SnapshotSet::revert`] managed to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevertReport {
    /// Files whose previous contents were written back.
    pub restored: Vec<PathBuf>,
    /// Files that had not existed and were removed again.
    pub removed: Vec<PathBuf>,
    /// Files that could not be reverted, with the reason.
    pub failed: Vec<(PathBuf, String)>,
    /// Files with no usable before-image, with the reason.
    pub unrecoverable: Vec<(PathBuf, String)>,
}

impl RevertReport {
    /// Whether every captured file came back.
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty() && self.unrecoverable.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Commands the user pre-approved, so ordinary work does not re-prompt (§11
/// "allowlist rules").
///
/// Allowing a *program* still runs [`classify_command`]: `git` on the allowlist
/// does not silently authorise `git reset --hard`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandAllowlist {
    /// Exact command lines that need no prompt.
    pub exact: Vec<String>,
    /// Program names whose invocations need no prompt.
    pub programs: Vec<String>,
}

impl CommandAllowlist {
    /// Whether a command runs without asking.
    pub fn allows(&self, command: &str) -> bool {
        if matches!(classify_command(command), CommandRisk::TypedConfirmation(_)) {
            return false;
        }
        // A command that reaches the shell's own features is never covered by a
        // program allowlist: `git status; rm -rf ~` starts with `git`.
        if shell_metacharacters(command) {
            return self.exact.iter().any(|c| c == command);
        }
        if self.exact.iter().any(|c| c == command) {
            return true;
        }
        command
            .split_whitespace()
            .next()
            .is_some_and(|program| self.programs.iter().any(|p| p == program))
    }
}

/// Read-only filesystem access as a tier-3 tool.
#[derive(Debug, Clone)]
pub struct ReadFileTool {
    scope: Scope,
}

impl ReadFileTool {
    /// Bind the tool to a scope.
    pub const fn new(scope: Scope) -> Self {
        Self { scope }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".to_owned(),
            description: "Read a UTF-8 text file from a folder the user added.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            tier: 3,
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::ShellFs
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let path = PathBuf::from(str_arg(&args, "read_file", "path")?);
        let scope = self.scope.clone();
        let text = tokio::task::spawn_blocking(move || read_file(&scope, &path))
            .await
            .map_err(|e| ToolError::Failed {
                tool: "read_file".to_owned(),
                message: e.to_string(),
            })??;
        Ok(ToolOutput::text(text))
    }
}

/// Directory listing as a tier-3 tool.
#[derive(Debug, Clone)]
pub struct ListDirTool {
    scope: Scope,
}

impl ListDirTool {
    /// Bind the tool to a scope.
    pub const fn new(scope: Scope) -> Self {
        Self { scope }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_dir".to_owned(),
            description: "List a directory inside a folder the user added.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            tier: 3,
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::ShellFs
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let path = PathBuf::from(str_arg(&args, "list_dir", "path")?);
        let scope = self.scope.clone();
        let entries = tokio::task::spawn_blocking(move || list_dir(&scope, &path))
            .await
            .map_err(|e| ToolError::Failed {
                tool: "list_dir".to_owned(),
                message: e.to_string(),
            })??;
        let rendered = entries
            .iter()
            .map(|e| {
                let mut line = e.name.clone();
                if e.is_dir {
                    line.push('/');
                }
                if e.is_symlink {
                    line.push_str(" -> (symlink)");
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n");
        let structured = entries
            .iter()
            .map(|e| json!({ "name": e.name, "is_dir": e.is_dir, "is_symlink": e.is_symlink }))
            .collect::<Vec<_>>();
        Ok(ToolOutput::json(rendered, json!({ "entries": structured })))
    }
}

/// Shell execution as a tier-3 tool, restricted to the allowlist.
///
/// Anything not on the allowlist comes back as
/// [`DenyReason::NotApproved`] — the model cannot talk its way past the prompt,
/// because the prompt is not in this code path at all. The permission gate asks
/// the user and then calls [`ShellExecutor::run`] directly with a
/// [`CommandApproval`].
#[derive(Debug, Clone)]
pub struct ShellTool {
    executor: ShellExecutor,
    allowlist: Arc<CommandAllowlist>,
    default_cwd: PathBuf,
}

impl ShellTool {
    /// Build the tool. `default_cwd` is used when the model gives no `cwd`.
    pub fn new(executor: ShellExecutor, allowlist: CommandAllowlist, default_cwd: PathBuf) -> Self {
        Self {
            executor,
            allowlist: Arc::new(allowlist),
            default_cwd,
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "shell".to_owned(),
            description: "Run a shell command in a folder the user added. Only pre-approved \
                          commands run without asking."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" }
                },
                "required": ["command"]
            }),
            tier: 3,
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::ShellFs
    }

    async fn call(
        &self,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> ToolResult<ToolOutput> {
        let command = str_arg(&args, "shell", "command")?;
        let cwd = crate::args::opt_str(&args, "cwd")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_cwd.clone());

        if !self.allowlist.allows(command) {
            return Err(DenyReason::NotApproved {
                what: command.to_owned(),
            }
            .into());
        }

        let request = ShellRequest::new(command, cwd);
        // The allowlist *is* the approval for these commands, and it was given
        // by the user. `revalidate` still re-resolves the cwd and re-classifies
        // the command, so an allowlisted string that became dangerous — or a
        // directory that became a symlink — is still caught.
        let approval = CommandApproval::granted(&request);
        let outcome = self.executor.run(&request, &approval, cancel).await?;

        let text = if outcome.stderr.is_empty() {
            outcome.stdout.clone()
        } else {
            format!("{}\n[stderr]\n{}", outcome.stdout, outcome.stderr)
        };
        Ok(ToolOutput {
            text,
            structured: Some(json!({
                "exit_code": outcome.exit_code,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
                "truncated": outcome.truncated,
            })),
            is_error: !outcome.succeeded(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
        scope: Scope,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "classified").unwrap();
        std::fs::write(root.join("ok.txt"), "hello").unwrap();
        let scope = Scope::new([root.clone()]).unwrap();
        // Canonicalise for comparisons: on macOS the temp dir lives under a
        // symlinked /tmp, which is precisely the case that would break a naive
        // string comparison.
        let root = std::fs::canonicalize(&root).unwrap();
        let outside = std::fs::canonicalize(&outside).unwrap();
        Fixture {
            _tmp: tmp,
            root,
            outside,
            scope,
        }
    }

    // -- containment --------------------------------------------------------

    #[test]
    fn a_path_inside_the_scope_resolves() {
        let f = fixture();
        let resolved = f.scope.resolve_existing(&f.root.join("ok.txt")).unwrap();
        assert!(f.scope.contains(&resolved));
    }

    #[test]
    fn dot_dot_traversal_is_denied() {
        let f = fixture();
        let err = f
            .scope
            .resolve_existing(&f.root.join("../outside/secret.txt"))
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_scope_fails_closed() {
        let f = fixture();
        std::os::unix::fs::symlink(&f.outside, f.root.join("escape")).unwrap();

        // The link itself.
        let err = f
            .scope
            .resolve_existing(&f.root.join("escape"))
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied(DenyReason::SymlinkEscape { .. })),
            "resolving the link itself must be denied, got {err:?}"
        );

        // A file reached *through* the link — the case a pre-resolution check
        // would wave through, because the string starts with the root.
        let err = f
            .scope
            .resolve_existing(&f.root.join("escape/secret.txt"))
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied(DenyReason::SymlinkEscape { .. })),
            "traversal through the link must be denied, got {err:?}"
        );

        // And the high-level read must fail the same way, not just the
        // low-level resolver.
        let err = read_file(&f.scope, &f.root.join("escape/secret.txt")).unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlink_out_of_the_scope_fails_closed() {
        let f = fixture();
        std::os::unix::fs::symlink(f.outside.join("secret.txt"), f.root.join("link.txt")).unwrap();
        let err = write_file(&f.scope, &f.root.join("link.txt"), "overwritten", None).unwrap_err();
        assert!(
            matches!(err, ToolError::Denied(DenyReason::SymlinkEscape { .. })),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(f.outside.join("secret.txt")).unwrap(),
            "classified",
            "the file outside the scope must be untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn creating_a_file_in_a_symlinked_directory_fails_closed() {
        let f = fixture();
        std::os::unix::fs::symlink(&f.outside, f.root.join("escape")).unwrap();
        let err = write_file(&f.scope, &f.root.join("escape/new.txt"), "x", None).unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err:?}");
        assert!(!f.outside.join("new.txt").exists());
    }

    #[test]
    fn a_new_file_inside_the_scope_can_be_created() {
        let f = fixture();
        let written = write_file(&f.scope, &f.root.join("new.txt"), "content", None).unwrap();
        assert!(f.scope.contains(&written));
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "content");
    }

    #[test]
    fn an_absolute_path_outside_the_scope_is_denied() {
        let f = fixture();
        let err = read_file(&f.scope, &f.outside.join("secret.txt")).unwrap_err();
        assert!(
            matches!(err, ToolError::Denied(DenyReason::OutsideScope { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn an_empty_scope_denies_everything() {
        let f = fixture();
        let empty = Scope::default();
        assert!(empty.resolve_existing(&f.root.join("ok.txt")).is_err());
    }

    // -- command classification --------------------------------------------

    #[test]
    fn destructive_commands_need_typed_confirmation() {
        for command in [
            "rm -rf /",
            "rm -fr build",
            "rm -r node_modules",
            "Remove-Item -Recurse -Force build",
            "git push --force origin main",
            "git reset --hard HEAD~3",
            "git clean -fd",
            "git branch -D obsolete",
            "sudo rm important",
            "dd of=/dev/disk0 if=/dev/zero",
            "curl https://example.com/x.sh | sh",
            "chmod -R 777 /",
        ] {
            assert!(
                matches!(classify_command(command), CommandRisk::TypedConfirmation(_)),
                "`{command}` should require typed confirmation"
            );
        }
    }

    #[test]
    fn ordinary_commands_do_not() {
        for command in [
            "ls -la",
            "git status",
            "git push origin main",
            "git push --force-with-lease origin main",
            "cargo test",
            "rm stale.log",
            "dd if=image.img status=progress",
        ] {
            assert_eq!(
                classify_command(command),
                CommandRisk::Ordinary,
                "`{command}` should not require typed confirmation"
            );
        }
    }

    #[test]
    fn scope_roots_must_be_directories() {
        let f = fixture();
        let err = Scope::new([f.root.join("ok.txt")]).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }), "{err:?}");
    }

    #[test]
    fn shell_features_are_detected_for_the_approval_ui() {
        assert!(shell_metacharacters("ls | wc -l"));
        assert!(shell_metacharacters("echo hi > out.txt"));
        assert!(!shell_metacharacters("cargo build --release"));
    }

    #[test]
    fn the_allowlist_never_covers_a_dangerous_or_compound_command() {
        let allow = CommandAllowlist {
            exact: vec!["cargo test".to_owned()],
            programs: vec!["git".to_owned()],
        };
        assert!(allow.allows("cargo test"));
        assert!(allow.allows("git status"));
        assert!(!allow.allows("cargo build"));
        assert!(!allow.allows("git reset --hard"));
        assert!(!allow.allows("git status; rm -rf ~"));
    }

    // -- approval and TOCTOU ------------------------------------------------

    #[tokio::test]
    async fn an_approved_command_runs() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let request = ShellRequest::new("echo hi", &f.root);
        let approval = CommandApproval::granted(&request);
        let outcome = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap();
        assert!(outcome.succeeded());
        assert!(outcome.stdout.contains("hi"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_output_is_utf8_and_preserves_unicode() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let request = ShellRequest::new("Write-Output '日本語 ✓ →'", &f.root);
        let approval = CommandApproval::granted(&request);
        let outcome = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap();

        assert!(outcome.succeeded(), "{}", outcome.stderr);
        assert_eq!(outcome.stdout.trim(), "日本語 ✓ →");
        assert!(!outcome.stdout.contains('\u{fffd}'));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_reads_bomless_utf8_files_by_default() {
        let f = fixture();
        std::fs::write(f.root.join("utf8.txt"), "日本語 ✓ →").expect("write fixture");
        let exec = ShellExecutor::new(f.scope.clone());
        let request = ShellRequest::new("Get-Content -Raw -LiteralPath 'utf8.txt'", &f.root);
        let approval = CommandApproval::granted(&request);
        let outcome = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap();

        assert!(outcome.succeeded(), "{}", outcome.stderr);
        assert_eq!(outcome.stdout.trim(), "日本語 ✓ →");
        assert!(!outcome.stdout.contains('\u{fffd}'));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_process_has_no_window() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let request = ShellRequest::new("(Get-Process -Id $PID).MainWindowHandle", &f.root);
        let approval = CommandApproval::granted(&request);
        let outcome = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap();

        assert!(outcome.succeeded(), "{}", outcome.stderr);
        assert_eq!(outcome.stdout.trim(), "0");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_errors_produce_a_failed_outcome() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let request = ShellRequest::new("Write-Error 'expected failure'", &f.root);
        let approval = CommandApproval::granted(&request);
        let outcome = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap();

        assert!(!outcome.succeeded());
        assert!(outcome.stderr.contains("expected failure"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn noisy_commands_are_drained_without_unbounded_capture_or_pipe_blockage() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let request = ShellRequest::new(
            "yes o | head -c 1048576; yes e | head -c 1048576 >&2",
            &f.root,
        );
        let approval = CommandApproval::granted(&request);
        let outcome = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap();

        assert!(outcome.succeeded());
        assert!(outcome.truncated);
        assert!(outcome.stdout.len() <= MAX_CAPTURE_BYTES);
        assert!(outcome.stderr.len() <= MAX_CAPTURE_BYTES);
        assert!(outcome.stdout.ends_with(TRUNCATION_MARKER));
        assert!(outcome.stderr.ends_with(TRUNCATION_MARKER));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_is_written_while_output_is_drained() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let mut request = ShellRequest::new("head -c 1048576 /dev/zero; wc -c", &f.root);
        request.stdin = Some("x".repeat(512 << 10));
        request.timeout = Duration::from_secs(5);
        let approval = CommandApproval::granted(&request);

        let outcome = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap();

        assert!(outcome.succeeded(), "{}", outcome.stderr);
        assert!(outcome.truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_includes_a_blocked_stdin_write() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let mut request = ShellRequest::new("sleep 30", &f.root);
        request.stdin = Some("x".repeat(512 << 10));
        request.timeout = Duration::from_millis(200);
        let approval = CommandApproval::granted(&request);
        let started = Instant::now();

        let err = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Sandbox { tier: 3, .. }), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_interrupts_a_blocked_stdin_write() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let mut request = ShellRequest::new("sleep 30", &f.root);
        request.stdin = Some("x".repeat(512 << 10));
        let approval = CommandApproval::granted(&request);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });

        let err = exec.run(&request, &approval, cancel).await.unwrap_err();

        assert!(matches!(err, ToolError::Cancelled), "{err:?}");
    }

    #[tokio::test]
    async fn oversized_stdin_is_rejected_before_spawning() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let marker = f.root.join("must-not-exist");
        let mut request = ShellRequest::new(format!("touch {}", marker.display()), &f.root);
        request.stdin = Some("x".repeat(MAX_STDIN_BYTES + 1));
        let approval = CommandApproval::granted(&request);

        let err = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::InvalidArguments { .. }), "{err:?}");
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn an_approval_for_a_different_command_is_refused() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let approved = ShellRequest::new("echo one", &f.root);
        let approval = CommandApproval::granted(&approved);
        let swapped = ShellRequest::new("echo two", &f.root);
        let err = exec
            .run(&swapped, &approval, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied(DenyReason::Stale { .. })),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_dangerous_command_without_a_typed_confirmation_is_refused() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let request = ShellRequest::new("rm -rf build", &f.root);
        let approval = CommandApproval::granted(&request);
        let err = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ToolError::Denied(DenyReason::NeedsTypedConfirmation { .. })
            ),
            "{err:?}"
        );

        // The same command with the exact text typed back does pass the gate.
        let typed = CommandApproval::typed(&request, "rm -rf build");
        assert!(exec.revalidate(&request, &typed).is_ok());

        // A near-miss does not.
        let wrong = CommandApproval::typed(&request, "rm -rf buil");
        assert!(exec.revalidate(&request, &wrong).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cwd_swapped_for_a_symlink_after_approval_is_caught_at_execution() {
        let f = fixture();
        let work = f.root.join("work");
        std::fs::create_dir(&work).unwrap();
        let exec = ShellExecutor::new(f.scope.clone());
        let request = ShellRequest::new("echo hi", &work);
        let approval = CommandApproval::granted(&request);

        // Approval was valid at the time.
        assert!(exec.revalidate(&request, &approval).is_ok());

        // Now the directory becomes a link out of the scope — the TOCTOU race
        // §11 names. Re-validation must catch it.
        std::fs::remove_dir(&work).unwrap();
        std::os::unix::fs::symlink(&f.outside, &work).unwrap();
        let err = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_overruns_its_timeout_is_stopped() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let mut request = ShellRequest::new("sleep 30", &f.root);
        request.timeout = Duration::from_millis(200);
        let approval = CommandApproval::granted(&request);
        let started = Instant::now();
        let err = exec
            .run(&request, &approval, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Sandbox { tier: 3, .. }), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_and_reaps_a_running_process_tree() {
        let f = fixture();
        let exec = ShellExecutor::new(f.scope.clone());
        let pid_file = f.root.join("grandchild.pid");
        let request = ShellRequest::new(
            format!("sleep 30 & echo $! > {}; wait", pid_file.display()),
            &f.root,
        );
        let approval = CommandApproval::granted(&request);
        let cancel = CancellationToken::new();
        let probe = cancel.clone();
        let probe_file = pid_file.clone();
        tokio::spawn(async move {
            for _ in 0..100 {
                if probe_file.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            probe.cancel();
        });
        let err = exec.run(&request, &approval, cancel).await.unwrap_err();
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
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("grandchild {pid} survived shell cancellation");
    }

    // -- snapshots ----------------------------------------------------------

    #[test]
    fn a_modified_file_is_restored() {
        let f = fixture();
        let mut snapshots = SnapshotSet::new();
        let path = f.root.join("ok.txt");
        write_file(&f.scope, &path, "changed", Some(&mut snapshots)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed");

        let report = snapshots.revert();
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn a_created_file_is_removed_again() {
        let f = fixture();
        let mut snapshots = SnapshotSet::new();
        let path = f.root.join("created.txt");
        write_file(&f.scope, &path, "new", Some(&mut snapshots)).unwrap();
        assert!(path.exists());

        let report = snapshots.revert();
        assert!(report.is_complete(), "{report:?}");
        assert!(!path.exists());
    }

    #[test]
    fn a_deleted_file_comes_back() {
        let f = fixture();
        let mut snapshots = SnapshotSet::new();
        let path = f.root.join("ok.txt");
        delete_file(&f.scope, &path, Some(&mut snapshots)).unwrap();
        assert!(!path.exists());
        snapshots.revert();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn the_first_capture_wins_so_revert_returns_to_the_start() {
        let f = fixture();
        let mut snapshots = SnapshotSet::new();
        let path = f.root.join("ok.txt");
        write_file(&f.scope, &path, "first", Some(&mut snapshots)).unwrap();
        write_file(&f.scope, &path, "second", Some(&mut snapshots)).unwrap();
        snapshots.revert();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn revert_reports_rather_than_pretending_when_it_cannot_restore() {
        let f = fixture();
        let mut snapshots = SnapshotSet::new();
        let path = f.root.join("ok.txt");
        snapshots.capture(&path).unwrap();
        // Simulate the file having been unreadable at capture time.
        snapshots
            .entries
            .insert(path.clone(), FileSnapshot::TooLarge { bytes: 1 << 30 });
        let report = snapshots.revert();
        assert!(!report.is_complete());
        assert_eq!(report.unrecoverable.len(), 1);
    }

    #[test]
    fn the_limitations_string_says_what_revert_cannot_do() {
        // §11: never present this as an undo for the whole operation.
        let text = SnapshotSet::LIMITATIONS;
        for claim in ["commands that ran", "git", "network"] {
            assert!(
                text.contains(claim),
                "limitations copy must mention {claim}"
            );
        }
        // The word "undo" may appear, but only inside a disclaimer. §11: never
        // present this as an undo for the whole operation.
        let lowered = text.to_lowercase();
        for (i, _) in lowered.match_indices("undo") {
            assert!(
                lowered[..i].ends_with("cannot "),
                "`undo` must only ever follow `cannot` in the revert copy"
            );
        }
    }

    // -- tools --------------------------------------------------------------

    #[tokio::test]
    async fn the_read_tool_refuses_a_path_outside_the_scope() {
        let f = fixture();
        let tool = ReadFileTool::new(f.scope.clone());
        let out = tool
            .call(
                json!({ "path": f.outside.join("secret.txt").to_str().unwrap() }),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(out, Err(ToolError::Denied(_))));

        let ok = tool
            .call(
                json!({ "path": f.root.join("ok.txt").to_str().unwrap() }),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(ok.text, "hello");
        assert!(!ok.origin().may_authorise_tools());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_list_tool_marks_symlinks() {
        let f = fixture();
        std::os::unix::fs::symlink(&f.outside, f.root.join("escape")).unwrap();
        let tool = ListDirTool::new(f.scope.clone());
        let out = tool
            .call(
                json!({ "path": f.root.to_str().unwrap() }),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.text.contains("escape -> (symlink)"), "{}", out.text);
    }

    #[tokio::test]
    async fn the_shell_tool_refuses_anything_not_allowlisted() {
        let f = fixture();
        let tool = ShellTool::new(
            ShellExecutor::new(f.scope.clone()),
            CommandAllowlist {
                exact: vec![],
                programs: vec!["echo".to_owned()],
            },
            f.root.clone(),
        );
        let err = tool
            .call(
                json!({ "command": "cat /etc/passwd" }),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied(DenyReason::NotApproved { .. })),
            "{err:?}"
        );

        let ok = tool
            .call(
                json!({ "command": "echo allowed" }),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(ok.text.contains("allowed"));
    }
}
