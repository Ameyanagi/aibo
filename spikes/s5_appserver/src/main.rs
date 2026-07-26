//! # S5 — `codex app-server` handshake spike
//!
//! Throwaway binary for risk-register item **S5** (docs/plan.md §20).
//!
//! Question the plan asks: *"Spawn `codex` over stdio, `initialize`,
//! `account/read`, run one thread. Does published protocol 0.63.0 deserialise
//! today's binary? Minimum version floor?"*
//!
//! What this binary does, in order, printing a machine-readable line per step:
//!
//! 1. Locates the `codex` binary and records `codex --version`.
//! 2. Spawns `codex app-server --stdio` with piped stdio.
//! 3. Sends `initialize` and prints the whole result verbatim, then explicitly
//!    reports whether the response carries **any** protocol-version field.
//! 4. Sends the `initialized` notification.
//! 5. Calls `account/read` and `account/rateLimits/read`.
//! 6. Runs a set of *wire-shape probes* that decide how permissive aibo's own
//!    codec has to be (see [`probes`]).
//! 7. Optionally (`--thread`) starts one ephemeral, read-only, never-approve
//!    thread and runs a single trivial turn to end-to-end-verify the
//!    thread/turn half of the protocol. **This spends real quota**, so it is
//!    off by default.
//!
//! Run with `--json` to get one JSON object per line instead of prose.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

/// How long any single request may take before the spike gives up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a single agent turn may take before the spike gives up.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let run_thread = args.iter().any(|a| a == "--thread");
    let json_out = args.iter().any(|a| a == "--json");

    let mut out = Report::new(json_out);

    // ---- 0. which codex? ---------------------------------------------------
    let codex = std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
    let version = capture_version(&codex)?;
    out.kv("codex.bin", &codex);
    out.kv("codex.version", version.trim());

    // ---- 1. spawn ----------------------------------------------------------
    let started = Instant::now();
    let mut client = Client::spawn(&codex).context("spawning `codex app-server --stdio`")?;
    out.kv("spawn.ok", "true");

    // ---- 2. initialize -----------------------------------------------------
    let init = client.request(
        "initialize",
        json!({
            "clientInfo": {
                "name": "aibo-spike-s5",
                "title": "aibo S5 handshake spike",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }),
    );
    match &init {
        Ok(v) => {
            out.kv("initialize.ok", "true");
            out.kv("initialize.latency_ms", &started.elapsed().as_millis().to_string());
            out.kv("initialize.result", &v.to_string());
            // The plan's phrasing assumes there is a protocol version to floor
            // against. Check, do not assume.
            let version_field = ["protocolVersion", "protocol_version", "version", "schemaVersion"]
                .iter()
                .find(|k| v.get(**k).is_some())
                .map(|k| format!("{k}={}", v[*k]));
            out.kv(
                "initialize.protocol_version_field",
                version_field.as_deref().unwrap_or("ABSENT"),
            );
            out.kv(
                "initialize.userAgent",
                v.get("userAgent").and_then(Value::as_str).unwrap_or("<none>"),
            );
        }
        Err(e) => {
            out.kv("initialize.ok", "false");
            out.kv("initialize.error", &e.to_string());
            out.finish();
            bail!("initialize failed — everything downstream is moot");
        }
    }

    // ---- 3. initialized notification ---------------------------------------
    client.notify("initialized", json!({}))?;
    out.kv("initialized.sent", "true");

    // ---- 4. account/read ---------------------------------------------------
    match client.request("account/read", json!({})) {
        Ok(v) => {
            out.kv("account_read.ok", "true");
            out.kv("account_read.result", &redact_account(&v).to_string());
        }
        Err(e) => {
            out.kv("account_read.ok", "false");
            out.kv("account_read.error", &e.to_string());
        }
    }

    // §3: rate limits are a *separate channel* from account/read. Confirm.
    match client.request("account/rateLimits/read", json!({})) {
        Ok(v) => {
            out.kv("rate_limits.ok", "true");
            out.kv(
                "rate_limits.has_ratelimits_key",
                &v.get("rateLimits").is_some().to_string(),
            );
            out.kv(
                "rate_limits.keys",
                &v.as_object()
                    .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                    .unwrap_or_default(),
            );
        }
        Err(e) => {
            out.kv("rate_limits.ok", "false");
            out.kv("rate_limits.error", &e.to_string());
        }
    }

    // ---- 5. wire-shape probes ---------------------------------------------
    probes::run(&mut client, &mut out);

    // ---- 6. optional: one real thread --------------------------------------
    if run_thread {
        match one_thread(&mut client, &mut out) {
            Ok(()) => out.kv("thread.ok", "true"),
            Err(e) => {
                out.kv("thread.ok", "false");
                out.kv("thread.error", &e.to_string());
            }
        }
    } else {
        out.kv("thread.skipped", "pass --thread to run one real turn (spends quota)");
    }

    let stderr = client.stderr_snapshot();
    if !stderr.is_empty() {
        out.kv("server.stderr", &stderr.join(" | "));
    }
    client.shutdown();
    out.finish();
    Ok(())
}

