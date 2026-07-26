//! `aibo` — the binary.
//!
//! §6 makes this deliberately thin: *"wire everything, install the panic/crash
//! handler"*. Everything with behaviour lives in `crates/`. What is here is the
//! set of concerns only the process entry point can own:
//!
//! | Concern | §6 requirement | Where |
//! |---|---|---|
//! | Panic strategy | unwind, catch at every task boundary, redacted ring buffer | [`diagnostics`] |
//! | Single instance | lock file + liveness check | [`instance`] |
//! | Orphaned children | reap on startup *and* shutdown | [`children`] |
//! | Atomic config writes | temp file + rename | [`paths::atomic_write`] |
//! | Two schedulers | tokio multi-thread runtime, iced daemon on the main thread | [`main`] |
//!
//! ## Threading
//!
//! §6's diagram is load-bearing and this file is where it becomes real. The
//! **main thread** runs the iced/winit event loop and nothing else: winit
//! requires it, `tray-icon` requires it on macOS, and `NSScreen` is
//! main-thread-only. Every await point in the product runs on a tokio
//! multi-thread runtime built here and *entered* before the loop starts, so the
//! `Subscription`s and `Task`s iced creates have a reactor to attach to.
//!
//! The two halves talk over the [`aibo_ui::bridge`] vocabulary: an unbounded
//! `UiRequest` sender lives in UI state, an unbounded `UiEvent` receiver is
//! drained by an iced `Subscription`. Neither side ever blocks on the other.
//!
//! ## Inference dispatch
//!
//! [`aibo_session::Engine`] owns it: §1 surface inference, §4 routing and chain
//! fallback, §5 prompt assembly and the context budget, §7 streaming, §13's
//! per-provider offline hysteresis and cancellation, §14's reserve-then-
//! reconcile, and §12 persistence. This file's job is to *build* one — resolve
//! the config, the credentials, the price table and the database — and then
//! translate its [`aibo_session::SessionEvent`]s into `aibo_ui::UiEvent`s.
//!
//! Everything the engine needs is allowed to fail softly. A missing config, a
//! locked keychain and an unopenable database each degrade one capability and
//! none of them stop the tray from appearing: §13's only blocking error is
//! `NoProviderConfigured`, and that one already has a designed treatment.

// The binary is allowed no `unsafe` at all. Every platform API that needs it is
// isolated inside `aibo-platform` (§7).
#![forbid(unsafe_code)]

use anyhow::Context as _;

/// How long the tokio runtime is given to finish in-flight work at shutdown.
///
/// Long enough for a cancelled provider stream to unwind and a SQLite write to
/// commit; short enough that quitting feels immediate.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

