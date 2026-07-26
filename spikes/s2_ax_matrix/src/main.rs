//! S2 — AX read across real apps (§20). macOS half.
//!
//! > Does `text_field_context()` work in Safari, Chrome, VS Code, Slack, Word,
//! > Notion, Terminal? Build the honest matrix **and** `testapps/`.
//! > If it fails: Complete degrades to clipboard-only in unsupported apps;
//! > narrow the marketing claim. — §20
//!
//! This binary does not decide anything. It walks the AX tree of whatever app
//! the operator puts in front of it and writes down exactly what was readable,
//! what failed, with which `AXError`, and how long each read took. The matrix in
//! `docs/app-matrix.md` (§18 tier 4) is assembled from its output by hand.
//!
//! Timings are first-class: §8 gives AX/UIA capture a hard **120 ms** deadline,
//! so "supported but slow" is a distinct and equally important finding from
//! "unsupported".

#[cfg(target_os = "macos")]
mod ax;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!(
        "S2's macOS half only builds on macOS.\n\
         The Windows half of §20's S2 row — UIA `GetSelection`, and Chromium \n\
         declaring no `ITextProvider2` so Chrome/Edge/Electron/Slack/VS Code have\n\
         no `GetCaretRange` — is a separate harness. See README.md."
    );
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    macos::run()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use clap::{Parser, Subcommand};
    use serde::Serialize;
    use serde_json::json;

    use crate::ax::{AxElement, describe, process_is_trusted};

    #[derive(Parser, Debug)]
    #[command(
        name = "s2_ax_matrix",
        about = "S2 — walk the AX tree of the focused app and record what is readable (plan §8, §20)"
    )]
    struct Cli {
        /// Seconds to wait before capturing, so the operator can click into the
        /// target app. THIS process must not be frontmost when the read happens.
        #[arg(long, default_value_t = 5)]
        delay: u64,

        /// Per-call AX messaging timeout, seconds. §8: an unbounded AX call
        /// against a busy app blocks for *seconds*. Keep this small; a read that
        /// only succeeds with a large value is a finding, not a success.
        #[arg(long, default_value_t = 1.0)]
        ax_timeout: f32,

        #[command(subcommand)]
        command: Command,
    }

    #[derive(Subcommand, Debug)]
    enum Command {
        /// Everything the product would try to read on hotkey-down, for the
        /// currently focused field. **This is the command that fills the matrix.**
        Probe {
            /// Also print every attribute the focused element advertises,
            /// including ones aibo does not use.
            #[arg(long)]
            all_attributes: bool,
            /// Append the JSON result to this file, one object per line.
            #[arg(long)]
            out: Option<std::path::PathBuf>,
        },

        /// Walk the AX tree from the application element down, printing roles.
        ///
        /// Use this when `probe` says "unsupported" and you want to know whether
        /// the text is somewhere else in the tree, or genuinely absent.
        Walk {
            /// Maximum depth.
            #[arg(long, default_value_t = 6)]
            depth: usize,
            /// Stop after this many nodes. Chrome's web area is tens of
            /// thousands of nodes; an unbounded walk is not a test, it is a hang.
            #[arg(long, default_value_t = 400)]
            budget: usize,
            /// Print `AXValue` for text-bearing roles too.
            #[arg(long)]
            values: bool,
        },

        /// Set an AX-tree-enabling flag on the focused app, then re-probe.
        ///
        /// §8: Chrome/Chromium honours `AXEnhancedUserInterface`; **Electron
        /// honours `AXManualAccessibility`**; the wrong one returns
        /// `kAXErrorAttributeUnsupported`. Chrome's activation is also
        /// *asynchronous*, so `--settle` exists and a zero settle time will lie
        /// to you.
        Enable {
            /// Which flag.
            #[arg(long, value_enum)]
            flag: Flag,
            /// Milliseconds to wait after setting before re-reading.
            #[arg(long, default_value_t = 500)]
            settle: u64,
        },

        /// Print who is frontmost. A sanity check for the `--delay` dance.
        Who,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
    enum Flag {
        /// Chrome / Chromium.
        Enhanced,
        /// Electron (Slack, VS Code, Notion, Discord…).
        Manual,
    }

    impl Flag {
        fn attribute(self) -> &'static str {
            match self {
                // Not in accessibility-sys' constant list: both are private
                // conventions, not public AX attributes.
                Flag::Enhanced => "AXEnhancedUserInterface",
                Flag::Manual => "AXManualAccessibility",
            }
        }
    }

    /// Who was frontmost when the probe fired.
    #[derive(Debug, Serialize)]
    struct Target {
        /// Localised app name, e.g. `Google Chrome`.
        name: String,
        /// Bundle identifier — the key §8 says the AX-enabling flag is chosen by.
        bundle_id: String,
        /// Process id.
        pid: i32,
    }

    fn frontmost() -> Result<Target> {
        use objc2_app_kit::NSWorkspace;

        let workspace = NSWorkspace::sharedWorkspace();
        let app = workspace
            .frontmostApplication()
            .context("no frontmost application — is the login session unlocked?")?;
        Ok(Target {
            name: app
                .localizedName()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".to_owned()),
            bundle_id: app
                .bundleIdentifier()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".to_owned()),
            pid: app.processIdentifier(),
        })
    }

    fn countdown(seconds: u64) {
        if seconds == 0 {
            return;
        }
        eprintln!("Click into the app and the text field you want to test.");
        for remaining in (1..=seconds).rev() {
            eprint!("\r  capturing in {remaining}s… ");
            std::thread::sleep(Duration::from_secs(1));
        }
        eprintln!("\r  capturing now.            ");
    }

    /// The attributes aibo's `text_field_context()` would actually read (§7, §8).
    const PROBED: &[(&str, &str)] = &[
        (accessibility_sys::kAXRoleAttribute, "role"),
        (accessibility_sys::kAXSubroleAttribute, "subrole"),
        (accessibility_sys::kAXTitleAttribute, "title"),
        (
            accessibility_sys::kAXDescriptionAttribute,
            "description (field label)",
        ),
        (
            accessibility_sys::kAXPlaceholderValueAttribute,
            "placeholder",
        ),
        (
            accessibility_sys::kAXValueAttribute,
            "VALUE — the whole field text",
        ),
        (
            accessibility_sys::kAXNumberOfCharactersAttribute,
            "length (UTF-16)",
        ),
        (accessibility_sys::kAXSelectedTextAttribute, "SELECTED TEXT"),
        (
            accessibility_sys::kAXSelectedTextRangeAttribute,
            "CARET/SELECTION RANGE (AXValue<CFRange>)",
        ),
        (
            accessibility_sys::kAXVisibleCharacterRangeAttribute,
            "visible range",
        ),
        (accessibility_sys::kAXFocusedAttribute, "focused"),
        (accessibility_sys::kAXEnabledAttribute, "enabled"),
        (accessibility_sys::kAXPositionAttribute, "position (anchor)"),
        (accessibility_sys::kAXSizeAttribute, "size (anchor)"),
    ];

    fn probe_focused(
        target: &Target,
        ax_timeout: f32,
        all_attributes: bool,
    ) -> Result<serde_json::Value> {
        let app = AxElement::application(target.pid);
        app.set_messaging_timeout(ax_timeout)
            .map_err(|e| anyhow::anyhow!("AXUIElementSetMessagingTimeout: {e}"))?;

        let started = Instant::now();
        let focused = app.element_attribute(accessibility_sys::kAXFocusedUIElementAttribute);
        let focus_elapsed = focused.elapsed;

        let focused = match focused.result {
            Ok(element) => element,
            Err(failure) => {
                println!(
                    "  AXFocusedUIElement          FAIL  {failure}  ({:?})",
                    focus_elapsed
                );
                println!(
                    "\n  No focused element at all. Either the app publishes no AX tree\n\
                     (try `enable`), or the focus is on a non-AX surface, or\n\
                     Accessibility is not granted to THIS binary."
                );
                return Ok(json!({
                    "target": target,
                    "focused_element": null,
                    "focus_error": failure.to_string(),
                    "focus_ms": focus_elapsed.as_millis(),
                }));
            }
        };
        println!(
            "  AXFocusedUIElement          ok    ({} ms)",
            focus_elapsed.as_millis()
        );

        let mut attributes = serde_json::Map::new();
        println!("\n  {:<32} {:>7}  result", "attribute", "ms");
        println!("  {:-<32} {:->7}  {:-<40}", "", "", "");

        for (name, note) in PROBED {
            let timed = focused.attribute(name);
            let ms = timed.elapsed.as_millis();
            let slow = if timed.within_deadline() { " " } else { "!" };
            match &timed.result {
                Ok(value) => {
                    let described = describe(value, 120);
                    println!("{slow} {name:<32} {ms:>7}  {}", summarise(&described));
                    attributes.insert(
                        (*name).to_owned(),
                        json!({ "ms": ms, "note": note, "value": described }),
                    );
                }
                Err(failure) => {
                    println!("{slow} {name:<32} {ms:>7}  — {failure}");
                    attributes.insert(
                        (*name).to_owned(),
                        json!({ "ms": ms, "note": note, "error": failure.to_string() }),
                    );
                }
            }
        }

        let advertised = focused.attribute_names();
        let advertised_names = advertised.result.unwrap_or_default();
        if all_attributes {
            println!(
                "\n  every advertised attribute ({}):",
                advertised_names.len()
            );
            for name in &advertised_names {
                println!("    {name}");
            }
        }

        let total = started.elapsed();
        println!(
            "\n  total capture {} ms — §8 budget is 120 ms for AX, 250 ms with the clipboard fallback: {}",
            total.as_millis(),
            if total <= Duration::from_millis(120) {
                "WITHIN"
            } else {
                "OVER (record this)"
            }
        );

        Ok(json!({
            "target": target,
            "focus_ms": focus_elapsed.as_millis(),
            "total_ms": total.as_millis(),
            "attributes": attributes,
            "advertised": advertised_names,
        }))
    }

    fn summarise(described: &serde_json::Value) -> String {
        match described.get("kind").and_then(|k| k.as_str()) {
            Some("string") => {
                let chars = described.get("chars").and_then(|c| c.as_u64()).unwrap_or(0);
                let text = described
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .replace('\n', "\\n");
                format!("string[{chars}] {text:?}")
            }
            Some("range_utf16") => format!(
                "range loc={} len={} (UTF-16 units)",
                described
                    .get("location")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1),
                described
                    .get("length")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1),
            ),
            Some("number") => format!(
                "number {}",
                described
                    .get("value")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(f64::NAN)
            ),
            Some("bool") => format!(
                "bool {}",
                described
                    .get("value")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            ),
            Some("array") => format!(
                "array[{}]",
                described
                    .get("count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1)
            ),
            Some("element") => "AXUIElement".to_owned(),
            _ => "(opaque AXValue — point/size/rect)".to_owned(),
        }
    }

    /// Iterative, budgeted tree walk. Never recurses — see `ax.rs`.
    fn walk(target: &Target, ax_timeout: f32, depth: usize, budget: usize, values: bool) {
        let app = AxElement::application(target.pid);
        let _ = app.set_messaging_timeout(ax_timeout);

        let mut stack: Vec<(AxElement, usize)> = vec![(app, 0)];
        let mut visited = 0usize;
        let started = Instant::now();

        while let Some((element, level)) = stack.pop() {
            if visited >= budget {
                println!(
                    "\n  stopped at the {budget}-node budget. Raise --budget only if you \n\
                     have a reason; a tree this large is itself a finding (§8: Chrome's \n\
                     web area is enormous and enabling it is asynchronous)."
                );
                break;
            }
            visited += 1;

            let role = element.role();
            let indent = "  ".repeat(level);
            let mut line = format!("  {indent}{role}");

            if let Ok(value) = element
                .attribute(accessibility_sys::kAXTitleAttribute)
                .result
                && let Some(text) = value.downcast::<core_foundation::string::CFString>()
            {
                let text = text.to_string();
                if !text.is_empty() {
                    line.push_str(&format!(" title={:?}", truncate(&text, 40)));
                }
            }
            if values
                && let Ok(value) = element
                    .attribute(accessibility_sys::kAXValueAttribute)
                    .result
                && let Some(text) = value.downcast::<core_foundation::string::CFString>()
            {
                let text = text.to_string();
                line.push_str(&format!(
                    " value[{}]={:?}",
                    text.chars().count(),
                    truncate(&text, 60)
                ));
            }
            println!("{line}");

            if level + 1 > depth {
                continue;
            }
            if let Ok(children) = element.children().result {
                // Reversed so the printed order matches the tree order.
                for child in children.into_iter().rev() {
                    stack.push((child, level + 1));
                }
            }
        }
        println!(
            "\n  {visited} nodes in {} ms",
            started.elapsed().as_millis()
        );
    }

    fn truncate(text: &str, limit: usize) -> String {
        let cleaned = text.replace('\n', "\\n");
        if cleaned.chars().count() <= limit {
            cleaned
        } else {
            cleaned.chars().take(limit).collect::<String>() + "…"
        }
    }

    pub fn run() -> Result<()> {
        let cli = Cli::parse();

        if !process_is_trusted() {
            eprintln!(
                "AXIsProcessTrusted() == false.\n\n\
                 Grant Accessibility to the binary that is actually running — for\n\
                 `cargo run` that is `target/debug/s2_ax_matrix`, NOT Terminal and NOT\n\
                 cargo. §17: the TCC grant is keyed to the code signature, so an\n\
                 unsigned debug build invalidates its own grant on every rebuild.\n\
                 Drag the binary into System Settings ▸ Privacy & Security ▸\n\
                 Accessibility, or run it from a terminal you have already granted.\n\n\
                 Continuing anyway — every read below will fail with kAXErrorAPIDisabled,\n\
                 which is itself worth seeing once."
            );
        }

        match cli.command {
            Command::Who => {
                countdown(cli.delay);
                let target = frontmost()?;
                println!("{}", serde_json::to_string_pretty(&target)?);
                Ok(())
            }

            Command::Probe {
                all_attributes,
                out,
            } => {
                countdown(cli.delay);
                let target = frontmost()?;
                println!(
                    "\n== {} ({}) pid {} ==\n",
                    target.name, target.bundle_id, target.pid
                );
                let report = probe_focused(&target, cli.ax_timeout, all_attributes)?;
                if let Some(path) = out {
                    use std::io::Write as _;
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .with_context(|| format!("cannot open {}", path.display()))?;
                    writeln!(file, "{}", serde_json::to_string(&report)?)?;
                    eprintln!("appended to {}", path.display());
                }
                Ok(())
            }

            Command::Walk {
                depth,
                budget,
                values,
            } => {
                countdown(cli.delay);
                let target = frontmost()?;
                println!(
                    "\n== {} ({}) pid {} ==\n",
                    target.name, target.bundle_id, target.pid
                );
                walk(&target, cli.ax_timeout, depth, budget, values);
                Ok(())
            }

            Command::Enable { flag, settle } => {
                countdown(cli.delay);
                let target = frontmost()?;
                println!(
                    "\n== {} ({}) pid {} ==\n",
                    target.name, target.bundle_id, target.pid
                );
                let app = AxElement::application(target.pid);
                app.set_messaging_timeout(cli.ax_timeout).ok();

                let attribute = flag.attribute();
                match app.set_bool_attribute(attribute, true) {
                    Ok(()) => println!("  set {attribute} = true"),
                    Err(failure) => {
                        println!("  set {attribute} FAILED: {failure}");
                        println!(
                            "  §8: kAXErrorAttributeUnsupported here means you picked the wrong\n\
                             flag for this app. Chrome/Chromium wants AXEnhancedUserInterface;\n\
                             Electron (Slack, VS Code, Notion) wants AXManualAccessibility."
                        );
                    }
                }

                println!(
                    "  waiting {settle} ms for the tree to appear (Chrome's activation is asynchronous)…"
                );
                std::thread::sleep(Duration::from_millis(settle));
                println!();
                let _ = probe_focused(&target, cli.ax_timeout, false)?;

                println!(
                    "\n  Now decide the user-hostility question §8 raises: AXEnhancedUserInterface\n\
                     also breaks window positioning and makes resizing sluggish. Did the app\n\
                     visibly degrade? Record it — 'we silently set a flag that makes your\n\
                     browser worse' is a product decision, not an implementation detail."
                );
                Ok(())
            }
        }
    }
}