/// Blank out the account e-mail so the spike output can be pasted into an issue.
fn redact_account(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(email) = v.pointer_mut("/account/email")
        && email.is_string()
    {
        *email = json!("<redacted>");
    }
    v
}

fn capture_version(codex: &str) -> Result<String> {
    let out = Command::new(codex)
        .arg("--version")
        .output()
        .with_context(|| format!("running `{codex} --version` — is codex on PATH?"))?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

/// Wire-shape probes. Each one answers a concrete question that changes how
/// `aibo-agent`'s vendored codec must be written.
mod probes {
    use super::*;

    /// Run every probe, recording pass/fail into the report.
    pub fn run(client: &mut Client, out: &mut Report) {
        // P1. The plan (§3) says the protocol is "JSON-RPC-2-*like*" and omits
        //     `"jsonrpc":"2.0"`. Does the server *reject* the field if a strict
        //     codec (e.g. `jsonrpsee`) sends it anyway?
        let id = client.next_id();
        let r = client.request_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "account/read",
            "params": {}
        }));
        out.probe(
            "strict_jsonrpc_field_accepted",
            r.is_ok(),
            "sending `\"jsonrpc\":\"2.0\"` alongside id/method/params",
        );

        // P2. Does the server ignore unknown fields in params? If yes, aibo can
        //     add fields for newer servers without breaking older ones.
        let r = client.request(
            "account/read",
            json!({ "aiboUnknownFieldProbe": true, "refreshToken": false }),
        );
        out.probe(
            "unknown_param_field_ignored",
            r.is_ok(),
            "extra unknown key inside params",
        );

        // P3. What does an unknown *method* look like? aibo must be able to
        //     tell "your codex is too old" apart from a transport failure.
        let r = client.request("aibo/definitelyNotAMethod", json!({}));
        match &r {
            Ok(v) => out.kv("probe.unknown_method.result", &v.to_string()),
            Err(e) => out.kv("probe.unknown_method.error", &e.to_string()),
        }
        out.probe(
            "unknown_method_is_clean_error",
            r.is_err(),
            "unknown method returns a JSON-RPC error rather than killing the connection",
        );

        // P4. Is the connection still usable after an error? If a single bad
        //     request tears the server down, aibo needs a supervisor per call.
        let r = client.request("account/read", json!({}));
        out.probe(
            "connection_survives_error",
            r.is_ok(),
            "a normal request still succeeds after an unknown-method error",
        );

        // P5. Are string request ids accepted? RequestId is `string | i64` in
        //     the generated schema; confirm the server really honours strings.
        let r = client.request_raw(json!({
            "id": "aibo-string-id-probe",
            "method": "account/read",
            "params": {}
        }));
        out.probe("string_request_id_accepted", r.is_ok(), "id as a JSON string");

        // P6. Does `initialize` twice fail cleanly? aibo reconnects on crash and
        //     must not wedge itself.
        let r = client.request(
            "initialize",
            json!({ "clientInfo": { "name": "aibo-spike-s5", "version": "0.1.0" } }),
        );
        out.kv(
            "probe.double_initialize",
            &match &r {
                Ok(v) => format!("accepted: {v}"),
                Err(e) => format!("rejected: {e}"),
            },
        );
    }
}

// ---------------------------------------------------------------------------
// One real thread + turn
// ---------------------------------------------------------------------------

