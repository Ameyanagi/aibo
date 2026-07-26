//! S9 driver — `run` sweeps candidates live, `check` re-scores offline.
//!
//! The two are separate subcommands on purpose. §5's rule is "rerun on every
//! prompt edit and every model binding change", and the operator must be able
//! to change a property assertion and re-score yesterday's outputs without
//! spending money or waiting on a network.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use s9_eval::fixture::{self, Fixture, Recorded, Surface};
use s9_eval::live::{self, Endpoint};
use s9_eval::prompt::{self, PromptVersion};
use s9_eval::report;

#[derive(Parser, Debug)]
#[command(
    name = "s9_eval",
    about = "S9 — Complete quality + eval harness (plan §5, §20)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List the fixtures and what each one exercises.
    List {
        /// Fixture directory.
        #[arg(long, default_value = "fixtures")]
        fixtures: PathBuf,
    },

    /// Print the assembled prompt for one fixture — read it before trusting a
    /// pass rate. A bug in the prompt looks exactly like a bad model.
    Show {
        /// Fixture directory.
        #[arg(long, default_value = "fixtures")]
        fixtures: PathBuf,
        /// Fixture id.
        #[arg(long)]
        id: String,
        /// Prompt version id, e.g. `complete/v2-terse`.
        #[arg(long)]
        prompt_version: Option<String>,
    },

    /// Sweep one candidate over the fixtures and append JSONL to `--out`.
    ///
    /// One candidate per invocation, deliberately: candidates are compared by
    /// concatenating their JSONL files, so a run that dies halfway costs one
    /// candidate rather than the sweep.
    Run {
        /// Fixture directory.
        #[arg(long, default_value = "fixtures")]
        fixtures: PathBuf,
        /// Only run fixtures for this surface.
        #[arg(long, value_parser = parse_surface)]
        surface: Option<Surface>,
        /// Base URL, e.g. `https://api.openai.com/v1`, `http://localhost:11434/v1`.
        #[arg(long)]
        base_url: String,
        /// Model id as the endpoint names it.
        #[arg(long)]
        model: String,
        /// Environment variable holding the bearer token. Never the token itself:
        /// a key on the command line lands in the shell history.
        #[arg(long)]
        api_key_env: Option<String>,
        /// Prompt version. Defaults to the first version for each fixture's surface.
        #[arg(long)]
        prompt_version: Option<String>,
        /// Per-request timeout in seconds.
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
        /// Where to append recorded outputs.
        #[arg(long, default_value = "s9-outputs.jsonl")]
        out: PathBuf,
    },

    /// Score recorded outputs and write the markdown report.
    Check {
        /// Fixture directory.
        #[arg(long, default_value = "fixtures")]
        fixtures: PathBuf,
        /// One or more JSONL files of recorded outputs.
        #[arg(long = "recorded", required = true)]
        recorded: Vec<PathBuf>,
        /// Where to write the report.
        #[arg(long, default_value = "s9-report.md")]
        out: PathBuf,
        /// Cap the failure listing.
        #[arg(long, default_value_t = 40)]
        max_failures: usize,
    },
}

fn parse_surface(raw: &str) -> Result<Surface, String> {
    match raw {
        "complete" => Ok(Surface::Complete),
        "transform" => Ok(Surface::Transform),
        "ask" => Ok(Surface::Ask),
        other => Err(format!("unknown surface {other:?}")),
    }
}

