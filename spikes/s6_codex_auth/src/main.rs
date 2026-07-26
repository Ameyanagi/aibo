//! S6 — Codex endpoint via device-code auth.
//!
//! > **S6** · Codex endpoint via device-code auth · Run the device flow
//! > (`/api/accounts/deviceauth/usercode` -> `/deviceauth/token`), then make one
//! > Responses call to `CHATGPT_CODEX_BASE_URL` with `Authorization` +
//! > `ChatGPT-Account-ID`. **Does it succeed without `x-oai-attestation`?**
//! > — §20
//!
//! Run `login` once, then `probe` as many times as you like. See `README.md`
//! for the operator script and what to record.

mod device;
mod jwt;
mod probe;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

/// Codex's own OAuth client id, published in the `openai/codex` tree.
///
/// SPIKE: S6 — [unverified]. §3a is explicit that this is Codex's client id and
/// that the consent screen the user sees says *Codex*, not aibo. Re-read it out
/// of the current `openai/codex` source before running, and override with
/// `--client-id` rather than editing this constant.
const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The device-flow issuer named in §3a.
const DEFAULT_ISSUER: &str = "https://auth.openai.com";

/// The page §3a says the human is sent to.
const CONSENT_PAGE: &str = "https://auth.openai.com/codex/device";

#[derive(Parser)]
#[command(
    name = "s6_codex_auth",
    about = "S6: does the Codex Responses endpoint accept device-code tokens without x-oai-attestation?"
)]
struct Cli {
    /// Where the token pair is written and read. Contains live credentials.
    #[arg(long, global = true, default_value = "s6-tokens.json")]
    token_file: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the device-code flow and store the resulting tokens.
    Login {
        /// OAuth client id. See `DEFAULT_CLIENT_ID`.
        #[arg(long, default_value = DEFAULT_CLIENT_ID)]
        client_id: String,
        /// Device-auth issuer origin.
        #[arg(long, default_value = DEFAULT_ISSUER)]
        issuer: String,
        /// Requested scope.
        #[arg(long, default_value = "openid profile email offline_access")]
        scope: String,
        /// Body encoding. Try `form` first (RFC 8628), then `json`.
        #[arg(long, value_enum, default_value_t = device::BodyEncoding::Form)]
        encoding: device::BodyEncoding,
    },
    /// Show what is in the stored ID token without making any request.
    Inspect,
    /// Run the header matrix against the Codex Responses endpoint.
    Probe {
        /// Base URL. Defaults to `CHATGPT_CODEX_BASE_URL`.
        #[arg(long, default_value = probe::CHATGPT_CODEX_BASE_URL)]
        base_url: String,
        /// Model id to request.
        ///
        /// SPIKE: S6 — [unverified]. If every variant 400s, try another id
        /// before concluding anything about auth.
        #[arg(long, default_value = "gpt-5.1-codex")]
        model: String,
        /// Ask for an SSE stream, which is the only way to get a TTFT number.
        #[arg(long, default_value_t = true)]
        stream: bool,
        /// Per-variant timeout in seconds.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        /// Bytes of body to keep per variant.
        #[arg(long, default_value_t = 4096)]
        body_head_bytes: usize,
        /// Only run one named variant.
        #[arg(long)]
        only: Option<String>,
        /// Where the machine-readable report is written.
        #[arg(long, default_value = "s6-report.json")]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Login {
            client_id,
            issuer,
            scope,
            encoding,
        } => login(&cli.token_file, &client_id, &issuer, &scope, encoding).await,
        Command::Inspect => {
            let tokens = load_tokens(&cli.token_file).await?;
            print_identity(&tokens);
            Ok(())
        }
        Command::Probe {
            base_url,
            model,
            stream,
            timeout_secs,
            body_head_bytes,
            only,
            out,
        } => {
            let tokens = load_tokens(&cli.token_file).await?;
            run_probe(
                tokens,
                probe::ProbeConfig {
                    base_url,
                    access_token: String::new(), // filled in below
                    account_id: None,
                    model,
                    stream,
                    timeout: Duration::from_secs(timeout_secs),
                    body_head_bytes,
                },
                only,
                out,
            )
            .await
        }
    }
}

