//! S4 — Insert reliability (§20).
//!
//! > Paste-and-restore vs `SendInput`/`CGEventPost` across the app set, Unicode
//! > and 5 KB inserts. **Does clipboard save/restore round-trip?**
//! > If it fails: paste-only, always ask before clobbering the clipboard. — §20
//!
//! Two insert methods, ten payloads, one app at a time. Where the target is
//! AX-readable the result is verified automatically and reported as a character
//! index of first divergence; everywhere else the operator reads the screen and
//! the harness tells them exactly what to look for.
//!
//! **This binary types into whatever is frontmost.** It will happily paste 5 KB
//! into your production Slack. Point it at `testapps/aibo-axtarget` or a scratch
//! document.

mod clipboard;
mod payload;
#[cfg(target_os = "macos")]
mod verify;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

/// How the text gets into the target app.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Method {
    /// §8's default: put the text on the pasteboard, synthesise ⌘V / Ctrl+V,
    /// then restore the pasteboard.
    Paste,
    /// §8's short-insert path: `CGEvent::set_string` on macOS (via enigo),
    /// `SendInput` per UTF-16 unit on Windows.
    Synthetic,
}

#[derive(Parser, Debug)]
#[command(
    name = "s4_insert",
    about = "S4 — insert reliability: paste-and-restore vs synthetic (plan §8, §20)"
)]
struct Cli {
    /// Seconds before the insert fires, so the operator can focus the target.
    #[arg(long, default_value_t = 5)]
    delay: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List the payloads and why each exists.
    Payloads,

    /// The clipboard round-trip question, on its own and without typing anything.
    ///
    /// §20 asks it directly: *"Does clipboard save/restore round-trip?"* Run this
    /// with something interesting on the clipboard — a screenshot, a chunk of
    /// styled text from Word, a file from Finder — not with plain text.
    Roundtrip,