/// Start an ephemeral, read-only, never-approve thread and run one trivial turn.
///
/// This is the half of S5 that costs money, hence the `--thread` gate.
fn one_thread(client: &mut Client, out: &mut Report) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let started = client.request(
        "thread/start",
        json!({
            "cwd": cwd.to_string_lossy(),
            "sandbox": "read-only",
            "approvalPolicy": "never",
            "ephemeral": true,
        }),
    )?;
    let thread_id = started
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("thread/start result has no /thread/id: {started}"))?
        .to_string();
    out.kv("thread.id", &thread_id);
    out.kv(
        "thread.model",
        started.get("model").and_then(Value::as_str).unwrap_or("<none>"),
    );

    let t0 = Instant::now();
    let _turn = client.request(
        "turn/start",
        json!({
            "threadId": thread_id,
            "input": [ { "type": "text", "text": "Reply with exactly: OK" } ],
        }),
    )?;

    // Drain notifications until turn/completed, recording which methods we saw.
    let mut seen: Vec<String> = Vec::new();
    let mut first_delta_ms: Option<u128> = None;
    let mut answer = String::new();
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        let Some(note) = client.next_notification(deadline)? else {
            bail!("timed out waiting for turn/completed after {:?}", TURN_TIMEOUT);
        };
        let method = note.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        if !seen.contains(&method) {
            seen.push(method.clone());
        }
        match method.as_str() {
            "item/agentMessage/delta" => {
                if first_delta_ms.is_none() {
                    first_delta_ms = Some(t0.elapsed().as_millis());
                }
                if let Some(d) = note.pointer("/params/delta").and_then(Value::as_str) {
                    answer.push_str(d);
                }
            }
            "turn/completed" => break,
            "error" => bail!("server sent an `error` notification: {note}"),
            _ => {}
        }
    }
    out.kv("thread.turn_total_ms", &t0.elapsed().as_millis().to_string());
    out.kv(
        "thread.first_delta_ms",
        &first_delta_ms.map(|m| m.to_string()).unwrap_or_else(|| "<none>".into()),
    );
    out.kv("thread.answer", answer.trim());
    out.kv("thread.notification_methods", &seen.join(","));

    let _ = client.request("thread/archive", json!({ "threadId": thread_id }));
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal NDJSON client
// ---------------------------------------------------------------------------

/// A line read off the server's stdout, already parsed.
#[derive(Debug)]
enum Incoming {
    /// A response to one of our requests: has `id` and `result`/`error`.
    Response { id: Value, body: Value },
    /// A server-initiated notification: has `method`, no `id`.
    Notification(Value),
    /// A server-initiated *request*: has both `method` and `id`. aibo must
    /// answer these (approvals, attestation); the spike only records them.
    ServerRequest(Value),
    /// A line that did not parse as JSON at all.
    Garbage(String),
}

/// A dead-simple newline-delimited-JSON client for `codex app-server --stdio`.
///
/// Deliberately synchronous and dependency-light: this is a spike, not the
/// product transport. The product transport lives in `aibo-agent`.
struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Incoming>,
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: i64,
    /// Notifications received while waiting on a response, kept in order.
    pending_notifications: Vec<Value>,
    /// Responses that arrived out of order, keyed by stringified id.
    pending_responses: HashMap<String, Value>,
    /// Server-initiated requests observed, for the report.
    server_requests: Vec<Value>,
}