async fn login(
    token_file: &PathBuf,
    client_id: &str,
    issuer: &str,
    scope: &str,
    encoding: device::BodyEncoding,
) -> Result<()> {
    let auth = device::DeviceAuth::new(probe::client()?, issuer, client_id, encoding);

    println!("Requesting a user code from {issuer} ({encoding:?} body)...");
    let code = auth.request_user_code(scope).await?;

    println!();
    println!("=========================================================");
    println!("  1. Open: {}", code.verification_uri.as_deref().unwrap_or(CONSENT_PAGE));
    println!("  2. Enter code: {}", code.user_code);
    println!();
    println!("  NOTE (§3a): the consent screen says *Codex*, not aibo.");
    println!("  Record whether that wording is acceptable to ship.");
    println!("=========================================================");
    println!();
    println!(
        "usercode response fields (record these — §3a does not specify them): {:?}",
        code.raw
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    );
    println!("Polling every {}s (expires in {}s)...", code.interval_secs, code.expires_in_secs);

    let tokens = auth.poll_for_tokens(&code).await?;
    let json = serde_json::to_string_pretty(&tokens)?;
    tokio::fs::write(token_file, json)
        .await
        .with_context(|| format!("failed to write {}", token_file.display()))?;

    println!("Tokens written to {}", token_file.display());
    println!("!! That file holds live credentials. Delete it when the spike is done.");
    print_identity(&tokens);
    Ok(())
}

async fn load_tokens(path: &PathBuf) -> Result<device::Tokens> {
    let raw = tokio::fs::read_to_string(path).await.with_context(|| {
        format!(
            "no token file at {} — run `login` first",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).context("token file is not the JSON this spike wrote")
}

fn print_identity(tokens: &device::Tokens) {
    println!();
    println!("--- identity ---");
    match &tokens.id_token {
        None => println!("no id_token in the response — ChatGPT-Account-ID cannot be derived"),
        Some(id_token) => match jwt::decode_payload(id_token) {
            Err(e) => println!("id_token payload unreadable: {e:#}"),
            Ok(payload) => {
                println!(
                    "chatgpt_account_id: {}",
                    jwt::chatgpt_account_id(&payload).unwrap_or_else(|| "<absent>".into())
                );
                println!(
                    "chatgpt_plan_type:  {}",
                    jwt::plan_type(&payload).unwrap_or_else(|| "<absent>".into())
                );
                println!(
                    "claim keys: {:?}",
                    payload
                        .as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                );
            }
        },
    }
    println!(
        "refresh_token present: {}",
        tokens.refresh_token.is_some()
    );
    println!("expires_in: {:?}", tokens.expires_in);
}

async fn run_probe(
    tokens: device::Tokens,
    mut config: probe::ProbeConfig,
    only: Option<String>,
    out: PathBuf,
) -> Result<()> {
    config.access_token = tokens.access_token.clone();
    config.account_id = tokens
        .id_token
        .as_deref()
        .and_then(|t| jwt::decode_payload(t).ok())
        .and_then(|p| jwt::chatgpt_account_id(&p));

    if config.account_id.is_none() {
        eprintln!(
            "WARNING: no chatgpt_account_id claim — the `minimal` and `codex_like` variants \
             will run without ChatGPT-Account-ID, which §3a calls mandatory. Record that."
        );
    }

    let all = probe::variants();
    let selected: Vec<_> = match &only {
        None => all.clone(),
        Some(name) => all.iter().filter(|v| v.name == name).cloned().collect(),
    };
    if selected.is_empty() {
        bail!("no variant named {only:?}; try one of {:?}", all.iter().map(|v| v.name).collect::<Vec<_>>());
    }

    let http = probe::client()?;
    let mut results = Vec::new();
    for variant in &selected {
        println!("-> {} ({})", variant.name, variant.isolates);
        let result = probe::run_variant(&http, &config, variant).await;
        println!(
            "   status={:?} headers_ms={} ttft_ms={:?}",
            result.status, result.headers_ms, result.ttft_ms
        );
        if !result.body_head.is_empty() {
            println!("   body: {}", first_line(&result.body_head, 300));
        }
        results.push(result);
    }

    println!();
    println!("{}", probe::markdown_table(&results, &all));
    println!("{}", probe::outcome(&results));
    println!();

    let report = serde_json::json!({
        "spike": "S6",
        "base_url": config.base_url,
        "model": config.model,
        "stream": config.stream,
        "account_id_present": config.account_id.is_some(),
        "results": results,
    });
    tokio::fs::write(&out, serde_json::to_string_pretty(&report)?)
        .await
        .with_context(|| format!("failed to write {}", out.display()))?;
    println!("Full report: {}", out.display());
    Ok(())
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.chars().take(max).collect()
}