fn main() -> anyhow::Result<()> {
    diagnostics::init_tracing();

    let paths = paths::Paths::resolve().context("could not resolve aibo's data directory")?;

    // §6: "show 'aibo restarted after an error' with a diagnostics link on the
    // next launch". The marker is written by the panic hook and consumed here,
    // before the hook for *this* run is installed.
    let recovered_from_crash = diagnostics::install_panic_hook(&paths);

    // §6 single instance. Two aibos fighting over one hotkey, one database and
    // one `codex` subprocess is a support nightmare.
    let _instance = match instance::acquire(&paths)? {
        instance::Outcome::Acquired(guard) => guard,
        instance::Outcome::AlreadyRunning { pid } => {
            // TODO(§6): "a second launch focuses the existing panel rather than
            // starting". That needs an IPC rendezvous the running instance
            // listens on — a Unix domain socket beside the lock file on macOS,
            // a named pipe on Windows. Until it exists we exit cleanly rather
            // than racing for the hotkey, which is the safe half of the rule.
            tracing::warn!(pid, "aibo is already running; exiting");
            eprintln!("aibo is already running (pid {pid}).");
            return Ok(());
        }
    };

    // §6: "Orphaned child cleanup on startup as well as shutdown — if aibo was
    // force-quit, a `codex app-server` or MCP server may still be running."
    let children = children::Registry::open(&paths);
    let reaped = children.reap_orphans();
    if reaped > 0 {
        tracing::info!(reaped, "terminated orphans left by a previous run");
    }

    // §6: the tokio half. `enable_all` because provider streams need the I/O
    // driver and every deadline in §8 needs the time driver.
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("aibo-worker")
        .build()
        .context("could not start the tokio runtime")?;

    let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();

    if recovered_from_crash {
        let _ = events_tx.send(aibo_ui::UiEvent::RecoveredFromCrash);
    }

    let ui_result = {
        // The enter guard must be dropped before `tokio_runtime`, hence the
        // block: dropping a runtime from inside its own context panics.
        let _enter = tokio_runtime.enter();

        let backend = runtime::Backend::new(paths.clone(), children.clone(), events_tx);

        // Every long-lived task goes through `supervise`: §6 requires a panic to
        // be caught at the task boundary so the tray survives it.
        tokio_runtime.spawn(diagnostics::supervise("backend", backend.run(requests_rx)));

        // iced owns the main thread from here until the user quits.
        aibo_ui::run(
            aibo_ui::UiConfig::default(),
            aibo_ui::UiHandles {
                requests: requests_tx,
                events: events_rx,
            },
        )
    };

    // §6: children must not outlive aibo. This is the orderly half; the startup
    // reap above covers the force-quit half.
    children.terminate_all();
    tokio_runtime.shutdown_timeout(SHUTDOWN_GRACE);

    ui_result.context("the aibo shell exited abnormally")
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/// Where aibo keeps its database, lock file and crash marker.
mod paths {
    use std::io;
    use std::path::{Path, PathBuf};

    /// Resolved per-user locations. Cheap to clone; every path is absolute.
    #[derive(Debug, Clone)]
    pub struct Paths {
        root: PathBuf,
    }

    impl Paths {
        /// Resolve the per-user root, creating it if necessary.
        ///
        /// `AIBO_HOME` overrides everything, which is what the eval harness and
        /// the spikes need in order not to touch a real install.
        pub fn resolve() -> io::Result<Self> {
            let root = match std::env::var_os("AIBO_HOME") {
                Some(dir) => PathBuf::from(dir),
                None => platform_root()?,
            };
            std::fs::create_dir_all(&root)?;
            Ok(Self { root })
        }

        /// A `Paths` rooted at an explicit directory.
        ///
        /// For tests, which must not mutate the process environment to pick a
        /// directory — `AIBO_HOME` is global and `cargo test` is threaded.
        #[cfg(test)]
        pub fn for_root(root: PathBuf) -> Self {
            Self { root }
        }

        /// The root directory itself.
        #[allow(dead_code)] // Kept for the settings UI's "reveal in Finder".
        pub fn root(&self) -> &Path {
            &self.root
        }

        /// The SQLCipher database (§12).
        pub fn database(&self) -> PathBuf {
            self.root.join("aibo.db")
        }

        /// The single-instance lock file (§6).
        pub fn lock(&self) -> PathBuf {
            self.root.join("aibo.lock")
        }

        /// Written by the panic hook, consumed on the next launch (§6).
        pub fn crash_marker(&self) -> PathBuf {
            self.root.join("crashed")
        }

        /// pid ledger for orphan cleanup (§6).
        pub fn children(&self) -> PathBuf {
            self.root.join("children.pids")
        }

        /// The user's TOML configuration. Written via [`atomic_write`].
        pub fn config(&self) -> PathBuf {
            self.root.join("config.toml")
        }

        /// The user's price-table overlay (§14: prices change faster than
        /// releases, so the table ships as TOML and the user may correct it).
        pub fn prices(&self) -> PathBuf {
            self.root.join("prices.toml")
        }
    }

    #[cfg(target_os = "macos")]
    fn platform_root() -> io::Result<PathBuf> {
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("aibo"))
    }

    #[cfg(target_os = "windows")]
    fn platform_root() -> io::Result<PathBuf> {
        let appdata =
            std::env::var_os("APPDATA").ok_or_else(|| io::Error::other("APPDATA is not set"))?;
        Ok(PathBuf::from(appdata).join("aibo"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn platform_root() -> io::Result<PathBuf> {
        // Not a shipping target (§2 locks macOS and Windows), but keeping the
        // binary buildable on Linux keeps CI and `cargo check` honest.
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .ok_or_else(|| io::Error::other("neither XDG_DATA_HOME nor HOME is set"))?;
        Ok(base.join("aibo"))
    }

    /// §6: *"write to a temp file and rename, so a crash mid-write doesn't leave
    /// unparseable TOML and a dead app on next launch."*
    ///
    /// `rename` within a directory is atomic on APFS/HFS+ and NTFS alike, so a
    /// reader sees either the old file or the new one, never a partial write.
    pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("path has no parent directory"))?;
        std::fs::create_dir_all(parent)?;

        let temp = parent.join(format!(
            ".{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("aibo")
        ));

        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(bytes)?;
            // The rename is only crash-atomic if the bytes reached the disk
            // first; without this the metadata operation can win the race.
            file.sync_all()?;
        }

        std::fs::rename(&temp, path)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn atomic_write_replaces_in_place() {
            let dir = std::env::temp_dir().join("aibo-atomic-write-test");
            let _ = std::fs::remove_dir_all(&dir);
            let target = dir.join("config.toml");

            atomic_write(&target, b"first").unwrap();
            atomic_write(&target, b"second").unwrap();

            assert_eq!(std::fs::read(&target).unwrap(), b"second");
            // The temp file must not survive a successful write.
            assert!(!dir.join(".config.toml.tmp").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// ---------------------------------------------------------------------------
// diagnostics
// ---------------------------------------------------------------------------

/// §6's panic strategy: unwind, catch at every task boundary, log to a redacted
/// ring buffer, keep the tray alive.
mod diagnostics {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::panic::AssertUnwindSafe;
    use std::sync::{Mutex, OnceLock};
    use std::time::SystemTime;

    use futures::FutureExt as _;

    use crate::paths::Paths;

    /// How many records the ring buffer keeps.
    ///
    /// Bounded on purpose: §15 budgets idle RSS, and an unbounded diagnostic log
    /// in a process that runs for weeks is a leak with a nice name.
    const RING_CAPACITY: usize = 256;

    /// One redacted diagnostic record.
    ///
    /// The fields are written by [`record`] and read by [`snapshot`]; the
    /// consumer of `snapshot` is `UiRequest::CopyDiagnostics`, which is not
    /// wired yet — hence the allow.
    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    pub struct Record {
        /// When it happened.
        pub at: SystemTime,
        /// `panic`, `task-panic`, …
        pub kind: &'static str,
        /// Already redacted. Never contains captured text or a credential.
        pub message: String,
        /// `file:line:col`, when the runtime supplied one.
        pub location: Option<String>,
    }

    fn ring() -> &'static Mutex<VecDeque<Record>> {
        static RING: OnceLock<Mutex<VecDeque<Record>>> = OnceLock::new();
        RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAPACITY)))
    }

    /// Push a record, evicting the oldest when full.
    pub fn record(kind: &'static str, message: String, location: Option<String>) {
        // A poisoned diagnostics mutex must not itself panic — that would turn
        // one caught panic into an uncatchable one.
        let Ok(mut buffer) = ring().lock() else {
            return;
        };
        if buffer.len() == RING_CAPACITY {
            buffer.pop_front();
        }
        buffer.push_back(Record {
            at: SystemTime::now(),
            kind,
            message,
            location,
        });
    }

    /// The ring buffer's contents, oldest first.
    ///
    /// This is what `UiRequest::CopyDiagnostics` (§13, §19) hands the user.
    // TODO(P1): wire to `UiRequest::CopyDiagnostics`, together with the config
    // and provider inventory §19 wants in the bundle.
    #[allow(dead_code)]
    pub fn snapshot() -> Vec<Record> {
        ring()
            .lock()
            .map(|buffer| buffer.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Install the tracing subscriber.
    ///
    /// `AIBO_LOG` uses `RUST_LOG` syntax. The default is deliberately quiet:
    /// the ring buffer, not stderr, is the diagnostic channel for a tray app
    /// with no console.
    pub fn init_tracing() {
        use tracing_subscriber::EnvFilter;

        let filter = EnvFilter::try_from_env("AIBO_LOG")
            .unwrap_or_else(|_| EnvFilter::new("aibo=info,warn"));

        // A second call (tests, a re-exec) must not abort the process.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .try_init();
    }

    /// Install the §6 panic hook and report whether the *previous* run crashed.
    ///
    /// The hook does three things and deliberately does not abort: it appends to
    /// the ring buffer, it logs, and it drops a marker file so the next launch
    /// can show "aibo restarted after an error".
    pub fn install_panic_hook(paths: &Paths) -> bool {
        let marker = paths.crash_marker();
        let previous_run_crashed = marker.exists();
        if previous_run_crashed {
            let _ = std::fs::remove_file(&marker);
        }

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = redact_payload(info.payload());
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));

            record("panic", message.clone(), location.clone());
            tracing::error!(location = ?location, "panic: {message}");

            // Best effort: if this write fails the process is in bad enough
            // shape that there is nothing useful left to do about it.
            let _ = crate::paths::atomic_write(
                &marker,
                format!(
                    "{message}\n{}\n",
                    location.as_deref().unwrap_or("<unknown>")
                )
                .as_bytes(),
            );

            // Still print the standard backtrace when someone is watching.
            default_hook(info);
        }));

        previous_run_crashed
    }

    /// Wrap a long-lived task so a panic inside it is caught at the boundary
    /// rather than unwinding out of a tokio worker (§6).
    ///
    /// The hook has already recorded the details by the time this sees the
    /// unwind; what this adds is *survival* — the tray, the hotkey and the other
    /// tasks keep running, which is the whole reason §15's release profile is
    /// `panic = "unwind"`.
    pub async fn supervise<F>(label: &'static str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if AssertUnwindSafe(future).catch_unwind().await.is_err() {
            record("task-panic", format!("task `{label}` panicked"), None);
            tracing::error!(task = label, "task panicked; the shell continues");
        }
    }

    /// Turn a panic payload into something safe to keep.
    ///
    /// The distinction *is* the "redacted" in §6:
    ///
    /// * `&'static str` payloads come from `panic!("literal")` — compile-time
    ///   constants that cannot contain user data.
    /// * `String` payloads come from `panic!("{}", x)`, an `unwrap` on an error,
    ///   or an assertion — any of which can embed a captured selection, a prompt
    ///   or an API key. Only the length is kept.
    fn redact_payload(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(literal) = payload.downcast_ref::<&'static str>() {
            redact_home(literal)
        } else if let Some(formatted) = payload.downcast_ref::<String>() {
            format!(
                "<redacted runtime-formatted panic message, {} bytes>",
                formatted.len()
            )
        } else {
            "<non-string panic payload>".to_owned()
        }
    }

    /// Replace the user's home directory with `~`, so a path baked into a
    /// literal cannot leak their account name into a diagnostics bundle.
    fn redact_home(text: &str) -> String {
        match std::env::var("HOME").ok().filter(|h| !h.is_empty()) {
            Some(home) => text.replace(&home, "~"),
            None => text.to_owned(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn formatted_payloads_are_redacted_to_a_length() {
            let secret = String::from("sk-live-0123456789 and the user's selected text");
            let redacted = redact_payload(&secret);
            assert!(!redacted.contains("sk-live"));
            assert!(redacted.contains(&secret.len().to_string()));
        }

        #[test]
        fn literal_payloads_survive() {
            let literal: &'static str = "router: unreachable rule";
            assert_eq!(redact_payload(&literal), literal);
        }

        #[test]
        fn ring_buffer_is_bounded() {
            for i in 0..RING_CAPACITY + 32 {
                record("test", format!("{i}"), None);
            }
            assert!(snapshot().len() <= RING_CAPACITY);
        }

        #[tokio::test]
        async fn a_panicking_task_does_not_take_the_process_with_it() {
            supervise("deliberate", async { panic!("deliberate") }).await;
            assert!(
                snapshot()
                    .iter()
                    .any(|r| r.kind == "task-panic" && r.message.contains("deliberate"))
            );
        }
    }
}

// ---------------------------------------------------------------------------
// instance
// ---------------------------------------------------------------------------

/// §6 single-instance: *"Named mutex on Windows, a lock file with a liveness
/// check on macOS."*
///
/// One implementation serves both. A named mutex is the more idiomatic Windows
/// primitive, but it answers a strictly weaker question — "is something holding
/// this name?" — and still needs a liveness story for the force-quit case. The
/// lock file carries the pid *and* the executable name, so a recycled pid does
/// not lock the user out of their own app.
mod instance {
    use std::path::PathBuf;

    use crate::paths::Paths;

    /// Holds the lock for the process lifetime and removes it on drop.
    ///
    /// Drop is best effort — `SIGKILL` leaves the file behind, which is exactly
    /// why [`acquire`] does a liveness check and not a bare `path.exists()`.
    #[derive(Debug)]
    pub struct Guard {
        path: PathBuf,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// What [`acquire`] found.
    #[derive(Debug)]
    pub enum Outcome {
        /// This process now owns the lock.
        Acquired(Guard),
        /// A live aibo already owns it.
        AlreadyRunning {
            /// The owning process id, for the log line.
            pid: u32,
        },
    }

    /// Take the single-instance lock.
    pub fn acquire(paths: &Paths) -> anyhow::Result<Outcome> {
        let path = paths.lock();

        if let Ok(contents) = std::fs::read_to_string(&path)
            && let Some((pid, name)) = parse(&contents)
            && crate::children::process_matches(pid, &name)
        {
            return Ok(Outcome::AlreadyRunning { pid });
        }

        let me = std::process::id();
        let name = executable_name();
        crate::paths::atomic_write(&path, format!("{me}\n{name}\n").as_bytes())?;

        Ok(Outcome::Acquired(Guard { path }))
    }

    fn parse(contents: &str) -> Option<(u32, String)> {
        let mut lines = contents.lines();
        let pid = lines.next()?.trim().parse().ok()?;
        let name = lines.next().unwrap_or("aibo").trim().to_owned();
        Some((pid, name))
    }

    /// The current executable's file name, used as the lock's identity token.
    fn executable_name() -> String {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "aibo".to_owned())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_lock_file_naming_a_dead_pid_is_reclaimed() {
            // pid 0 is not a live process on either platform, so the liveness
            // check must let us take the lock rather than refusing to start.
            assert!(parse("0\naibo\n").is_some());
            assert!(!crate::children::process_matches(0, "aibo"));
        }

        #[test]
        fn a_truncated_lock_file_is_not_fatal() {
            assert!(parse("").is_none());
            assert!(parse("not-a-pid\n").is_none());
            assert_eq!(parse("42\n").unwrap(), (42, "aibo".to_owned()));
        }
    }
}

// ---------------------------------------------------------------------------
// children
// ---------------------------------------------------------------------------

/// §6: *"Child processes must not outlive aibo."*
///
/// `aibo-agent` already spawns `codex app-server` into its own process group
/// with `kill_on_drop`, and `aibo-tools` does the same for shell commands —
/// which handles the *orderly* case. Neither survives a `SIGKILL` of aibo
/// itself, and §6 names that case explicitly. This ledger is the answer: pids
/// are written to disk as they are spawned and swept on the next launch.
mod children {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use crate::paths::Paths;

    /// A recorded child.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Entry {
        pid: u32,
        /// A substring expected in the process's command line. This is what
        /// makes the sweep safe against pid reuse: aibo must never signal a
        /// process it did not spawn.
        token: String,
    }

    /// The on-disk pid ledger. Cheap to clone; all clones share one file.
    #[derive(Debug, Clone)]
    pub struct Registry {
        path: Arc<PathBuf>,
        live: Arc<Mutex<Vec<Entry>>>,
    }

    impl Registry {
        /// Open (but do not read) the ledger for this install.
        pub fn open(paths: &Paths) -> Self {
            Self {
                path: Arc::new(paths.children()),
                live: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Terminate anything left over from a previous run and clear the
        /// ledger; returns how many processes were signalled.
        ///
        /// Called once, before the runtime starts, so a force-quit does not
        /// leave a `codex app-server` holding the user's files or their API
        /// quota (§6).
        pub fn reap_orphans(&self) -> usize {
            let Ok(contents) = std::fs::read_to_string(self.path.as_path()) else {
                return 0;
            };

            let mut reaped = 0;
            for entry in parse_ledger(&contents) {
                if process_matches(entry.pid, &entry.token) {
                    terminate(entry.pid);
                    reaped += 1;
                }
            }

            let _ = std::fs::remove_file(self.path.as_path());
            reaped
        }

        /// Record a freshly spawned child. `token` must appear in the child's
        /// command line.
        // TODO(P1): call from the `codex app-server` and MCP stdio spawn sites.
        // Both rely on `kill_on_drop` alone today, which §6 says is not enough.
        #[allow(dead_code)]
        pub fn record(&self, pid: u32, token: impl Into<String>) {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            live.push(Entry {
                pid,
                token: token.into(),
            });
            self.flush(&live);
        }

        /// Forget a child that exited normally.
        #[allow(dead_code)]
        pub fn forget(&self, pid: u32) {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            live.retain(|e| e.pid != pid);
            self.flush(&live);
        }

        /// Signal every live child and clear the ledger — the shutdown half of
        /// §6's requirement.
        pub fn terminate_all(&self) {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            for entry in live.iter() {
                if process_matches(entry.pid, &entry.token) {
                    terminate(entry.pid);
                }
            }
            live.clear();
            let _ = std::fs::remove_file(self.path.as_path());
        }

        fn flush(&self, live: &[Entry]) {
            let body: String = live
                .iter()
                .map(|e| format!("{}\t{}\n", e.pid, e.token))
                .collect();
            let _ = crate::paths::atomic_write(self.path.as_path(), body.as_bytes());
        }
    }

    fn parse_ledger(contents: &str) -> Vec<Entry> {
        contents
            .lines()
            .filter_map(|line| {
                let (pid, token) = line.split_once('\t')?;
                Some(Entry {
                    pid: pid.trim().parse().ok()?,
                    token: token.trim().to_owned(),
                })
            })
            .collect()
    }

    /// Whether `pid` is alive **and** its command line contains `token`.
    ///
    /// Implemented by shelling out rather than by `libc::kill` so the binary can
    /// keep `#![forbid(unsafe_code)]`. This runs a handful of times at startup
    /// and never on a latency path, so the process spawn costs nothing that
    /// matters.
    #[cfg(unix)]
    pub fn process_matches(pid: u32, token: &str) -> bool {
        let Ok(output) = std::process::Command::new("/bin/ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        String::from_utf8_lossy(&output.stdout).contains(token)
    }

    /// Windows equivalent: `tasklist` filtered by pid, matched on image name.
    ///
    /// `tasklist` reports the image name only, not the full command line, so
    /// `token` is matched against that — callers pass an executable name
    /// (`codex.exe`), not an argument.
    #[cfg(windows)]
    pub fn process_matches(pid: u32, token: &str) -> bool {
        let Ok(output) = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).contains(token)
    }

    /// Terminate the child's whole **process group**.
    ///
    /// `aibo-agent` spawns `codex app-server` with `process_group(0)`, and codex
    /// spawns MCP servers and shell commands of its own. Signalling only the
    /// leader orphans them, which is the bug this module exists to prevent.
    #[cfg(unix)]
    fn terminate(pid: u32) {
        // `kill -TERM -<pid>` addresses the group whose leader is `pid`.
        let _ = std::process::Command::new("/bin/kill")
            .args(["-TERM", &format!("-{pid}")])
            .status();
        // Then the process itself, in case it never became a group leader.
        let _ = std::process::Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }

    /// Windows has no POSIX process groups; `taskkill /T` walks the child tree
    /// instead. §6 asks for a Job Object, which is stronger (it survives the
    /// parent and cannot be escaped) — see the report.
    #[cfg(windows)]
    fn terminate(pid: u32) {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ledger_round_trips_and_skips_rubbish() {
            let parsed = parse_ledger("123\tcodex\n456\tmcp-server\nrubbish\n");
            assert_eq!(parsed.len(), 2);
            assert_eq!(parsed[0].pid, 123);
            assert_eq!(parsed[1].token, "mcp-server");
        }

        #[test]
        fn a_dead_pid_never_matches() {
            assert!(!process_matches(0, "aibo"));
        }

        #[test]
        fn a_live_pid_with_the_wrong_token_never_matches() {
            // This test binary is certainly alive, and certainly is not codex.
            assert!(!process_matches(
                std::process::id(),
                "definitely-not-this-process"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// secrets
// ---------------------------------------------------------------------------

/// The oversize half of §12's credential storage, and the `TokenStore` over it.
///
/// ## Why this exists at all
///
/// §12: *"Windows Credential Manager caps a secret at 2560 bytes
/// (`CRED_MAX_CREDENTIAL_BLOB_SIZE`), and `keyring` UTF-16-doubles first — so
/// `set_password` tops out around 1280 ASCII characters. A 32-byte database key
/// is fine; a multi-kilobyte OAuth JWT is not."* §3a then measured the thing:
/// the ChatGPT access token is **1652 bytes**, and a serialised
/// [`aibo_provider::StoredTokens`] carries an access token, a refresh token and
/// the claims alongside them.
///
/// `aibo_store::Keychain` writes through `set_password` and enforces that cap on
/// every platform, so it refuses a token pair outright — correctly, because
/// storing one that way is exactly the failure §12 predicted. What it does not
/// provide is somewhere for the token pair to *go*.
///
/// ## The two routes §12 allows, and which one this takes
///
/// §12 names *"either DPAPI-encrypted file storage or chunking across entries"*.
/// Chunking is rejected for the reason `aibo_store::secrets` already states — a
/// torn write across entries reassembles into garbage with no transaction to
/// prevent it — and the DPAPI file needs a `Protector`, whose implementation is
/// `unsafe` and belongs in `aibo-platform` (spike **S8**, not yet written).
///
/// So this takes the third route the plan's own note points at:
/// **`keyring::Entry::set_secret`**, which writes the raw blob rather than a
/// UTF-16 string. That removes the doubling — the cap becomes 2560 *bytes*, not
/// ~1280 characters — and on macOS, where Keychain Services has no comparable
/// limit, it removes the ceiling entirely. It stays one entry and one write, so
/// the torn-write objection to chunking does not apply.
///
/// A blob that is still over the Windows cap is refused with the limit named,
/// rather than truncated or written in plaintext. That case is real for a large
/// token pair on Windows and is what the DPAPI file store is for; the seam
/// ([`aibo_store::SecretStorage::with_oversize`]) already exists, so finishing
/// S8 is a substitution here and not a redesign.
mod secrets {
    use std::sync::Arc;

    use aibo_core::error::{AiboError, Result as CoreResult};
    use aibo_provider::auth::{StoredTokens, TokenStore};
    use aibo_store::error::{KeychainError, KeychainErrorKind};
    use aibo_store::secrets::{
        KEYCHAIN_SERVICE, SecretStore, WINDOWS_CREDENTIAL_BLOB_MAX_BYTES, provider_account,
    };
    use aibo_store::{Result as StoreResult, SecretStorage, StoreError};
    use async_trait::async_trait;
    use zeroize::Zeroizing;

    /// Service name for the raw-blob entries.
    ///
    /// Deliberately **not** [`KEYCHAIN_SERVICE`]. `SecretStorage` deletes the
    /// other backend's copy whenever it writes, so sharing a service *and* an
    /// account would make a small-secret write delete the entry it had just
    /// created. A separate service keeps the two backends genuinely separate.
    pub fn blob_service() -> String {
        format!("{KEYCHAIN_SERVICE}.blob")
    }

    /// A [`SecretStore`] that writes raw bytes via `keyring::Entry::set_secret`.
    #[derive(Debug, Clone)]
    pub struct BlobKeychain {
        service: String,
    }

    impl Default for BlobKeychain {
        fn default() -> Self {
            Self {
                service: blob_service(),
            }
        }
    }

    impl BlobKeychain {
        fn entry(&self, account: &str) -> StoreResult<keyring::Entry> {
            keyring::Entry::new(&self.service, account)
                .map_err(|e| self.failure(account, KeychainErrorKind::Platform, e.to_string()))
        }

        fn failure(&self, account: &str, kind: KeychainErrorKind, detail: String) -> StoreError {
            StoreError::Keychain(KeychainError {
                service: self.service.clone(),
                account: account.to_owned(),
                kind,
                detail,
            })
        }
    }

    /// Whether a raw blob fits Windows Credential Manager.
    ///
    /// The cap applies to the bytes actually written. `set_password` would have
    /// to clear `WINDOWS_CREDENTIAL_BLOB_MAX_BYTES / 2` characters instead,
    /// which is the doubling `set_secret` avoids — §3a's 1652-byte access token
    /// is the case that turns on exactly that distinction.
    pub const fn blob_fits_credential_manager(bytes: usize) -> bool {
        bytes <= WINDOWS_CREDENTIAL_BLOB_MAX_BYTES
    }

    impl SecretStore for BlobKeychain {
        fn get(&self, account: &str) -> StoreResult<Option<Zeroizing<String>>> {
            match self.entry(account)?.get_secret() {
                Ok(bytes) => {
                    let text = String::from_utf8(bytes).map_err(|_| {
                        self.failure(
                            account,
                            KeychainErrorKind::BadData,
                            "stored blob was not UTF-8".to_owned(),
                        )
                    })?;
                    Ok(Some(Zeroizing::new(text)))
                }
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(self.failure(account, KeychainErrorKind::Platform, e.to_string())),
            }
        }

        fn set(&self, account: &str, secret: &str) -> StoreResult<()> {
            // Only Windows caps a credential blob. Applying the cap on macOS
            // too — which is what `set_password`'s unconditional ceiling does
            // today — would refuse the one platform where the token
            // demonstrably fits, so the guard is scoped to the platform whose
            // rule it actually is (§8's secrets row: Keychain Services on
            // macOS, Credential Manager / DPAPI on Windows).
            if !blob_fits_credential_manager(secret.len()) {
                #[cfg(windows)]
                {
                    // The remaining §12 route is the DPAPI-encrypted file, and
                    // `SecretStorage::with_oversize` is already the seam for
                    // it — spike S8 supplies the `Protector`. Refuse with the
                    // limit named rather than truncate or write plaintext.
                    return Err(StoreError::SecretTooLarge {
                        account: account.to_owned(),
                        utf16_bytes: secret.len(),
                        limit: WINDOWS_CREDENTIAL_BLOB_MAX_BYTES,
                    });
                }
                #[cfg(not(windows))]
                tracing::debug!(
                    account,
                    bytes = secret.len(),
                    limit = WINDOWS_CREDENTIAL_BLOB_MAX_BYTES,
                    "secret exceeds the Windows credential cap; Keychain Services has none"
                );
            }
            self.entry(account)?
                .set_secret(secret.as_bytes())
                .map_err(|e| self.failure(account, KeychainErrorKind::Platform, e.to_string()))
        }

        fn delete(&self, account: &str) -> StoreResult<()> {
            match self.entry(account)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(self.failure(account, KeychainErrorKind::Platform, e.to_string())),
            }
        }
    }

    /// The keychain-backed [`TokenStore`] §3a's device flow persists through.
    ///
    /// One JSON document per storage key, routed by size:
    /// [`aibo_store::SecretStorage`] sends anything that fits to the ordinary
    /// keychain entry and everything larger — which is every real token pair —
    /// to [`BlobKeychain`].
    pub struct KeychainTokenStore {
        storage: Arc<SecretStorage>,
    }

    impl std::fmt::Debug for KeychainTokenStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("KeychainTokenStore").finish_non_exhaustive()
        }
    }

    impl KeychainTokenStore {
        /// Wrap a size-routing storage façade.
        pub fn new(storage: Arc<SecretStorage>) -> Self {
            Self { storage }
        }
    }

    /// The keychain account a token-storage key is filed under.
    ///
    /// Shares `provider:`-prefixed naming with API keys so one keychain audit
    /// shows every credential aibo holds.
    pub fn token_account(key: &str) -> String {
        provider_account(key)
    }

    /// §13: a keychain fault is never rendered raw, so it crosses as
    /// `Internal`, which §13 gives the generic treatment plus "copy
    /// diagnostics". `StoreError`'s `Display` names the service, the account
    /// and the platform detail, and never the secret.
    fn wrap(error: StoreError) -> AiboError {
        AiboError::Internal(Box::new(error))
    }

    #[async_trait]
    impl TokenStore for KeychainTokenStore {
        async fn load(&self, key: &str) -> CoreResult<Option<StoredTokens>> {
            let storage = self.storage.clone();
            let account = token_account(key);
            // `keyring` is blocking and can put a consent dialog on screen.
            let found = tokio::task::spawn_blocking(move || storage.get(&account))
                .await
                .map_err(|e| AiboError::Internal(Box::new(e)))?
                .map_err(wrap)?;

            let Some(json) = found else {
                return Ok(None);
            };
            match serde_json::from_str::<StoredTokens>(&json) {
                Ok(tokens) => Ok(Some(tokens)),
                Err(error) => {
                    // A blob written by an older build, or a half-restored
                    // keychain. Treat as "no credential" rather than as a hard
                    // failure: §13's answer to that is "sign in", which works.
                    tracing::warn!(%error, "stored Codex tokens could not be parsed; ignoring");
                    Ok(None)
                }
            }
        }

        async fn save(&self, key: &str, tokens: &StoredTokens) -> CoreResult<()> {
            let json =
                serde_json::to_string(tokens).map_err(|e| AiboError::Internal(Box::new(e)))?;
            let storage = self.storage.clone();
            let account = token_account(key);
            tokio::task::spawn_blocking(move || storage.set(&account, &json))
                .await
                .map_err(|e| AiboError::Internal(Box::new(e)))?
                .map_err(wrap)
        }

        async fn clear(&self, key: &str) -> CoreResult<()> {
            let storage = self.storage.clone();
            let account = token_account(key);
            tokio::task::spawn_blocking(move || storage.delete(&account))
                .await
                .map_err(|e| AiboError::Internal(Box::new(e)))?
                .map_err(wrap)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use aibo_store::secrets::{fits_in_credential_manager, utf16_bytes};

        /// §3a measured the access token at 1652 bytes. That is the number the
        /// storage decision turns on, so it is asserted rather than described:
        /// `set_password`'s UTF-16 doubling puts it over the Windows cap, and
        /// writing the raw blob does not.
        #[test]
        fn the_measured_access_token_needs_the_raw_blob_path() {
            let access_token = "e".repeat(1652);
            assert_eq!(access_token.len(), 1652);

            assert_eq!(utf16_bytes(&access_token), 3304);
            assert!(
                !fits_in_credential_manager(&access_token),
                "§12: `set_password` doubles first, so 1652 bytes becomes 3304 and \
                 exceeds the {WINDOWS_CREDENTIAL_BLOB_MAX_BYTES}-byte cap"
            );
            assert!(
                blob_fits_credential_manager(access_token.len()),
                "`set_secret` writes the raw bytes, and 1652 is inside the cap"
            );
        }

        /// The oversize backend must not share a namespace with the ordinary
        /// one: `SecretStorage` deletes the other backend's copy on every
        /// write, so a shared service and account would make writing a small
        /// secret delete it again.
        #[test]
        fn the_blob_backend_has_its_own_service_namespace() {
            assert_ne!(blob_service(), KEYCHAIN_SERVICE);
            assert!(blob_service().starts_with(KEYCHAIN_SERVICE));
        }

        /// The token entry is filed alongside the API keys, so one keychain
        /// audit shows everything aibo holds.
        #[test]
        fn the_token_account_is_namespaced_with_the_other_credentials() {
            let account = token_account(aibo_provider::codex::TOKEN_STORAGE_KEY);
            assert_eq!(account, "provider:codex/device-auth");
        }
    }
}

// ---------------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------------

/// Building the [`aibo_session::Engine`] the app runs on.
///
/// Three inputs, each of which is allowed to be missing:
///
/// | Input | Missing means |
/// |---|---|
/// | `config.toml` | no providers — §13's blocking `NoProviderConfigured`, which is the correct state for a fresh install |
/// | keychain credential | that provider cannot be built; the error names the environment variable instead |
/// | database key | history and clipboard search are off; inference still works |
///
/// Nothing here aborts startup. §6 wants the tray up and the hotkey live even
/// when the app is misconfigured, because settings is reached *through* the
/// tray.
mod bootstrap {
    use std::sync::{Arc, Mutex};

    use aibo_core::cost::PriceTable;
    use aibo_core::types::ProviderId;
    use aibo_provider::ProviderRegistry;
    use aibo_provider::auth::RefreshingTokenProvider;
    use aibo_session::{Config, CredentialSource, EngineConfig, EnvCredentials};

    use crate::paths::Paths;
    use crate::secrets::{BlobKeychain, KeychainTokenStore};

    /// Credentials from the OS keychain (§12), falling back to the environment.
    ///
    /// The fallback is not a weakening: `AIBO_<PROVIDER>_API_KEY` is what CI,
    /// the §5 eval harness and a first run before onboarding have, and §12's
    /// rule is about not writing secrets to *disk*, which this does not.
    struct Credentials {
        keychain: Arc<aibo_store::SecretStorage>,
    }

    impl CredentialSource for Credentials {
        fn api_key(&self, provider: &ProviderId) -> Option<secrecy::SecretString> {
            let account = aibo_store::secrets::provider_account(provider.as_str());
            match self.keychain.get(&account) {
                Ok(Some(secret)) => Some(secrecy::SecretString::from(secret.to_string())),
                Ok(None) => EnvCredentials.api_key(provider),
                Err(error) => {
                    // A locked or denied keychain is not fatal: §13 would rather
                    // show "sign in" than fail to start.
                    tracing::warn!(%provider, %error, "could not read the keychain (§12)");
                    EnvCredentials.api_key(provider)
                }
            }
        }
    }

    /// Everything needed to build — and **re**build — the engine.
    ///
    /// Rebuilding is not a nicety. Signing in to Codex adds a provider to the
    /// registry, and a registry is fixed at `Engine::new`; without a rebuild the
    /// user would complete a device-code login and then have to restart the app
    /// before anything could use it, which is indistinguishable from the login
    /// not working.
    pub struct Bootstrap {
        paths: Paths,
        secrets: Arc<aibo_store::SecretStorage>,
        /// Opened once and shared across rebuilds: re-opening SQLCipher on
        /// every sign-in would cost a key derivation and an integrity check for
        /// nothing, and two live handles to one file is what §12's "database is
        /// locked" path exists to avoid.
        store: Mutex<Option<Option<Arc<dyn aibo_session::SessionStore>>>>,
    }

    impl std::fmt::Debug for Bootstrap {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Bootstrap")
                .field("root", &self.paths.root())
                .finish_non_exhaustive()
        }
    }

    impl Bootstrap {
        /// Resolve the storage seams. Touches neither the keychain nor the
        /// database — both are opened lazily, which is what keeps the
        /// fresh-install path free of an unprompted keychain dialog (§17).
        pub fn new(paths: Paths) -> Self {
            Self {
                paths,
                // §12/§3a: the Windows cap makes this a two-backend façade.
                // `Keychain` holds the 32-byte database key and the API keys;
                // `BlobKeychain` holds anything token-shaped. See `mod secrets`.
                secrets: Arc::new(aibo_store::SecretStorage::with_oversize(
                    aibo_store::Keychain::default(),
                    BlobKeychain::default(),
                )),
                store: Mutex::new(None),
            }
        }

        /// The user's configuration, or the default when it cannot be read.
        pub fn config(&self) -> Config {
            match Config::load(&self.paths.config()) {
                Ok(config) => config,
                Err(error) => {
                    // §6 writes the config atomically precisely so this is
                    // rare; when it happens, do not lose the tray over it.
                    tracing::error!(%error, "the configuration could not be read");
                    Config::default()
                }
            }
        }

        /// Where aibo keeps things.
        pub fn paths(&self) -> &Paths {
            &self.paths
        }

        /// A Codex token provider over the OS keychain (§3a).
        ///
        /// Constructed on demand and never cached: the client id is a setting,
        /// so a provider built before it changed would keep using the old one.
        /// Construction reads nothing — [`RefreshingTokenProvider`] loads on
        /// first use — so building one is free and cannot prompt for keychain
        /// access on a fresh install.
        pub fn codex_tokens(&self, config: &Config) -> Option<Arc<RefreshingTokenProvider>> {
            let store = Arc::new(KeychainTokenStore::new(self.secrets.clone()));
            match aibo_provider::codex::token_provider(config.codex.client_id(), store, None) {
                Ok(tokens) => Some(tokens),
                Err(error) => {
                    // Only reachable with a blank client id, which
                    // `CodexConfig::client_id` already rules out — but the
                    // device flow's own posture note says the value is meant to
                    // stay configurable, so a bad one must degrade rather than
                    // abort startup.
                    tracing::error!(%error, "could not build the Codex token provider (§3a)");
                    None
                }
            }
        }

        /// Build the engine, degrading rather than failing.
        pub fn engine(&self) -> Arc<aibo_session::Engine> {
            let config_path = self.paths.config();
            if !config_path.exists() {
                // The fresh-install path. Deliberately does *not* touch the
                // keychain: an unprompted "aibo wants to use your keychain"
                // dialog before the user has configured anything is the worst
                // possible first impression, and §17 owns that moment.
                tracing::info!(
                    path = %config_path.display(),
                    "no configuration; starting with no providers (§13 NoProviderConfigured)"
                );
                return Arc::new(aibo_session::Engine::new(
                    ProviderRegistry::new(),
                    EngineConfig::default(),
                ));
            }

            let config = self.config();
            let prices = load_prices(&self.paths);
            let credentials = Credentials {
                keychain: self.secrets.clone(),
            };
            // The token provider is passed whether or not `[codex] enabled` is
            // set: building one is free, and handing it over unconditionally
            // means `enabled = true` can never fail for the one reason the user
            // cannot act on.
            let codex_tokens = self.codex_tokens(&config);

            let (registry, engine_config) =
                match config.build_with_codex(&credentials, prices, codex_tokens) {
                    Ok(built) => built,
                    Err(error) => {
                        tracing::error!(%error, "the configuration could not be applied");
                        (ProviderRegistry::new(), EngineConfig::default())
                    }
                };

            tracing::info!(providers = ?registry.ids(), "provider registry built");

            let mut engine = aibo_session::Engine::new(registry, engine_config);
            if let Some(store) = self.session_store() {
                engine = engine.with_store(store);
            }
            Arc::new(engine)
        }

        fn session_store(&self) -> Option<Arc<dyn aibo_session::SessionStore>> {
            let mut slot = self.store.lock().unwrap_or_else(|e| e.into_inner());
            slot.get_or_insert_with(|| open_store(&self.paths, &self.secrets))
                .clone()
        }
    }

    /// §14's table: the shipped defaults with the user's overlay on top.
    fn load_prices(paths: &Paths) -> PriceTable {
        let user = std::fs::read_to_string(paths.prices()).ok();
        match PriceTable::load(user.as_deref()) {
            Ok(table) => table,
            Err(error) => {
                // §14 makes a malformed *user* file an error rather than a
                // silent fallback — mis-pricing after the user tried to fix a
                // rate is the failure the meter exists to prevent. Here that
                // means: say so loudly, and price nothing rather than price it
                // wrongly.
                tracing::error!(%error, "the price table is invalid; costs will show as unknown");
                PriceTable::empty()
            }
        }
    }

    /// §12 persistence, if there is a key to open it with.
    ///
    /// Deliberately read-only about the key. `SecretStorage::db_key_or_create`
    /// would work, but §12 requires the recovery code to be *shown exactly
    /// once, at setup* — generating one silently would leave the user with an
    /// encrypted database and no way back into it after a keychain loss.
    /// Creating the key belongs to onboarding (§17).
    fn open_store(
        paths: &Paths,
        secrets: &aibo_store::SecretStorage,
    ) -> Option<Arc<dyn aibo_session::SessionStore>> {
        let key = match secrets.db_key() {
            Ok(Some(key)) => key,
            Ok(None) => {
                tracing::info!(
                    "no database key yet; history is off until onboarding creates one (§12, §17)"
                );
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, "could not read the database key; history is off");
                return None;
            }
        };

        match aibo_store::Db::open(paths.database(), &key) {
            Ok(db) => Some(Arc::new(aibo_session::SqliteStore::new(db))),
            Err(error) => {
                // §12 designs paths for "locked", "corrupt" and
                // "half-migrated". None of them is a reason not to answer the
                // next hotkey press.
                tracing::error!(%error, "could not open the database; history is off (§12)");
                None
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The fresh-install path, and the one this test can assert without
        /// touching the user's keychain — which is exactly the property the
        /// early return exists to guarantee.
        #[test]
        fn a_fresh_install_starts_with_no_providers_and_no_keychain_access() {
            let dir = std::env::temp_dir().join("aibo-bootstrap-test");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();

            // `Paths` resolves from AIBO_HOME, but constructing one directly
            // keeps this test free of process-global environment mutation.
            let paths = Paths::for_root(dir.clone());
            let engine = Bootstrap::new(paths).engine();

            assert!(engine.providers().is_empty());
            assert!(!dir.join("aibo.db").exists(), "no database was created");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Building a Codex token provider must stay free of I/O, because it
        /// happens on every engine build — including the fresh-install one,
        /// where a keychain prompt would be the user's first experience of the
        /// app (§17).
        #[test]
        fn building_the_codex_token_provider_touches_nothing() {
            // Per-process and per-test, so two `cargo test` invocations against
            // the same checkout cannot race each other through a shared path.
            let dir =
                std::env::temp_dir().join(format!("aibo-codex-tokens-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();

            let boot = Bootstrap::new(Paths::for_root(dir.clone()));
            let config = boot.config();
            assert!(
                !config.codex.enabled,
                "a fresh install has no Codex sign-in"
            );
            assert!(boot.codex_tokens(&config).is_some());
            assert!(!dir.join("aibo.db").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// ---------------------------------------------------------------------------
// config_file
// ---------------------------------------------------------------------------

/// Writing the parts of `config.toml` the app owns.
///
/// §12 keeps settings in plaintext TOML and credentials in the keychain, so
/// what a completed Codex login persists here is exactly two facts — that the
/// user signed in, and which model they chose — and **never a token**.
///
/// This splices a table rather than re-serialising the file. `Config` is
/// `Deserialize`-only by design (it holds no secrets *because* nothing round
/// trips through it), and a serialise-everything write would silently discard
/// comments and any key a future build added. Replacing one table leaves the
/// rest of the user's file exactly as they wrote it.
mod config_file {
    use std::io;
    use std::path::Path;

    /// Rewrite `config.toml`'s `[codex]` table (§3a).
    pub fn write_codex(path: &Path, enabled: bool, model: &str) -> io::Result<()> {
        let existing = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let body = format!("enabled = {enabled}\nmodel = {}\n", quote(model));
        let updated = splice_table(&existing, "codex", &body);
        crate::paths::atomic_write(path, updated.as_bytes())
    }

    /// TOML basic-string quoting, for a value that is a model id.
    fn quote(value: &str) -> String {
        let escaped: String = value
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                other => vec![other],
            })
            .collect();
        format!("\"{escaped}\"")
    }

    /// Replace `[table]` and its key/value lines with `body`, appending the
    /// table when it is absent.
    ///
    /// A table ends at the next line whose first non-whitespace character is
    /// `[`, which is TOML's own rule for where a table's keys stop.
    fn splice_table(source: &str, table: &str, body: &str) -> String {
        let header = format!("[{table}]");
        let mut out = String::with_capacity(source.len() + body.len() + header.len() + 2);
        let mut skipping = false;
        let mut replaced = false;

        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed == header {
                skipping = true;
                replaced = true;
                out.push_str(&header);
                out.push('\n');
                out.push_str(body);
                continue;
            }
            if skipping {
                // Any other table header — including `[[providers]]` — ends it.
                if trimmed.starts_with('[') {
                    skipping = false;
                } else {
                    continue;
                }
            }
            out.push_str(line);
            out.push('\n');
        }

        if !replaced {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(&header);
            out.push('\n');
            out.push_str(body);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_missing_table_is_appended() {
            let out = splice_table(
                "[[providers]]\nbackend = \"groq\"\n",
                "codex",
                "enabled = true\n",
            );
            assert!(out.contains("backend = \"groq\""));
            assert!(out.contains("[codex]\nenabled = true\n"));
        }

        #[test]
        fn an_existing_table_is_replaced_in_place_and_nothing_else_moves() {
            let source = "\
# my notes
[[providers]]
backend = \"groq\"

[codex]
enabled = true
model = \"gpt-5.6-sol\"

[health]
degrade_after = 4
";
            let out = splice_table(
                &source.replace('\r', ""),
                "codex",
                "enabled = false\nmodel = \"gpt-5.5\"\n",
            );

            assert!(out.contains("# my notes"), "comments survive: {out}");
            assert!(out.contains("backend = \"groq\""));
            assert!(out.contains("[health]\ndegrade_after = 4"));
            assert!(out.contains("enabled = false"));
            assert!(out.contains("model = \"gpt-5.5\""));
            assert!(
                !out.contains("gpt-5.6-sol"),
                "the previous model must not linger: {out}"
            );
            assert_eq!(out.matches("[codex]").count(), 1);
        }

        /// The round trip that matters: what is written back must parse, and
        /// §12's rule must hold — no token anywhere in the file.
        #[test]
        fn what_is_written_parses_and_holds_no_secret() {
            let dir =
                std::env::temp_dir().join(format!("aibo-config-codex-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let path = dir.join("config.toml");

            write_codex(&path, true, "gpt-5.5").unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            let config = aibo_session::Config::from_toml_str(&text).unwrap();
            assert!(config.codex.enabled);
            assert_eq!(config.codex.model, "gpt-5.5");

            for forbidden in ["access_token", "refresh_token", "id_token", "Bearer", "eyJ"] {
                assert!(
                    !text.contains(forbidden),
                    "§12: `{forbidden}` must never reach the plaintext config"
                );
            }

            // Signing out flips one flag and leaves the rest alone.
            write_codex(&path, false, "gpt-5.5").unwrap();
            let config =
                aibo_session::Config::from_toml_str(&std::fs::read_to_string(&path).unwrap())
                    .unwrap();
            assert!(!config.codex.enabled);

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Asserted against a real TOML parse rather than against the escaping
        /// code's own opinion of itself.
        #[test]
        fn a_model_id_with_a_quote_cannot_break_out_of_the_value() {
            let quoted = quote("gpt\"5.5");
            assert_eq!(quoted, "\"gpt\\\"5.5\"");

            let config =
                aibo_session::Config::from_toml_str(&format!("[codex]\nmodel = {quoted}\n"))
                    .expect("the escaped value must still be valid TOML");
            assert_eq!(config.codex.model, "gpt\"5.5");
        }
    }
}

// ---------------------------------------------------------------------------
// codex_signin
// ---------------------------------------------------------------------------

/// Driving §3a's verified device-code login and reporting it to the UI.
///
/// **This module implements none of the protocol.** Every deviation from
/// RFC 8628 that §3a records — JSON for `usercode` and the poll but form
/// encoding for the exchange, no `device_code`, a poll keyed on `user_code`,
/// HTTP 403 for "still pending", an authorization code plus a PKCE pair instead
/// of tokens, `{issuer}/deviceauth/callback` as the redirect URI, and the
/// Codex-shaped `User-Agent` without which Cloudflare answers 530 — lives in
/// `aibo_provider::codex`, was executed end-to-end against the live endpoint on
/// 2026-07-26, and is called here rather than reproduced.
///
/// What is here is the part that module deliberately does not own: turning a
/// button press into `start` → `wait_for_token` → `seed`, and turning the wait
/// into something visible.
mod codex_signin {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use aibo_provider::auth::RefreshingTokenProvider;
    use aibo_provider::codex::{DeviceAuthClient, DeviceCodeChallenge};
    use aibo_ui::settings::CodexPhase;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio_util::sync::CancellationToken;

    /// How often the countdown under the user code is refreshed.
    ///
    /// Independent of the *poll* interval, which the server dictates and
    /// `wait_for_token` honours (§3a deviation: a client that ignores
    /// `slow_down` gets rate-limited out of its own login). This only moves a
    /// number on screen.
    const PROGRESS_TICK: Duration = Duration::from_secs(1);

    /// One step of a login, as the backend loop sees it.
    #[derive(Debug, Clone)]
    pub enum Event {
        /// A phase change with the sentence the user should read.
        Progress {
            /// Where the flow is.
            phase: CodexPhase,
            /// Human-readable, already redacted — never a token.
            detail: String,
        },
        /// Tokens are in the keychain and the provider can be built.
        Succeeded {
            /// `chatgpt_account_id`, when the ID token carried one.
            account: Option<String>,
        },
        /// The attempt ended without tokens.
        Failed {
            /// One sentence, §13-shaped.
            detail: String,
        },
        /// The user pressed the button again while it was running.
        Cancelled,
    }

    /// Run one login, reporting through `events`.
    ///
    /// Returns the terminal [`Event`]; the progress ones are sent as they
    /// happen because a device flow is, by construction, a minutes-long wait
    /// during which silence is indistinguishable from a hang.
    pub async fn run(
        client_id: String,
        tokens: Arc<RefreshingTokenProvider>,
        cancel: CancellationToken,
        events: UnboundedSender<Event>,
    ) -> Event {
        let progress = |phase, detail: String| {
            let _ = events.send(Event::Progress { phase, detail });
        };

        progress(
            CodexPhase::Starting,
            "Asking OpenAI for a device code…".to_owned(),
        );

        let client = match DeviceAuthClient::new(None, Some(client_id)) {
            Ok(client) => client,
            Err(error) => {
                return Event::Failed {
                    detail: format!("Could not start the sign-in: {error}"),
                };
            }
        };

        let challenge = match client.start().await {
            Ok(challenge) => challenge,
            Err(error) => {
                return Event::Failed {
                    detail: format!("OpenAI refused the sign-in request: {error}"),
                };
            }
        };

        progress(
            CodexPhase::AwaitingApproval,
            awaiting_detail(&challenge, SystemTime::now()),
        );
        // Convenience only — the URL is in the sentence above either way.
        open_verification_page(&challenge.verification_uri);

        // §3a deviation 4: pending approval is HTTP 403, not
        // `authorization_pending` in a 400 body, and `poll_once` maps it to
        // `PollOutcome::Pending`. `wait_for_token` is the loop that honours the
        // server's interval, widens it on `slow_down`, stops at `expires_at`
        // and observes the cancellation token — all four already verified.
        let waiter = client.wait_for_token(&challenge, cancel.clone());
        tokio::pin!(waiter);

        // `interval` fires immediately on its first tick, which would repeat
        // the line just published. Start one period out instead.
        let mut tick =
            tokio::time::interval_at(tokio::time::Instant::now() + PROGRESS_TICK, PROGRESS_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let tokens_result = loop {
            tokio::select! {
                result = &mut waiter => break result,
                _ = tick.tick() => {
                    progress(
                        CodexPhase::AwaitingApproval,
                        awaiting_detail(&challenge, SystemTime::now()),
                    );
                }
            }
        };

        // Cancellation and expiry both surface as `Auth { kind: Expired }`, so
        // the token is what distinguishes "the user pressed cancel" from "the
        // code ran out" — and they need different copy: one is not a failure.
        if cancel.is_cancelled() {
            return Event::Cancelled;
        }

        let token_set = match tokens_result {
            Ok(set) => set,
            Err(error) => {
                let detail = if challenge.is_expired(SystemTime::now()) {
                    format!(
                        "The code {} expired before it was approved. Start again.",
                        challenge.user_code
                    )
                } else {
                    format!("Sign-in failed: {error}")
                };
                return Event::Failed { detail };
            }
        };

        progress(
            CodexPhase::Exchanging,
            "Approved. Storing the tokens in your keychain…".to_owned(),
        );

        let account = token_set.account_id.clone();
        // §12: this is the only place the tokens are written, and they go to
        // the OS keychain — never to the config file.
        if let Err(error) = tokens.seed(token_set).await {
            return Event::Failed {
                detail: format!("Signed in, but the tokens could not be stored: {error}"),
            };
        }

        Event::Succeeded { account }
    }

    /// The sentence shown while the user approves.
    ///
    /// Carries the two things §3a's flow gives the user and nothing else does:
    /// the code the poll is keyed on, and the page it is typed into. The
    /// countdown is the difference between "waiting" and "hung".
    pub fn awaiting_detail(challenge: &DeviceCodeChallenge, now: SystemTime) -> String {
        let remaining = challenge
            .expires_at
            .duration_since(now)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        format!(
            "Enter the code {code} at {uri} — waiting for approval ({minutes}m {seconds:02}s left).",
            code = challenge.user_code,
            uri = challenge.verification_uri,
            minutes = remaining / 60,
            seconds = remaining % 60,
        )
    }

    /// Open the verification page in the user's browser.
    ///
    /// Best effort by design: the URL is in the sentence above either way, so a
    /// machine with no handler for it loses convenience and not the flow.
    pub fn open_verification_page(uri: &str) {
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut c = std::process::Command::new("/usr/bin/open");
            c.arg(uri);
            c
        };
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "start", "", uri]);
            c
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let mut command = {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(uri);
            c
        };

        match command.spawn() {
            Ok(_) => tracing::info!("opened the Codex device-approval page"),
            Err(error) => {
                tracing::warn!(%error, "could not open the approval page; the URL is in the panel")
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn challenge(seconds_left: u64) -> DeviceCodeChallenge {
            DeviceCodeChallenge {
                user_code: "ABCD-1234".to_owned(),
                device_auth_id: "dev_1".to_owned(),
                verification_uri: aibo_provider::codex::VERIFICATION_URI.to_owned(),
                interval: Duration::from_secs(5),
                expires_at: SystemTime::now() + Duration::from_secs(seconds_left),
            }
        }

        /// The user code and the verification URI reach the user, because this
        /// sentence is the only place either one is ever shown.
        #[test]
        fn the_progress_line_carries_the_code_the_page_and_the_countdown() {
            let now = SystemTime::now();
            let detail = awaiting_detail(&challenge(585), now);

            assert!(detail.contains("ABCD-1234"), "{detail}");
            assert!(
                detail.contains("https://auth.openai.com/codex/device"),
                "§3a: this exact page is where approval happens — {detail}"
            );
            assert!(
                detail.contains("9m"),
                "the countdown must be visible: {detail}"
            );
        }

        /// An expired challenge counts down to zero rather than underflowing or
        /// panicking on `duration_since`.
        #[test]
        fn an_expired_challenge_reports_no_time_left() {
            let expired = DeviceCodeChallenge {
                expires_at: SystemTime::now() - Duration::from_secs(60),
                ..challenge(0)
            };
            let detail = awaiting_detail(&expired, SystemTime::now());
            assert!(detail.contains("0m 00s"), "{detail}");
            assert!(expired.is_expired(SystemTime::now()));
        }

        /// The progress sentence survives the health channel intact — that
        /// round trip is the whole path from the poll to the user's eyes.
        #[test]
        fn the_progress_line_survives_the_settings_channel() {
            let detail = awaiting_detail(&challenge(300), SystemTime::now());
            let health = CodexPhase::AwaitingApproval.to_health(&detail);
            let (phase, read) = CodexPhase::read(&health);

            assert_eq!(phase, CodexPhase::AwaitingApproval);
            assert_eq!(read, detail);
            assert!(read.contains("ABCD-1234"));
        }

        /// Nothing token-shaped may reach a progress sentence: these strings
        /// are rendered in a window and land in screenshots.
        #[test]
        fn progress_lines_never_carry_a_token() {
            for detail in [
                awaiting_detail(&challenge(120), SystemTime::now()),
                "Approved. Storing the tokens in your keychain…".to_owned(),
            ] {
                assert!(!detail.contains("eyJ"), "{detail}");
                assert!(!detail.to_lowercase().contains("bearer"), "{detail}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// runtime
// ---------------------------------------------------------------------------

/// The tokio half of §6's diagram: everything the UI asks for, none of it on the
/// UI thread.
mod runtime {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use aibo_core::AiboError;
    use aibo_core::error::InsertFailure;
    use aibo_core::traits::PlatformBackend;
    use aibo_core::types::{
        AppInfo, AppRef, ClipboardItem, FieldContext, InsertMode, InsertTarget, PowerEvent,
        ProviderId, Role, Surface,
    };
    use aibo_session::{Capture, Engine, EventSink, Outcome, SessionEvent, Submission};
    use aibo_ui::settings::CodexPhase;
    use aibo_ui::{SessionId, UiEvent, UiRequest};
    use futures::StreamExt as _;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
    use tokio_util::sync::CancellationToken;

    use crate::bootstrap::Bootstrap;
    use crate::children::Registry as ChildRegistry;
    use crate::codex_signin;

    /// §8 step 3: the hard deadline for a pure AX/UIA read.
    const AX_DEADLINE: Duration = Duration::from_millis(120);
    /// §8 step 3: the deadline including the synthetic-copy clipboard fallback.
    const CAPTURE_DEADLINE: Duration = Duration::from_millis(250);
    /// §8 requires `restore_focus` to *confirm* focus landed before pasting,
    /// with a bounded retry, but names no number. This is the same order as the
    /// capture deadline — a knob, not a measurement.
    const FOCUS_DEADLINE: Duration = Duration::from_millis(250);

    /// What the backend remembers about one panel invocation.
    ///
    /// §13: *"one panel, one session"*. The session is the unit of cancellation
    /// and the unit the UI drops stale events against.
    #[derive(Default)]
    struct Session {
        /// Set once capture resolved the frontmost app.
        app: Option<AppInfo>,
        /// Everything that must still be true at insert time (§8).
        target: Option<InsertTarget>,
        /// The captured selection.
        selection: Option<String>,
        /// The captured field.
        field: Option<FieldContext>,
        /// The captured clipboard, for §5's priority-4 attachment.
        clipboard: Option<ClipboardItem>,
        /// The last submission, so `Retry` can re-run it at another role
        /// without asking the user to retype (§13's "Retry with Smart").
        last: Option<LastSubmission>,
    }

    /// Enough of a submission to replay it.
    #[derive(Clone)]
    struct LastSubmission {
        instruction: String,
        surface: Surface,
        role: Option<Role>,
    }

    /// Messages the backend's own tasks send back to its loop.
    ///
    /// Capture and insert run as spawned tasks so a hung app cannot stall the
    /// request loop; their results still have to reach the session table, and
    /// this is that path.
    enum Internal {
        /// Boxed because the capture payload dwarfs the other variant, and an
        /// enum sized for its largest arm is copied on every channel send.
        Captured(Box<Captured>),
        /// §13 sleep/wake: the machine woke, so every degraded provider is due
        /// for a probe.
        Woke,
        /// A step of the Codex device-code login (§3a).
        CodexAuth(codex_signin::Event),
    }

    /// What the backend remembers about the Codex login (§3a).
    ///
    /// The phase is authoritative here rather than in the UI: the settings
    /// window renders whatever the last `ProviderHealth` said, and a window
    /// that has never been opened has said nothing at all.
    struct CodexAuth {
        phase: CodexPhase,
        /// Set only while a login is running; dropping it is how the one
        /// button's "cancel" meaning is implemented.
        cancel: Option<CancellationToken>,
    }

    impl CodexAuth {
        /// The startup phase, taken from the config rather than the keychain.
        ///
        /// Reading the keychain here would put a consent dialog on screen
        /// before the user has done anything, which §17 rules out. `enabled` is
        /// exactly the fact that a login once completed; if the token has since
        /// been revoked, the first request's §13 `Auth` failure says so, which
        /// is both truthful and later than a startup prompt.
        fn at_startup(config: &aibo_session::Config) -> Self {
            Self {
                phase: if config.codex.enabled {
                    CodexPhase::SignedIn
                } else {
                    CodexPhase::SignedOut
                },
                cancel: None,
            }
        }
    }

    /// The deadline-bounded half of §8's capture, once it has landed.
    struct Captured {
        session: SessionId,
        app: Option<AppInfo>,
        target: Option<InsertTarget>,
        selection: Option<String>,
        field: Option<FieldContext>,
        clipboard: Option<ClipboardItem>,
    }

    /// The tokio-side owner of the platform backend, the session engine and the
    /// session table.
    pub struct Backend {
        platform: Arc<dyn PlatformBackend>,
        /// §4, §5, §13, §14 — everything between "the user pressed return" and
        /// "tokens are arriving".
        ///
        /// Replaced, not mutated, when the provider set changes: a registry is
        /// fixed at `Engine::new`, so signing in to Codex means a new engine.
        engine: Arc<Engine>,
        /// Everything needed to build that replacement.
        bootstrap: Arc<Bootstrap>,
        /// §3a's device-code login.
        codex: CodexAuth,
        sessions: HashMap<SessionId, Session>,
        events: UnboundedSender<UiEvent>,
        /// TODO(P1): hand to the agent and MCP spawn sites (§6).
        #[allow(dead_code)]
        children: ChildRegistry,
    }

    impl Backend {
        /// Build the backend and start its platform thread.
        pub fn new(
            paths: crate::paths::Paths,
            children: ChildRegistry,
            events: UnboundedSender<UiEvent>,
        ) -> Self {
            let bootstrap = Arc::new(Bootstrap::new(paths));
            let codex = CodexAuth::at_startup(&bootstrap.config());
            Self {
                platform: platform_backend(),
                engine: bootstrap.engine(),
                bootstrap,
                codex,
                sessions: HashMap::new(),
                events,
                children,
            }
        }

        /// Drain `UiRequest`s until the UI hangs up or asks to quit.
        pub async fn run(mut self, mut requests: UnboundedReceiver<UiRequest>) {
            let (internal_tx, mut internal_rx) = tokio::sync::mpsc::unbounded_channel();

            // §13: "Still handle NSWorkspaceDidWakeNotification /
            // WM_POWERBROADCAST, but for re-probing provider health and
            // clearing the degraded flags, not for re-warming sockets."
            self.spawn_power_watch(internal_tx.clone());

            // The first thing the UI should know is which providers exist and
            // what aibo currently believes about them.
            for (provider, health) in self.engine.health().snapshot() {
                if provider == ProviderId::CODEX {
                    continue; // Owned by the sign-in state machine below.
                }
                self.emit(UiEvent::ProviderHealth { provider, health });
            }
            // The Codex row has exactly one publisher, and this is it. §13's
            // health table says "Unknown" for a provider it has never probed,
            // which is indistinguishable from "signed out" — and a second
            // publisher would be worse than uninformative, because the row's
            // button means three different things depending on what it says.
            self.publish_codex_phase();

            loop {
                tokio::select! {
                    request = requests.recv() => match request {
                        Some(UiRequest::Quit) | None => break,
                        Some(request) => self.handle(request, &internal_tx),
                    },
                    Some(message) = internal_rx.recv() => self.handle_internal(message),
                }
            }

            // Cancel everything still in flight before the caller reaps children.
            self.engine.cancel_all();
        }

        fn spawn_power_watch(&self, internal: UnboundedSender<Internal>) {
            let platform = self.platform.clone();
            tokio::spawn(crate::diagnostics::supervise("power", async move {
                let mut stream = match platform.power_events() {
                    Ok(stream) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "no power-event stream; §13 wake re-probing is off");
                        return;
                    }
                };
                while let Some(event) = stream.next().await {
                    if event == PowerEvent::DidWake {
                        let _ = internal.send(Internal::Woke);
                    }
                }
            }));
        }

        fn emit(&self, event: UiEvent) {
            // The UI having hung up is not an error: it means the daemon exited
            // and this loop is about to notice.
            let _ = self.events.send(event);
        }

        fn handle(&mut self, request: UiRequest, internal: &UnboundedSender<Internal>) {
            match request {
                UiRequest::CaptureContext { session } => self.capture(session, internal.clone()),

                UiRequest::Submit {
                    session,
                    instruction,
                    surface,
                    role_override,
                } => self.submit(
                    session,
                    instruction,
                    Some(surface),
                    role_override,
                    internal.clone(),
                ),

                // §13: `esc`. The engine owns the token; a cancel for a session
                // it has already moved past is a no-op there, not here.
                UiRequest::Cancel { session } => self.engine.cancel(session),

                UiRequest::Insert { session, text } => self.insert(session, text),

                UiRequest::Copy { text } => self.copy(text),

                // §4: "Escalation is explicit, never automatic." This is the
                // user pressing ⌘↩ or the inline "Retry with Smart" button, so
                // the re-run is a new submission at a named role.
                UiRequest::Retry { session, role } => match self.replay(session) {
                    Some(last) => self.submit(
                        session,
                        last.instruction,
                        Some(last.surface),
                        role.or(last.role),
                        internal.clone(),
                    ),
                    None => tracing::warn!(%session, "nothing to retry for this session"),
                },

                // §3a. The one per-provider action the settings vocabulary
                // carries, and for Codex it means whichever of start / cancel /
                // sign out the current phase makes it mean — which is what the
                // button's label says.
                UiRequest::SignIn { provider } if provider == ProviderId::CODEX => {
                    self.codex_sign_in_pressed(internal.clone());
                }

                UiRequest::SignIn { provider } => {
                    // Every other provider authenticates with an API key, and
                    // there is no field to type one into yet.
                    // TODO(§10, §17): a key field per provider row.
                    tracing::warn!(%provider, "no sign-in flow for this provider yet");
                }

                // The rest need subsystems that exist but have no config to be
                // constructed from. Each logs rather than `todo!()`-ing: a tray
                // app must not die because the user clicked something
                // unfinished (§6).
                other => tracing::warn!(request = ?other, "unhandled UiRequest; not wired yet"),
            }
        }

        fn handle_internal(&mut self, message: Internal) {
            match message {
                Internal::Captured(captured) => {
                    let Captured {
                        session,
                        app,
                        target,
                        selection,
                        field,
                        clipboard,
                    } = *captured;
                    let state = self.sessions.entry(session).or_default();
                    state.app = app;
                    state.target = target;
                    state.selection = selection;
                    state.field = field;
                    state.clipboard = clipboard;
                }

                Internal::Woke => {
                    // Not "clear the degraded flags": the lid opening says
                    // nothing about whether the corporate proxy is back. What
                    // it does say is that waiting out a five-minute backoff is
                    // pointless, so the next request probes immediately (§13).
                    tracing::info!("woke; every degraded provider is due for a probe (§13)");
                    self.engine.health().probe_all_now();
                }

                Internal::CodexAuth(event) => self.handle_codex_auth(event),
            }
        }

        fn replay(&self, session: SessionId) -> Option<LastSubmission> {
            self.sessions.get(&session).and_then(|s| s.last.clone())
        }

        // -- Codex sign-in (§3a) ----------------------------------------------

        /// Tell the UI where the login stands.
        fn publish_codex_phase(&self) {
            self.publish_codex_detail(default_codex_detail(self.codex.phase));
        }

        fn publish_codex_detail(&self, detail: String) {
            self.emit(UiEvent::ProviderHealth {
                provider: ProviderId::CODEX,
                health: self.codex.phase.to_health(&detail),
            });
        }

        /// The Providers tab's one Codex action.
        ///
        /// Three meanings, disambiguated by the phase — and the button's label
        /// is derived from the same phase, so what the user reads is always
        /// what pressing it does.
        fn codex_sign_in_pressed(&mut self, internal: UnboundedSender<Internal>) {
            match codex_button_action(self.codex.phase) {
                CodexAction::Cancel => self.codex_cancel(),
                CodexAction::SignOut => self.codex_sign_out(internal),
                CodexAction::Start => self.codex_start(internal),
            }
        }

        fn codex_cancel(&mut self) {
            if let Some(cancel) = self.codex.cancel.take() {
                cancel.cancel();
            }
            self.codex.phase = CodexPhase::SignedOut;
            self.publish_codex_phase();
        }

        fn codex_start(&mut self, internal: UnboundedSender<Internal>) {
            let config = self.bootstrap.config();
            let Some(tokens) = self.bootstrap.codex_tokens(&config) else {
                self.codex.phase = CodexPhase::Failed;
                self.publish_codex_detail(
                    "No OAuth client id is configured for the Codex device flow (§3a). \
                     Set `[codex] client_id` or AIBO_CODEX_CLIENT_ID."
                        .to_owned(),
                );
                return;
            };

            let cancel = CancellationToken::new();
            self.codex.cancel = Some(cancel.clone());
            self.codex.phase = CodexPhase::Starting;
            self.publish_codex_phase();

            let client_id = config.codex.client_id();

            // Progress lines and the terminal outcome reach the loop from
            // **one** task, in order. Forwarding progress from a second task
            // would let a countdown tick overtake "signed in" and repaint the
            // card as "waiting for approval" for a login that had already
            // succeeded — which is the same class of bug as the button label
            // disagreeing with the action.
            tokio::spawn(crate::diagnostics::supervise("codex-signin", async move {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let flow = codex_signin::run(client_id, tokens, cancel, tx);
                tokio::pin!(flow);

                let outcome = loop {
                    tokio::select! {
                        // Biased: drain anything already queued before looking
                        // at whether the flow has finished.
                        biased;
                        Some(event) = rx.recv() => {
                            let _ = internal.send(Internal::CodexAuth(event));
                        }
                        outcome = &mut flow => break outcome,
                    }
                };

                // `try_recv`, not `recv().await`: the pinned future still owns
                // the sender until this scope ends, so awaiting here would
                // block forever on a channel that can never close. Everything
                // `run` sent is already queued — the sends are synchronous —
                // so draining what is present is exactly right.
                while let Ok(event) = rx.try_recv() {
                    let _ = internal.send(Internal::CodexAuth(event));
                }
                let _ = internal.send(Internal::CodexAuth(outcome));
            }));
        }

        fn codex_sign_out(&mut self, internal: UnboundedSender<Internal>) {
            let config = self.bootstrap.config();
            let tokens = self.bootstrap.codex_tokens(&config);
            let path = self.bootstrap.paths().config();
            let model = config.codex.model.clone();

            // Order matters. The config flag is what the next launch reads, so
            // it is written even if the keychain delete fails — a keychain
            // entry aibo no longer uses is inert, while an `enabled = true`
            // whose token is gone is a provider that 401s on every request.
            if let Err(error) = crate::config_file::write_codex(&path, false, &model) {
                tracing::error!(%error, "could not record the Codex sign-out in the config");
            }

            self.codex.phase = CodexPhase::SignedOut;
            self.publish_codex_phase();
            self.rebuild_engine();

            tokio::spawn(crate::diagnostics::supervise("codex-signout", async move {
                let Some(tokens) = tokens else { return };
                // `forget` clears the cached pair *and* the keychain entry.
                match tokens.forget().await {
                    Ok(()) => tracing::info!("cleared the Codex tokens from the keychain (§12)"),
                    Err(error) => tracing::error!(%error, "could not clear the Codex tokens"),
                }
                let _ = internal.send(Internal::CodexAuth(codex_signin::Event::Progress {
                    phase: CodexPhase::SignedOut,
                    detail: default_codex_detail(CodexPhase::SignedOut),
                }));
            }));
        }

        fn handle_codex_auth(&mut self, event: codex_signin::Event) {
            match event {
                codex_signin::Event::Progress { phase, detail } => {
                    // A late progress line from a run the user already
                    // cancelled must not resurrect the spinner.
                    if !accepts_progress(self.codex.phase, phase) {
                        tracing::debug!(?phase, "dropped a stale Codex progress line");
                        return;
                    }
                    self.codex.phase = phase;
                    self.publish_codex_detail(detail);
                }

                codex_signin::Event::Cancelled => {
                    self.codex.cancel = None;
                    self.codex.phase = CodexPhase::SignedOut;
                    self.publish_codex_phase();
                }

                codex_signin::Event::Failed { detail } => {
                    self.codex.cancel = None;
                    self.codex.phase = CodexPhase::Failed;
                    self.publish_codex_detail(detail);
                }

                codex_signin::Event::Succeeded { account } => {
                    self.codex.cancel = None;
                    self.codex.phase = CodexPhase::SignedIn;

                    // §12: the tokens are already in the keychain. What lands
                    // in the plaintext config is the two facts that are not
                    // secrets — that Codex is on, and which model it uses.
                    let config = self.bootstrap.config();
                    let path = self.bootstrap.paths().config();
                    if let Err(error) =
                        crate::config_file::write_codex(&path, true, &config.codex.model)
                    {
                        tracing::error!(%error, "signed in, but the config could not be written");
                        self.codex.phase = CodexPhase::Failed;
                        self.publish_codex_detail(format!(
                            "Signed in, but aibo could not record it: {error}"
                        ));
                        return;
                    }

                    // Without this the user completes a login and nothing
                    // changes until they restart, which reads as the login
                    // having failed.
                    self.rebuild_engine();

                    // The account id is logged rather than shown: a signed-in
                    // row is published as `Health::Ok`, which has nowhere to
                    // carry a sentence, and `CodexPhase::to_health` refuses to
                    // fake a `Degraded` in order to smuggle one through.
                    tracing::info!(
                        account = account.as_deref().unwrap_or("<no claim>"),
                        "Codex sign-in complete (§3a)"
                    );
                    self.publish_codex_phase();
                }
            }
        }

        /// Rebuild the engine so a provider set change takes effect at once.
        ///
        /// In-flight work belongs to the old engine and is cancelled: §13 says
        /// a request is bound to one session and one provider, and letting a
        /// stream outlive the registry it was dispatched from would report
        /// against a provider set that no longer exists.
        fn rebuild_engine(&mut self) {
            self.engine.cancel_all();
            self.engine = self.bootstrap.engine();
            tracing::info!(
                providers = ?self.engine.providers().ids(),
                "engine rebuilt after a provider change"
            );
            for (provider, health) in self.engine.health().snapshot() {
                if provider != ProviderId::CODEX {
                    self.emit(UiEvent::ProviderHealth { provider, health });
                }
            }
        }

        // -- capture ---------------------------------------------------------

        /// §8's corrected capture rule, in full.
        ///
        /// The instant snapshot (`focused_app_ref`) happens inline; everything
        /// deadline-bounded happens in a spawned task, so a hung target app
        /// delays only its own context — never the panel, never the next
        /// request.
        ///
        /// Without a step-1 snapshot there is nothing to read *from*: §7's
        /// deferred capture methods take the `AppRef` as their subject, and this
        /// task runs after the panel has taken focus, so there is no honest
        /// value to substitute — "whatever is frontmost now" is aibo. A failed
        /// snapshot therefore reports empty context rather than capturing
        /// something and attributing it to the wrong application.
        fn capture(&mut self, session: SessionId, internal: UnboundedSender<Internal>) {
            self.sessions.entry(session).or_default();

            let platform = self.platform.clone();
            let events = self.events.clone();

            // §8 step 1: instant, cannot block, taken before the panel appears.
            let app_ref = match platform.focused_app_ref() {
                Ok(app_ref) => app_ref,
                Err(error) => {
                    // Not fatal: §8 requires the panel to tolerate context that
                    // never arrives. Toast it, then resolve the "reading
                    // context…" chip to "unavailable" so it cannot spin forever.
                    let _ = events.send(UiEvent::ContextFailed {
                        session,
                        error: Arc::new(error),
                    });
                    let _ = events.send(UiEvent::Context {
                        session,
                        app: None,
                        field: None,
                        selection: None,
                        clipboard: None,
                    });
                    return;
                }
            };

            tokio::spawn(crate::diagnostics::supervise("capture", async move {
                let captured = capture_context(platform.as_ref(), &app_ref, session).await;

                let ui = UiEvent::Context {
                    session,
                    app: captured.app.clone(),
                    field: captured.field.clone().map(Box::new),
                    selection: captured.selection.clone(),
                    clipboard: captured.clipboard.clone().map(Box::new),
                };
                let _ = internal.send(Internal::Captured(Box::new(captured)));
                let _ = events.send(ui);
            }));
        }

        // -- submit ----------------------------------------------------------

        /// Hand the request to [`Engine::run`] and forward what comes back.
        ///
        /// Everything interesting happens inside the engine; what this owns is
        /// the translation from [`SessionEvent`] to [`UiEvent`] and the §13
        /// rule that a partial result is shown, never inserted.
        fn submit(
            &mut self,
            session: SessionId,
            instruction: String,
            surface: Option<Surface>,
            role_override: Option<Role>,
            internal: UnboundedSender<Internal>,
        ) {
            let state = self.sessions.entry(session).or_default();
            state.last = Some(LastSubmission {
                instruction: instruction.clone(),
                surface: surface.unwrap_or(Surface::Ask),
                role: role_override,
            });

            let submission = Submission {
                session,
                instruction,
                surface,
                role_override,
                capture: Capture {
                    app: state.app.clone(),
                    field: state.field.clone(),
                    selection: state.selection.clone(),
                    clipboard: state.clipboard.clone(),
                },
                conversation_id: None,
                history: Vec::new(),
            };

            let engine = self.engine.clone();
            let events = self.events.clone();
            let (sink, mut session_events) = EventSink::channel();

            // The event pump and the request run concurrently in one task, so
            // the UI sees tokens as they arrive rather than in a burst at the
            // end.
            tokio::spawn(crate::diagnostics::supervise("submit", async move {
                let pump = {
                    let events = events.clone();
                    tokio::spawn(async move {
                        while let Some(event) = session_events.recv().await {
                            for ui in translate(session, event) {
                                let _ = events.send(ui);
                            }
                        }
                    })
                };

                let outcome = engine.run(submission, &sink).await;
                drop(sink);
                let _ = pump.await;

                // §13's invariant, restated at the one call site that could
                // violate it: aibo never auto-inserts. Even a *completed*
                // result waits for `UiRequest::Insert`, which is the user
                // accepting it. `insertable_text` exists so that a future
                // auto-apply feature cannot reach for a partial by accident.
                match &outcome {
                    Outcome::Completed(completion) => tracing::info!(
                        provider = %completion.provider,
                        model = %completion.model,
                        latency_ms = completion.latency_ms,
                        escalate = completion.offer_escalation,
                        "completed"
                    ),
                    Outcome::Partial { reason, .. } => {
                        tracing::info!(?reason, "partial result; not insertable (§13)");
                    }
                    Outcome::Failed(error) => {
                        tracing::info!(%error, "request failed");
                        // §3a: a Codex `Auth` failure is the one health signal
                        // the settings row must not miss — it means the stored
                        // pair is dead and only a fresh login fixes it. It goes
                        // through the sign-in state machine so the row's phase
                        // and the button's meaning move together.
                        if let AiboError::Auth { provider, kind } = error.as_ref()
                            && *provider == ProviderId::CODEX
                        {
                            let _ =
                                internal.send(Internal::CodexAuth(codex_signin::Event::Failed {
                                    detail: format!(
                                        "Your ChatGPT sign-in is no longer valid ({kind}). \
                                         Sign in again to keep using Codex."
                                    ),
                                }));
                        }
                    }
                }
            }));
        }

        // -- write-back ------------------------------------------------------

        /// §8: restore focus *with confirmation*, validate, then one atomic
        /// paste.
        fn insert(&mut self, session: SessionId, text: String) {
            let target = self.sessions.get(&session).and_then(|s| s.target.clone());
            let platform = self.platform.clone();
            let events = self.events.clone();

            tokio::spawn(crate::diagnostics::supervise("insert", async move {
                let Some(target) = target else {
                    // No captured target means aibo does not know where "here"
                    // is. §8: offer copy instead, never guess.
                    let _ = events.send(UiEvent::Failed {
                        session,
                        error: Arc::new(AiboError::InsertFailed {
                            reason: InsertFailure::Cancelled,
                        }),
                    });
                    return;
                };

                let event = match insert_sequence(platform.as_ref(), &target, &text).await {
                    Ok(()) => UiEvent::Inserted { session },
                    Err(error) => UiEvent::Failed {
                        session,
                        error: Arc::new(error),
                    },
                };
                let _ = events.send(event);
            }));
        }

        /// Put text on the clipboard.
        ///
        /// This does *not* go through `PlatformBackend`, which exposes a
        /// clipboard **read** only — see the report's cross-crate notes.
        fn copy(&self, text: String) {
            tokio::spawn(crate::diagnostics::supervise("copy", async move {
                // arboard is blocking and touches the pasteboard; `spawn_blocking`
                // keeps it off the async workers.
                let joined = tokio::task::spawn_blocking(move || {
                    arboard::Clipboard::new().and_then(|mut c| c.set_text(text))
                })
                .await;

                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "could not write the clipboard"),
                    Err(error) => tracing::warn!(%error, "the clipboard task failed"),
                }
            }));
        }
    }

    /// What pressing the Codex card's single button does right now (§3a).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CodexAction {
        /// Begin a device-code login.
        Start,
        /// Abandon the one in flight.
        Cancel,
        /// Forget the tokens and drop the provider.
        SignOut,
    }

    /// The action a press means, given the phase.
    ///
    /// Paired with `aibo_ui::settings::codex_action_label`, which words the same
    /// decision. The two are asserted to agree, because one control with three
    /// meanings is only safe while its label is always the true one.
    const fn codex_button_action(phase: CodexPhase) -> CodexAction {
        match phase {
            CodexPhase::SignedIn => CodexAction::SignOut,
            CodexPhase::Starting | CodexPhase::AwaitingApproval | CodexPhase::Exchanging => {
                CodexAction::Cancel
            }
            CodexPhase::SignedOut | CodexPhase::Failed => CodexAction::Start,
        }
    }

    /// Whether a progress line from the login task still applies.
    ///
    /// The countdown ticker keeps emitting for as long as its `select!` is
    /// alive, so pressing "Cancel" races with an in-flight tick. The rule is
    /// the general one rather than a patch for that single case: **once the
    /// login has reached a terminal phase, no in-flight line applies to it.**
    /// Narrowing this to "…unless the user cancelled" would leave a tick able
    /// to repaint a *successful* login as "waiting for approval", which is the
    /// same defect wearing a different hat.
    const fn accepts_progress(current: CodexPhase, incoming: CodexPhase) -> bool {
        let settled = matches!(
            current,
            CodexPhase::SignedOut | CodexPhase::SignedIn | CodexPhase::Failed
        );
        !(settled && incoming.in_flight())
    }

    /// The standing sentence for a Codex phase that carries no detail of its
    /// own — the ones a device flow passes through without anything to say.
    fn default_codex_detail(phase: CodexPhase) -> String {
        use aibo_ui::settings::codex_text;
        match phase {
            CodexPhase::SignedOut => codex_text::SIGNED_OUT.to_owned(),
            CodexPhase::SignedIn => codex_text::SIGNED_IN.to_owned(),
            CodexPhase::Starting => "Asking OpenAI for a device code…".to_owned(),
            CodexPhase::AwaitingApproval => "Waiting for approval…".to_owned(),
            CodexPhase::Exchanging => "Approved. Storing the tokens…".to_owned(),
            CodexPhase::Failed => "Sign-in failed.".to_owned(),
        }
    }

    /// §8 step 3, as one function so its contract is testable.
    ///
    /// **Every read is aimed at `app_ref`** — the snapshot taken on hotkey-down,
    /// before the panel appeared. §7 makes this the parameter rather than an
    /// implicit "frontmost", because by the time this runs the panel holds focus
    /// and "frontmost" is aibo: the app identity would be aibo's bundle id, the
    /// focused element would be aibo's own panel text box (so `field.prefix`
    /// would be the query the user just typed), and §12's clipboard denylist
    /// would be attributed to aibo and could never match.
    ///
    /// Each read gets its own deadline rather than one shared budget: a slow AX
    /// read must not eat the clipboard's.
    async fn capture_context(
        platform: &dyn PlatformBackend,
        app_ref: &AppRef,
        session: SessionId,
    ) -> Captured {
        let app = platform.focused_app(app_ref, AX_DEADLINE).await.ok();
        let field = platform
            .text_field_context(app_ref, AX_DEADLINE)
            .await
            .ok()
            .flatten();
        // Only these two get the longer budget — they are the two that may fall
        // back to a synthetic copy chord (§8, §12).
        let selection = platform
            .selected_text(app_ref, CAPTURE_DEADLINE)
            .await
            .ok()
            .flatten();
        let clipboard = platform.clipboard(app_ref, CAPTURE_DEADLINE).await.ok();

        let target = InsertTarget {
            app_ref: app_ref.clone(),
            // TODO(cross-crate): no backend exposes the focused element's
            // identity from a *capture* call, only from inside
            // `validate_target`. `None` means the element check is skipped; the
            // pid, window and content checks still run.
            focused_element: None,
            selection_hash: selection.as_deref().map(content_hash),
            prefix_hash: field.as_ref().map(|f| content_hash(&f.prefix)),
        };

        Captured {
            session,
            app,
            target: Some(target),
            selection,
            field,
            clipboard,
        }
    }

    /// §8's insert sequence, in the order §8 fixes it — as one function so the
    /// order itself can be asserted on.
    ///
    /// ```text
    /// 1. hide the panel            — the UI's half, before `UiRequest::Insert`
    /// 2. restore_focus(target)     — and CONFIRM it landed, bounded retry
    /// 3. validate_target(target)
    /// 4. one atomic paste
    /// ```
    ///
    /// **Validate comes after restore, and that is not cosmetic.** Validation's
    /// first check is `frontmost pid == target pid`. Run before the restore, it
    /// compares aibo's pid — the panel is what has focus — against the target's,
    /// so it returns `false` unconditionally, every insert becomes
    /// `InsertFailed { Cancelled }` ("target changed, copy instead"), and the
    /// feature cannot work at all. The confirm-and-retry loop inside
    /// `restore_focus` is itself the proof that aibo is expected to hold focus
    /// at that point.
    ///
    /// A `false` from validation *after* a confirmed restore is the real signal
    /// it was always meant to be: the user switched apps, closed the tab, or
    /// edited the text. That maps to `Cancelled`, whose §13 treatment is a toast
    /// with the result left in the panel — never a paste over the wrong content,
    /// which is unrecoverable.
    async fn insert_sequence(
        platform: &dyn PlatformBackend,
        target: &InsertTarget,
        text: &str,
    ) -> std::result::Result<(), AiboError> {
        // Step 2. An unconfirmed restore races and pastes into the wrong window
        // — "the most damaging bug this product can ship" (§8).
        platform
            .restore_focus(&target.app_ref, FOCUS_DEADLINE)
            .await?;

        // Step 3, now that the target is frontmost again and the question is
        // answerable.
        if !matches!(platform.validate_target(target).await, Ok(true)) {
            return Err(AiboError::InsertFailed {
                reason: InsertFailure::Cancelled,
            });
        }

        // Step 4. §13: one paste, never chunked, never incremental. That
        // invariant is what makes undo and partial failure tractable.
        platform
            .insert_text(text, InsertMode::PasteAndRestore)
            .await
    }

    /// [`SessionEvent`] → [`UiEvent`].
    ///
    /// A `Vec` because §14's cost event feeds two UI surfaces — the per-turn
    /// footer and the monthly meter — and they are separate variants.
    ///
    /// Currency formatting lives here rather than in `aibo-session` because
    /// `UiEvent::Cost` carries an *already formatted* label and the display
    /// currency is a settings concern.
    fn translate(session: SessionId, event: SessionEvent) -> Vec<UiEvent> {
        match event {
            SessionEvent::Routed {
                surface,
                role,
                rule,
            } => {
                tracing::info!(?surface, ?role, rule, "routed (§4)");
                Vec::new()
            }

            SessionEvent::Skipped { provider, reason } => {
                tracing::info!(%provider, %reason, "chain entry skipped");
                Vec::new()
            }

            SessionEvent::Dispatched {
                provider,
                model,
                substituted_for,
            } => vec![UiEvent::Dispatched {
                session,
                provider,
                model,
                substituted_for,
            }],

            SessionEvent::FirstToken { elapsed_ms } => {
                vec![UiEvent::FirstToken {
                    session,
                    elapsed_ms,
                }]
            }

            SessionEvent::Stream(event) => vec![UiEvent::Stream { session, event }],

            // The Codex row is published by the sign-in state machine alone
            // (§3a). Forwarding §13's health here too would give it two
            // publishers, and the second one has no idea whether the user is
            // signed in — so an `Unavailable` from a dropped Wi-Fi connection
            // would repaint the row as "Sign in with ChatGPT" while the button
            // behind it still meant "Sign out". A control with three meanings
            // is only safe while exactly one thing decides which.
            //
            // Nothing is lost: an auth failure reaches the row through
            // `Internal::CodexAuth`, and every other §13 treatment for a failed
            // request belongs to the panel, not to a settings row.
            SessionEvent::ProviderHealth { provider, health } if provider != ProviderId::CODEX => {
                vec![UiEvent::ProviderHealth { provider, health }]
            }

            SessionEvent::ProviderHealth { health, .. } => {
                tracing::debug!(
                    ?health,
                    "codex health is owned by the sign-in state machine"
                );
                Vec::new()
            }

            SessionEvent::Cost {
                usage,
                cost_micros,
                committed_micros,
            } => vec![
                UiEvent::Cost {
                    session,
                    label: format_micros(cost_micros),
                    usage,
                },
                UiEvent::Spend {
                    label: format_micros(Some(committed_micros)),
                    // TODO(settings): the fraction needs the configured monthly
                    // cap, which lives in `EngineConfig`. Left `None` rather
                    // than guessed — a meter showing a made-up percentage is
                    // worse than one showing none.
                    fraction_of_cap: None,
                },
            ],

            SessionEvent::Failed(error) => vec![UiEvent::Failed { session, error }],

            // `SessionEvent` is `#[non_exhaustive]`: a new variant must not
            // break the binary, and silently dropping it is the right default —
            // a UI cannot render an event it has no state for.
            other => {
                tracing::debug!(event = ?other, "unhandled SessionEvent");
                Vec::new()
            }
        }
    }

    /// §14's micros, as a display string.
    ///
    /// `None` is *"cost unknown"*, never `0.00`: §14 is explicit that reporting
    /// zero for a model whose price aibo does not know is worse than reporting
    /// nothing.
    fn format_micros(micros: Option<u64>) -> String {
        match micros {
            Some(m) => format!("{:.4}", m as f64 / 1_000_000.0),
            None => "—".to_owned(),
        }
    }

    /// Stable-within-a-process content hash, matching `aibo-platform`'s.
    ///
    /// macOS exports `macos::content_hash`; the Windows equivalent
    /// (`windows::text_hash`) is `pub(crate)`. Both are `DefaultHasher` over the
    /// string, so this is byte-for-byte equivalent — but the duplication is a
    /// real smell and the fix belongs in `aibo-core` (see the report).
    fn content_hash(text: &str) -> u64 {
        use std::hash::{DefaultHasher, Hash as _, Hasher as _};
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        hasher.finish()
    }

    /// Construct the platform backend for this OS.
    #[cfg(target_os = "macos")]
    fn platform_backend() -> Arc<dyn PlatformBackend> {
        Arc::new(aibo_platform::macos::MacosBackend::default())
    }

    #[cfg(target_os = "windows")]
    fn platform_backend() -> Arc<dyn PlatformBackend> {
        // §9: `WindowsBackend::new` opts into Per-Monitor-V2 DPI awareness and
        // must therefore run before the first window exists — which is why the
        // backend is built here, ahead of `aibo_ui::run`, and not lazily.
        Arc::new(
            aibo_platform::windows::WindowsBackend::new()
                .expect("the Windows platform threads could not start"),
        )
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn platform_backend() -> Arc<dyn PlatformBackend> {
        // §2 locks the shipping targets to macOS and Windows. Keeping the binary
        // buildable elsewhere is worth more than a `compile_error!`, but there
        // is no third implementation and writing one would be exactly the
        // "invent platform behaviour" the plan forbids.
        todo!("no PlatformBackend for this OS; §2 ships macOS and Windows only")
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Mutex;

        use aibo_core::types::{ClipboardKind, DisplayInfo, Permission, PermissionStatus, Rect};

        use super::*;

        /// The app that had focus when the hotkey fired.
        const TARGET_PID: i32 = 4242;
        /// aibo's own panel, which owns the foreground by the time §8 step 3
        /// runs. Every value below that mentions it is a value the product must
        /// never produce.
        const AIBO_PID: i32 = 99;

        fn target_ref() -> AppRef {
            AppRef {
                pid: TARGET_PID,
                window: Some(7),
            }
        }

        fn aibo_ref() -> AppRef {
            AppRef {
                pid: AIBO_PID,
                window: Some(1),
            }
        }

        /// A `PlatformBackend` that models the one fact §7 and §8 turn on: by
        /// the time the deferred capture and the insert run, **aibo owns the
        /// foreground**.
        ///
        /// Reads are answered from whichever app the caller names, exactly as a
        /// correct backend does — so a caller that names the wrong app gets the
        /// wrong app's data rather than an error, which is what made the real
        /// defect invisible. `restore_focus` is what moves the foreground back.
        #[derive(Default)]
        struct FakePlatform {
            /// Ordered log of the write-back calls, for the §8 sequence.
            calls: Mutex<Vec<&'static str>>,
            /// Whose pid currently owns the foreground.
            foreground: Mutex<Option<i32>>,
            /// Which `AppRef` each capture method was handed.
            asked_about: Mutex<Vec<(&'static str, AppRef)>>,
            /// Where focus actually lands after `restore_focus`. `None` means
            /// "on the app it was asked to restore", the ordinary case; a value
            /// models the user having switched apps in the meantime.
            restore_lands_on: Option<i32>,
        }

        impl FakePlatform {
            /// aibo's panel is up and focused: the state every deferred call
            /// actually runs in.
            fn panel_focused() -> Self {
                Self {
                    foreground: Mutex::new(Some(AIBO_PID)),
                    ..Self::default()
                }
            }

            fn note(&self, call: &'static str) {
                self.calls.lock().unwrap().push(call);
            }

            fn calls(&self) -> Vec<&'static str> {
                self.calls.lock().unwrap().clone()
            }

            fn asked_about(&self, method: &str) -> AppRef {
                self.asked_about
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(m, _)| *m == method)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_else(|| panic!("{method} was never called"))
            }

            fn record(&self, method: &'static str, of: &AppRef) {
                self.asked_about.lock().unwrap().push((method, of.clone()));
            }

            /// Bundle identifier of whichever app is named.
            fn identifier(pid: i32) -> String {
                match pid {
                    TARGET_PID => "com.microsoft.VSCode".to_owned(),
                    AIBO_PID => "com.aibo.aibo".to_owned(),
                    other => format!("pid.{other}"),
                }
            }
        }

        #[async_trait::async_trait]
        impl PlatformBackend for FakePlatform {
            fn focused_app_ref(&self) -> aibo_core::error::Result<AppRef> {
                Ok(target_ref())
            }

            fn active_display(&self) -> aibo_core::error::Result<DisplayInfo> {
                let bounds = Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                };
                Ok(DisplayInfo {
                    id: 0,
                    bounds,
                    visible_frame: bounds,
                    scale_factor: 1.0,
                    is_primary: true,
                })
            }

            fn secure_input_active(&self) -> bool {
                false
            }

            async fn focused_app(
                &self,
                of: &AppRef,
                _timeout: Duration,
            ) -> aibo_core::error::Result<AppInfo> {
                self.record("focused_app", of);
                let identifier = Self::identifier(of.pid);
                Ok(AppInfo {
                    app_ref: of.clone(),
                    is_code_app: identifier == "com.microsoft.VSCode",
                    identifier,
                    display_name: String::new(),
                })
            }

            async fn selected_text(
                &self,
                of: &AppRef,
                _timeout: Duration,
            ) -> aibo_core::error::Result<Option<String>> {
                self.record("selected_text", of);
                Ok(Some(match of.pid {
                    TARGET_PID => "the user's document selection".to_owned(),
                    _ => "whatever is in aibo's own panel".to_owned(),
                }))
            }

            async fn text_field_context(
                &self,
                of: &AppRef,
                _timeout: Duration,
            ) -> aibo_core::error::Result<Option<FieldContext>> {
                self.record("text_field_context", of);
                Ok(Some(FieldContext {
                    prefix: match of.pid {
                        TARGET_PID => "fn main() {".to_owned(),
                        // What `AXFocusedUIElement` returns once the panel has
                        // focus: the query the user just typed into aibo.
                        _ => "rewrite this more politely".to_owned(),
                    },
                    suffix: String::new(),
                    caret: None,
                    label: None,
                    is_secure: false,
                    ime_active: false,
                    truncated: false,
                    caret_bounds: None,
                }))
            }

            async fn clipboard(
                &self,
                owner_hint: &AppRef,
                _timeout: Duration,
            ) -> aibo_core::error::Result<ClipboardItem> {
                self.record("clipboard", owner_hint);
                Ok(ClipboardItem {
                    kind: ClipboardKind::Text,
                    text: Some("clip".to_owned()),
                    files: Vec::new(),
                    concealed: false,
                    transient: false,
                    source_app: Some(Self::identifier(owner_hint.pid)),
                    sequence: 1,
                    restorable: true,
                })
            }

            async fn insert_text(
                &self,
                _text: &str,
                _mode: InsertMode,
            ) -> aibo_core::error::Result<()> {
                self.note("insert_text");
                Ok(())
            }

            async fn replace_selection(&self, _text: &str) -> aibo_core::error::Result<()> {
                self.note("replace_selection");
                Ok(())
            }

            /// §8's first check is `frontmost pid == target pid`. Modelled
            /// literally, because that literal check is what fails when this is
            /// called before the restore.
            async fn validate_target(
                &self,
                target: &InsertTarget,
            ) -> aibo_core::error::Result<bool> {
                self.note("validate_target");
                Ok(*self.foreground.lock().unwrap() == Some(target.app_ref.pid))
            }

            async fn restore_focus(
                &self,
                prev: &AppRef,
                _timeout: Duration,
            ) -> aibo_core::error::Result<()> {
                self.note("restore_focus");
                *self.foreground.lock().unwrap() = Some(self.restore_lands_on.unwrap_or(prev.pid));
                Ok(())
            }

            fn permission_status(&self, _p: Permission) -> PermissionStatus {
                PermissionStatus::Granted
            }

            fn request_permission(&self, _p: Permission) -> aibo_core::error::Result<()> {
                Ok(())
            }

            fn power_events(
                &self,
            ) -> aibo_core::error::Result<aibo_core::types::BoxStream<'static, PowerEvent>>
            {
                Ok(Box::pin(futures::stream::empty()))
            }
        }

        /// **F1 regression.** Every deferred read must be attributed to the
        /// `AppRef` snapshotted on hotkey-down — never re-resolved from
        /// "frontmost", which by then is aibo's own panel.
        ///
        /// Fails if `capture_context` drops the snapshot: the fake answers about
        /// whichever app it is handed, so a frontmost-based caller silently gets
        /// aibo's bundle id, aibo's panel text and aibo-attributed clipboard —
        /// exactly the shape of the shipped defect.
        #[tokio::test]
        async fn capture_reads_the_snapshotted_app_not_the_frontmost_one() {
            let platform = FakePlatform::panel_focused();
            let snapshot = target_ref();

            let captured = capture_context(&platform, &snapshot, SessionId::nil()).await;

            for method in [
                "focused_app",
                "text_field_context",
                "selected_text",
                "clipboard",
            ] {
                assert_eq!(
                    platform.asked_about(method),
                    snapshot,
                    "{method} was aimed at the wrong application; §7 requires the AppRef taken \
                     on hotkey-down, and re-resolving 'frontmost' here reads aibo's own panel"
                );
            }

            // …and the consequences §7 enumerates, asserted on the values that
            // actually reach routing, the prompt and the store.
            let app = captured.app.expect("app identity");
            assert_eq!(app.identifier, "com.microsoft.VSCode");
            assert!(app.is_code_app, "§4 routing reads this");
            assert_eq!(app.app_ref.pid, TARGET_PID);
            assert_eq!(
                captured.field.expect("field").prefix,
                "fn main() {",
                "the captured prefix is the user's document, not their panel query"
            );
            assert_eq!(
                captured.selection.as_deref(),
                Some("the user's document selection")
            );
            assert_eq!(
                captured.clipboard.expect("clipboard").source_app.as_deref(),
                Some("com.microsoft.VSCode"),
                "§12's clipboard denylist matches on this field; attributing it to aibo makes \
                 the denylist inert (F3)"
            );

            // Proof the assertions above have teeth rather than passing by
            // coincidence: the same fake, asked about the app that really is
            // frontmost, hands back precisely the values the defect shipped.
            let wrong = platform
                .focused_app(&aibo_ref(), AX_DEADLINE)
                .await
                .expect("app identity");
            assert_eq!(wrong.identifier, "com.aibo.aibo");
            assert!(!wrong.is_code_app);
            assert_eq!(
                platform
                    .text_field_context(&aibo_ref(), AX_DEADLINE)
                    .await
                    .expect("field")
                    .expect("field")
                    .prefix,
                "rewrite this more politely"
            );
        }

        /// **F3 regression.** The clipboard's `owner_hint` is the snapshot, so a
        /// password manager on §12's denylist is actually reachable. Attributed
        /// to the frontmost app it would always be aibo, which is on no
        /// denylist.
        #[tokio::test]
        async fn the_clipboard_owner_hint_can_name_a_denylisted_app() {
            let platform = FakePlatform::panel_focused();
            let snapshot = target_ref();

            let item = platform
                .clipboard(&snapshot, CAPTURE_DEADLINE)
                .await
                .expect("clipboard");

            assert_ne!(
                item.source_app.as_deref(),
                Some("com.aibo.aibo"),
                "the clipboard was attributed to aibo, so no denylist entry can ever match"
            );
            assert_eq!(platform.asked_about("clipboard"), snapshot);
        }

        /// **F2 regression.** §8 fixes the order: restore focus (confirmed),
        /// *then* validate, *then* one paste.
        ///
        /// The fake models the real mechanism rather than the symptom —
        /// `validate_target` compares the live foreground pid against the
        /// target's, and `restore_focus` is what changes it. With validation
        /// first, aibo still holds the foreground, so it answers `false` and the
        /// insert dies as `Cancelled`. That is not a hypothetical ordering
        /// preference; it is every insert failing.
        #[tokio::test]
        async fn the_insert_sequence_restores_focus_before_validating() {
            let platform = FakePlatform::panel_focused();
            let target = InsertTarget {
                app_ref: target_ref(),
                focused_element: None,
                selection_hash: None,
                prefix_hash: None,
            };

            insert_sequence(&platform, &target, "rewritten text")
                .await
                .expect("the insert must succeed once the order is right");

            assert_eq!(
                platform.calls(),
                vec!["restore_focus", "validate_target", "insert_text"],
                "§8's insert sequence is ordered and the order is load-bearing: validating \
                 while aibo still holds focus compares aibo's pid against the target's and \
                 fails every time"
            );
        }

        /// The other half of the same rule. Reordering must not have turned
        /// validation into a rubber stamp: when the target *really has* changed
        /// — the user switched apps while the model was streaming — the answer
        /// is still "copy instead", and nothing is pasted.
        #[tokio::test]
        async fn a_genuinely_changed_target_is_still_refused() {
            let mut platform = FakePlatform::panel_focused();
            // The user moved on: whatever `restore_focus` does, the foreground
            // ends up somewhere that is not the capture target.
            platform.restore_lands_on = Some(31337);

            let target = InsertTarget {
                app_ref: target_ref(),
                focused_element: None,
                selection_hash: None,
                prefix_hash: None,
            };

            let outcome = insert_sequence(&platform, &target, "rewritten text").await;

            assert!(
                matches!(
                    outcome,
                    Err(AiboError::InsertFailed {
                        reason: InsertFailure::Cancelled
                    })
                ),
                "a real target change must refuse, not paste: {outcome:?}"
            );
            assert_eq!(
                platform.calls(),
                vec!["restore_focus", "validate_target"],
                "the paste must not be reached once validation refuses"
            );
        }

        // -- Codex sign-in (§3a) ------------------------------------------

        /// **The safety property of collapsing three actions into one button.**
        /// The label the settings window draws and the action the backend takes
        /// are decided in two different crates from the same phase; if they
        /// ever disagree, a user reading "Sign in" gets signed out. Asserted
        /// over every phase rather than spot-checked.
        #[test]
        fn the_button_always_does_what_its_label_says() {
            use aibo_ui::settings::{codex_action_label, codex_text};

            for phase in [
                CodexPhase::SignedOut,
                CodexPhase::Starting,
                CodexPhase::AwaitingApproval,
                CodexPhase::Exchanging,
                CodexPhase::SignedIn,
                CodexPhase::Failed,
            ] {
                let expected = match codex_button_action(phase) {
                    CodexAction::Start => codex_text::SIGN_IN,
                    CodexAction::Cancel => codex_text::CANCEL,
                    CodexAction::SignOut => codex_text::SIGN_OUT,
                };
                assert_eq!(
                    codex_action_label(phase),
                    expected,
                    "{phase:?}: the label and the action disagree"
                );
            }
        }

        /// A settled login must stay settled. The countdown ticker races with
        /// the outcome, so a stray in-flight line would otherwise repaint a
        /// finished flow — leaving the button reading "Cancel sign-in" for a
        /// login that had already succeeded, failed or been cancelled.
        ///
        /// Asserted over the whole cross-product rather than the one case that
        /// prompted it, because the narrow version of this rule is a bug.
        #[test]
        fn a_late_progress_line_cannot_resurrect_a_settled_login() {
            let settled = [
                CodexPhase::SignedOut,
                CodexPhase::SignedIn,
                CodexPhase::Failed,
            ];
            let in_flight = [
                CodexPhase::Starting,
                CodexPhase::AwaitingApproval,
                CodexPhase::Exchanging,
            ];

            for current in settled {
                for incoming in in_flight {
                    assert!(
                        !accepts_progress(current, incoming),
                        "{incoming:?} must not reopen a login already at {current:?}"
                    );
                }
                // Terminal outcomes always land: the task must be able to say
                // it finished, and approval can cross with a cancel.
                for incoming in settled {
                    assert!(accepts_progress(current, incoming));
                }
            }

            // An ordinary run is unaffected.
            for current in in_flight {
                for incoming in in_flight.iter().chain(settled.iter()) {
                    assert!(accepts_progress(current, *incoming));
                }
            }
        }

        /// Startup must read the config, not the keychain: a consent dialog
        /// before the user has done anything is what §17 rules out. The two
        /// states the config can express map to the two phases.
        #[test]
        fn the_startup_phase_comes_from_the_config_not_the_keychain() {
            let signed_out = aibo_session::Config::from_toml_str("").unwrap();
            assert_eq!(
                CodexAuth::at_startup(&signed_out).phase,
                CodexPhase::SignedOut
            );

            let signed_in =
                aibo_session::Config::from_toml_str("[codex]\nenabled = true\n").unwrap();
            let state = CodexAuth::at_startup(&signed_in);
            assert_eq!(state.phase, CodexPhase::SignedIn);
            assert!(
                state.cancel.is_none(),
                "nothing is in flight at startup, so there is nothing to cancel"
            );
        }

        /// Every phase must have something to say. A card with an empty body is
        /// the silent failure §13 opens by forbidding.
        #[test]
        fn every_phase_has_a_sentence_and_it_survives_the_channel() {
            for phase in [
                CodexPhase::SignedOut,
                CodexPhase::Starting,
                CodexPhase::AwaitingApproval,
                CodexPhase::Exchanging,
                CodexPhase::SignedIn,
                CodexPhase::Failed,
            ] {
                let detail = default_codex_detail(phase);
                assert!(!detail.trim().is_empty(), "{phase:?} has no copy");

                let (read_phase, read_detail) = CodexPhase::read(&phase.to_health(&detail));
                assert_eq!(read_phase, phase);
                assert!(
                    !read_detail.trim().is_empty(),
                    "{phase:?} reached the card with nothing to read"
                );
                // Every phase except `SignedIn` rides in `Health::Degraded` and
                // keeps its own sentence; `SignedIn` is `Health::Ok`, which
                // cannot carry one, so the canonical copy is what shows.
                if phase == CodexPhase::SignedIn {
                    assert_eq!(read_detail, aibo_ui::settings::codex_text::SIGNED_IN);
                } else {
                    assert_eq!(read_detail, detail);
                }
            }
        }

        #[test]
        fn an_unknown_cost_is_not_reported_as_zero() {
            assert_eq!(format_micros(None), "—");
            assert_eq!(format_micros(Some(0)), "0.0000");
            assert_eq!(format_micros(Some(1_500_000)), "1.5000");
        }

        #[test]
        fn a_routed_event_produces_no_ui_traffic() {
            let events = translate(
                SessionId::nil(),
                SessionEvent::Routed {
                    surface: Surface::Ask,
                    role: Role::Smart,
                    rule: "ask_is_smart",
                },
            );
            assert!(events.is_empty(), "routing is a log line, not a UI state");
        }

        #[test]
        fn a_cost_event_feeds_both_meters() {
            let events = translate(
                SessionId::nil(),
                SessionEvent::Cost {
                    usage: aibo_core::types::Usage::default(),
                    cost_micros: Some(300),
                    committed_micros: 300,
                },
            );
            assert_eq!(events.len(), 2);
        }
    }
}