impl Client {
    /// Spawn `codex app-server --stdio` and start the stdout/stderr readers.
    fn spawn(codex: &str) -> Result<Self> {
        let mut child = Command::new(codex)
            .arg("app-server")
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr_pipe = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

        let (tx, rx): (Sender<Incoming>, Receiver<Incoming>) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let msg = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => classify(v),
                    Err(_) => Incoming::Garbage(line),
                };
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });

        let stderr = Arc::new(Mutex::new(Vec::new()));
        {
            let stderr = Arc::clone(&stderr);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr_pipe).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(mut g) = stderr.lock() {
                        g.push(line);
                    }
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            rx,
            stderr,
            next_id: 0,
            pending_notifications: Vec::new(),
            pending_responses: HashMap::new(),
            server_requests: Vec::new(),
        })
    }

    /// Allocate the next request id.
    fn next_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    /// Send a request and wait for its response. Returns the `result` value, or
    /// an error carrying the server's `error` object.
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id();
        self.request_raw(json!({ "id": id, "method": method, "params": params }))
    }

    /// Send a fully hand-built request frame. Used by the wire-shape probes.
    fn request_raw(&mut self, frame: Value) -> Result<Value> {
        let id = frame
            .get("id")
            .cloned()
            .ok_or_else(|| anyhow!("request_raw frame has no id"))?;
        self.write_line(&frame)?;
        let body = self.await_response(&id, Instant::now() + REQUEST_TIMEOUT)?;
        if let Some(err) = body.get("error") {
            bail!("{err}");
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send a notification (no id, no response expected).
    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_line(&json!({ "method": method, "params": params }))
    }

    fn write_line(&mut self, frame: &Value) -> Result<()> {
        writeln!(self.stdin, "{frame}")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Pump incoming messages until the response for `id` arrives.
    fn await_response(&mut self, id: &Value, deadline: Instant) -> Result<Value> {
        let key = id.to_string();
        if let Some(body) = self.pending_responses.remove(&key) {
            return Ok(body);
        }
        loop {
            match self.pump(deadline)? {
                Incoming::Response { id: got, body } => {
                    if got.to_string() == key {
                        return Ok(body);
                    }
                    self.pending_responses.insert(got.to_string(), body);
                }
                Incoming::Notification(v) => self.pending_notifications.push(v),
                Incoming::ServerRequest(v) => self.server_requests.push(v),
                Incoming::Garbage(line) => {
                    bail!("server emitted a non-JSON line on stdout: {line:?}")
                }
            }
        }
    }

    /// Return the next queued or freshly-arrived notification, or `None` on
    /// deadline.
    fn next_notification(&mut self, deadline: Instant) -> Result<Option<Value>> {
        if !self.pending_notifications.is_empty() {
            return Ok(Some(self.pending_notifications.remove(0)));
        }
        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.pump(deadline) {
                Ok(Incoming::Notification(v)) => return Ok(Some(v)),
                Ok(Incoming::Response { id, body }) => {
                    self.pending_responses.insert(id.to_string(), body);
                }
                Ok(Incoming::ServerRequest(v)) => self.server_requests.push(v),
                Ok(Incoming::Garbage(line)) => bail!("non-JSON line: {line:?}"),
                Err(_) => return Ok(None),
            }
        }
    }

    fn pump(&mut self, deadline: Instant) -> Result<Incoming> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(left) {
            Ok(msg) => Ok(msg),
            Err(RecvTimeoutError::Timeout) => bail!("timed out after {REQUEST_TIMEOUT:?}"),
            Err(RecvTimeoutError::Disconnected) => {
                let tail = self.stderr_snapshot();
                bail!("app-server closed stdout (stderr tail: {tail:?})")
            }
        }
    }

    /// Everything the server has written to stderr so far.
    fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Close stdin and reap the child.
    fn shutdown(mut self) {
        drop(self.stdin);
        let _ = self.child.wait();
    }
}

/// Sort a decoded frame into the four categories the spike cares about.
fn classify(v: Value) -> Incoming {
    let has_id = v.get("id").is_some();
    let has_method = v.get("method").is_some();
    match (has_id, has_method) {
        (true, true) => Incoming::ServerRequest(v),
        (true, false) => {
            let id = v.get("id").cloned().unwrap_or(Value::Null);
            Incoming::Response { id, body: v }
        }
        (false, true) => Incoming::Notification(v),
        (false, false) => Incoming::Garbage(v.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Collects key/value observations and prints them as prose or NDJSON.
struct Report {
    json: bool,
    rows: Vec<(String, String)>,
}

impl Report {
    /// Create a report; `json` selects NDJSON output.
    fn new(json: bool) -> Self {
        Self { json, rows: Vec::new() }
    }

    /// Record one observation.
    fn kv(&mut self, key: &str, value: &str) {
        if self.json {
            println!("{}", json!({ "k": key, "v": value }));
        } else {
            println!("{key:<38} {value}");
        }
        self.rows.push((key.to_string(), value.to_string()));
    }

    /// Record a boolean probe result with a human description.
    fn probe(&mut self, name: &str, pass: bool, what: &str) {
        let key = format!("probe.{name}");
        let value = format!("{} ({what})", if pass { "PASS" } else { "FAIL" });
        self.kv(&key, &value);
    }

    /// Print the closing verdict block.
    fn finish(&mut self) {
        if self.json {
            return;
        }
        println!("\n--- S5 verdict -------------------------------------------");
        let get = |k: &str| {
            self.rows
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("<not run>")
        };
        println!("handshake succeeds:        {}", get("initialize.ok"));
        println!("account/read succeeds:     {}", get("account_read.ok"));
        println!("protocol version in init:  {}", get("initialize.protocol_version_field"));
        println!("server userAgent:          {}", get("initialize.userAgent"));
        println!(
            "\nRead this as: if `protocol version in init` is ABSENT, a numeric\n\
             version floor is not obtainable from the handshake and aibo must\n\
             gate on `codex --version` plus permissive parsing (plan §3)."
        );
    }
}