fn resolve_version(fixture: &Fixture, requested: Option<&str>) -> Result<PromptVersion> {
    match requested {
        None => Ok(prompt::default_version(fixture.surface)),
        Some(id) => {
            let version =
                prompt::version(id).with_context(|| format!("unknown prompt version {id:?}"))?;
            anyhow::ensure!(
                version.surface == fixture.surface,
                "prompt version {id:?} serves {} but fixture {} is {}",
                version.surface.as_str(),
                fixture.id,
                fixture.surface.as_str(),
            );
            Ok(version)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List { fixtures } => {
            let fixtures = fixture::load_dir(&fixtures)?;
            println!("| id | surface | lang | app | notes |");
            println!("|---|---|---|---|---|");
            for f in &fixtures {
                println!(
                    "| `{}` | {} | {:?} | {} | {} |",
                    f.id,
                    f.surface.as_str(),
                    f.lang,
                    f.app.as_deref().unwrap_or("-"),
                    f.notes.as_deref().unwrap_or("")
                );
            }
            println!("\n{} fixtures", fixtures.len());
            if fixtures.len() < 50 {
                println!(
                    "\nNOTE: §5 asks for ~50 real cases per surface. {} is a seed set, \
                     not a corpus — a pass rate computed on it is not evidence yet.",
                    fixtures.len()
                );
            }
            Ok(())
        }

        Command::Show {
            fixtures,
            id,
            prompt_version,
        } => {
            let fixtures = fixture::load_dir(&fixtures)?;
            let f = fixtures
                .iter()
                .find(|f| f.id == id)
                .with_context(|| format!("no fixture with id {id:?}"))?;
            let version = resolve_version(f, prompt_version.as_deref())?;
            let assembled = prompt::assemble(f, version);
            println!("# prompt version: {}\n", version.id);
            println!(
                "temperature={} max_tokens={} stop={:?}\n",
                assembled.temperature, assembled.max_tokens, assembled.stop
            );
            println!("--- system ---\n{}\n", assembled.system);
            println!("--- user ---\n{}", assembled.user);
            Ok(())
        }

        Command::Run {
            fixtures,
            surface,
            base_url,
            model,
            api_key_env,
            prompt_version,
            timeout_secs,
            out,
        } => {
            let all = fixture::load_dir(&fixtures)?;
            let selected: Vec<&Fixture> = all
                .iter()
                .filter(|f| surface.is_none_or(|s| f.surface == s))
                .collect();
            anyhow::ensure!(!selected.is_empty(), "no fixtures matched");

            let api_key = match &api_key_env {
                None => None,
                Some(name) => Some(
                    std::env::var(name)
                        .with_context(|| format!("environment variable {name} is not set"))?,
                ),
            };
            let endpoint = Endpoint {
                base_url,
                model: model.clone(),
                api_key,
                timeout: Duration::from_secs(timeout_secs),
            };
            let http = live::client()?;

            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&out)
                .with_context(|| format!("cannot open {}", out.display()))?;

            for (index, f) in selected.iter().enumerate() {
                let version = resolve_version(f, prompt_version.as_deref())?;
                let candidate = format!("{model} @ {}", version.id);
                let assembled = prompt::assemble(f, version);

                eprintln!(
                    "[{}/{}] {} ({})",
                    index + 1,
                    selected.len(),
                    f.id,
                    version.id
                );

                let row = match live::complete(&http, &endpoint, &assembled).await {
                    Ok(completion) => Recorded {
                        fixture_id: f.id.clone(),
                        candidate,
                        output: completion.text,
                        ttft_ms: completion.ttft_ms,
                        total_ms: Some(completion.total_ms),
                        error: None,
                    },
                    Err(error) => {
                        // A failing candidate must still produce a row: an
                        // error rate is a result, and silently skipping turns a
                        // broken endpoint into a suspiciously small sample.
                        eprintln!("    error: {error:#}");
                        Recorded {
                            fixture_id: f.id.clone(),
                            candidate,
                            output: String::new(),
                            ttft_ms: None,
                            total_ms: None,
                            error: Some(format!("{error:#}")),
                        }
                    }
                };
                use std::io::Write as _;
                writeln!(file, "{}", serde_json::to_string(&row)?)?;
                file.flush()?;
            }

            eprintln!("\nappended {} rows to {}", selected.len(), out.display());
            eprintln!("now: cargo run -- check --recorded {}", out.display());
            Ok(())
        }

        Command::Check {
            fixtures,
            recorded,
            out,
            max_failures,
        } => {
            let fixtures = fixture::load_dir(&fixtures)?;
            let mut rows = Vec::new();
            for path in &recorded {
                rows.extend(fixture::load_recorded(path)?);
            }
            anyhow::ensure!(!rows.is_empty(), "no recorded outputs to score");

            let (scored, orphans) = report::score(&fixtures, &rows);
            if !orphans.is_empty() {
                eprintln!(
                    "WARNING: {} recorded rows reference unknown fixture ids \
                     (they are NOT in the denominator): {:?}",
                    orphans.len(),
                    orphans
                );
            }
            let summaries = report::summarise(&scored);
            let markdown = report::markdown(&summaries, &scored, max_failures);
            std::fs::write(&out, &markdown)
                .with_context(|| format!("cannot write {}", out.display()))?;
            println!("{markdown}");
            eprintln!("written to {}", out.display());
            Ok(())
        }
    }
}