    /// Insert one payload with one method and check what landed.
    Insert {
        /// Payload id, or `all`.
        #[arg(long, default_value = "ascii")]
        payload: String,
        /// How to insert.
        #[arg(long, value_enum, default_value_t = Method::Paste)]
        method: Method,
        /// Milliseconds to wait after the insert before reading back. A 5 KB
        /// paste into Electron is not instant.
        #[arg(long, default_value_t = 600)]
        settle: u64,
        /// Restore the clipboard afterwards (paste method only).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        restore: bool,
        /// Append the JSON result to this file, one object per line.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

fn countdown(seconds: u64) {
    if seconds == 0 {
        return;
    }
    eprintln!(
        "Focus the target app and put the caret in an EMPTY field.\n\
         (Empty, because verification compares the whole field value against the payload.)"
    );
    for remaining in (1..=seconds).rev() {
        eprint!("\r  inserting in {remaining}s… ");
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!("\r  inserting now.           ");
}

/// Who is frontmost. Needed both to verify over AX and to label the report row.
#[cfg(target_os = "macos")]
fn frontmost() -> Result<(String, String, i32)> {
    use objc2_app_kit::NSWorkspace;
    let app = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .context("no frontmost application")?;
    Ok((
        app.localizedName()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        app.bundleIdentifier()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        app.processIdentifier(),
    ))
}

/// SPIKE: S4 — the Windows half. `GetForegroundWindow` +
/// `GetWindowThreadProcessId`, and UIA rather than AX for the read-back. §8 also
/// warns that UIPI blocks a non-elevated process from `SendInput`-ing to
/// elevated windows, so the Windows matrix needs an elevated target as a row.
#[cfg(not(target_os = "macos"))]
fn frontmost() -> Result<(String, String, i32)> {
    Ok(("unknown".to_owned(), "unknown".to_owned(), 0))
}

/// Synthesise the paste shortcut.
fn press_paste(enigo: &mut enigo::Enigo) -> Result<()> {
    use enigo::{Direction, Key, Keyboard as _};

    #[cfg(target_os = "macos")]
    let modifier = Key::Meta;
    #[cfg(not(target_os = "macos"))]
    let modifier = Key::Control;

    enigo
        .key(modifier, Direction::Press)
        .map_err(|e| anyhow::anyhow!("modifier press failed: {e}"))?;
    let result = enigo
        .key(Key::Unicode('v'), Direction::Click)
        .map_err(|e| anyhow::anyhow!("v failed: {e}"));
    // Release the modifier even if the keypress failed; a stuck ⌘ makes the
    // machine unusable and is a genuinely nasty way to end a spike run.
    enigo
        .key(modifier, Direction::Release)
        .map_err(|e| anyhow::anyhow!("modifier release failed: {e}"))?;
    result
}

/// Build an `Enigo`.
///
/// §8: *"its inter-event delay is only applied on `Drop`, so a long-lived
/// instance drops characters."* Every caller therefore builds a fresh one and
/// drops it before reading back — and if a payload only succeeds that way, that
/// is a finding about the crate, not about the app.
fn new_enigo() -> Result<enigo::Enigo> {
    enigo::Enigo::new(&enigo::Settings::default())
        .map_err(|e| anyhow::anyhow!("cannot create Enigo (Accessibility not granted?): {e}"))
}

#[derive(serde::Serialize)]
struct Row {
    app: String,
    bundle_id: String,
    payload: String,
    method: String,
    expected: payload::Counts,
    /// `None` when AX read-back was unavailable — NOT the same as a failure.
    actual: Option<payload::Counts>,
    verified: Option<bool>,
    diverged_at_char: Option<usize>,
    insert_ms: u128,
    clipboard_restored: Option<bool>,
    clipboard_restore_lossless: Option<bool>,
    change_count_before: Option<i64>,
    change_count_after: Option<i64>,
    notes: Vec<String>,
}

fn run_one(
    case: &payload::Payload,
    method: Method,
    settle: u64,
    restore_clipboard: bool,
) -> Result<Row> {
    let (app, bundle_id, pid) = frontmost()?;
    let expected = payload::counts(&case.text);
    let mut notes = Vec::new();

    println!("\n── {} via {:?} ──", case.id, method);
    println!("   why: {}", case.why);
    println!(
        "   payload: {} bytes / {} utf16 / {} chars   target: {app} ({bundle_id}) pid {pid}",
        expected.bytes, expected.utf16, expected.chars
    );

    let before = clipboard::snapshot().ok();
    let change_count_before = before.as_ref().and_then(|s| s.change_count);
    if let Some(snapshot) = &before
        && !snapshot.restore_can_be_lossless()
    {
        notes.push(format!(
            "clipboard held non-text flavours {:?} — a text-only restore is LOSSY",
            snapshot.flavours
        ));
    }

    let started = Instant::now();
    match method {
        Method::Paste => {
            clipboard::set(&case.text)?;
            let mut enigo = new_enigo()?;
            press_paste(&mut enigo)?;
            drop(enigo);
        }
        Method::Synthetic => {
            use enigo::Keyboard as _;
            let mut enigo = new_enigo()?;
            let result = enigo.text(&case.text);
            // Drop before evaluating: §8 says the inter-event delay is applied
            // on Drop, so the events are not necessarily out until then.
            drop(enigo);
            result.map_err(|e| anyhow::anyhow!("enigo.text failed: {e}"))?;
        }
    }
    let insert_ms = started.elapsed().as_millis();

    std::thread::sleep(Duration::from_millis(settle));

    let mut clipboard_restored = None;
    let mut clipboard_restore_lossless = None;
    if method == Method::Paste
        && restore_clipboard
        && let Some(snapshot) = &before
    {
        clipboard_restored = clipboard::restore(snapshot).ok();
        clipboard_restore_lossless = Some(snapshot.restore_can_be_lossless());
        let after_text = clipboard::get().ok().flatten();
        if after_text.as_deref() != snapshot.text.as_deref() {
            notes.push(
                "clipboard TEXT did not round-trip — the restore itself is broken".to_owned(),
            );
        }
        if snapshot.text.is_none() {
            notes.push(
                "clipboard was empty (or non-text) before; arboard cannot restore \
                     'empty', so the user's clipboard now holds the payload"
                    .to_owned(),
            );
        }
    }
    let change_count_after = clipboard::change_count();

    // Read back over AX where possible.
    #[cfg(target_os = "macos")]
    let actual_text = verify::focused_value(pid, 1.0);
    #[cfg(not(target_os = "macos"))]
    let actual_text: Option<String> = None;

    let mut verified = None;
    let mut diverged_at_char = None;
    let actual = actual_text.as_deref().map(payload::counts);

    match actual_text.as_deref() {
        None => {
            println!(
                "   read-back: UNAVAILABLE (AX could not read the field).\n\
                 →  Check the screen yourself. Look for: truncation, a missing \n\
                    leading newline, mangled emoji, and whether the app is still responsive."
            );
            notes.push("no AX read-back; result judged by the operator".to_owned());
        }
        Some(text) => {
            let contains = text.contains(&case.text);
            match payload::first_divergence(&case.text, text) {
                None => {
                    verified = Some(true);
                    println!("   read-back: EXACT MATCH ({} chars)", expected.chars);
                }
                Some((index, detail)) if contains => {
                    // The field already had content, or the app added something.
                    verified = Some(true);
                    diverged_at_char = Some(index);
                    println!(
                        "   read-back: payload PRESENT but the field is not only the payload \
                         (was it empty?) — first difference at char {index}\n      {detail}"
                    );
                }
                Some((index, detail)) => {
                    verified = Some(false);
                    diverged_at_char = Some(index);
                    println!("   read-back: MISMATCH at char {index}\n      {detail}");
                    if index == 0 && case.text.starts_with('\n') {
                        println!(
                            "   →  The insert lost its LEADING NEWLINE. This is §8's named enigo bug."
                        );
                    }
                }
            }
        }
    }

    println!(
        "   insert took {insert_ms} ms; clipboard changeCount {:?} → {:?}",
        change_count_before, change_count_after
    );
    for note in &notes {
        println!("   ! {note}");
    }

    Ok(Row {
        app,
        bundle_id,
        payload: case.id.to_owned(),
        method: format!("{method:?}").to_lowercase(),
        expected,
        actual,
        verified,
        diverged_at_char,
        insert_ms,
        clipboard_restored,
        clipboard_restore_lossless,
        change_count_before,
        change_count_after,
        notes,
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Payloads => {
            for case in payload::all() {
                let c = payload::counts(&case.text);
                println!(
                    "{:<26} {:>6} bytes {:>6} utf16 {:>6} chars\n    {}\n",
                    case.id, c.bytes, c.utf16, c.chars, case.why
                );
            }
            Ok(())
        }

        Command::Roundtrip => {
            println!(
                "Put something NON-TEXT on the clipboard first — a screenshot, styled text\n\
                 from Word, a file copied in Finder. Plain text always round-trips and\n\
                 proves nothing.\n"
            );
            let before = clipboard::snapshot()?;
            println!("  before: changeCount {:?}", before.change_count);
            println!("  before: flavours {:?}", before.flavours);
            println!(
                "  before: text {:?}",
                before
                    .text
                    .as_deref()
                    .map(|t| t.chars().take(60).collect::<String>())
            );
            println!(
                "  a text-only restore would be lossless: {}",
                before.restore_can_be_lossless()
            );

            clipboard::set("s4 round-trip probe")?;
            std::thread::sleep(Duration::from_millis(200));
            let restored = clipboard::restore(&before)?;
            std::thread::sleep(Duration::from_millis(200));

            let after = clipboard::snapshot()?;
            println!("\n  restored: {restored}");
            println!("  after: changeCount {:?}", after.change_count);
            println!("  after: flavours {:?}", after.flavours);
            println!("  text round-tripped: {}", before.text == after.text);

            let lost: Vec<&String> = before
                .flavours
                .iter()
                .filter(|f| !after.flavours.contains(f))
                .collect();
            if lost.is_empty() && !before.flavours.is_empty() {
                println!("  flavours lost: none");
            } else if !lost.is_empty() {
                println!("  flavours LOST: {lost:?}");
                println!(
                    "\n  §20's fallback applies: \"paste-only, always ask before clobbering the\n\
                     clipboard\". Save/restore is not lossless and the product must not pretend\n\
                     otherwise."
                );
            }
            println!(
                "\n  Also check by hand: does your clipboard MANAGER now show the probe string?\n\
                 §20: the org.nspasteboard.* exclusion markers are a community convention, not\n\
                 an Apple API, and must be written in the SAME declareTypes:owner: transaction\n\
                 or changeCount bumps twice and managers capture the first item.\n\
                 SPIKE: S4 — arboard does not expose declareTypes, so testing the markers needs\n\
                 raw NSPasteboard. Record whether your manager captured the probe."
            );
            Ok(())
        }

        Command::Insert {
            payload: which,
            method,
            settle,
            restore,
            out,
        } => {
            let cases = if which == "all" {
                payload::all()
            } else {
                vec![
                    payload::find(&which)
                        .with_context(|| format!("unknown payload {which:?}; try `payloads`"))?,
                ]
            };

            countdown(cli.delay);

            let mut rows = Vec::new();
            for (index, case) in cases.iter().enumerate() {
                if index > 0 {
                    println!("\n   -- clear the field, then press Return here to continue --");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line).ok();
                }
                match run_one(case, method, settle, restore) {
                    Ok(row) => rows.push(row),
                    Err(error) => println!("   FAILED TO RUN: {error:#}"),
                }
            }

            if let Some(path) = out {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("cannot open {}", path.display()))?;
                for row in &rows {
                    writeln!(file, "{}", serde_json::to_string(row)?)?;
                }
                eprintln!("\nappended {} rows to {}", rows.len(), path.display());
            }
            Ok(())
        }
    }
}
