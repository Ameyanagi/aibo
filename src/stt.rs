//! Push-to-talk dictation: microphone → OpenAI realtime transcription (§P9+).
//!
//! One turn per invocation. `⌘L` starts the microphone and a websocket to the
//! realtime transcription endpoint; text streams back as
//! [`UiEvent::DictationDelta`]s the panel appends to its input; `⌘L` again
//! commits the audio turn, drains the final fragments, and closes both.
//!
//! Protocol, from the transcription guides (checked 2026-08-01):
//!
//! * connect `wss://api.openai.com/v1/realtime?intent=transcription` with an
//!   `Authorization: Bearer` API key;
//! * configure with `session.update` — `session.type = "transcription"`,
//!   `audio/pcm` at 24 kHz, model `gpt-live-transcribe`, `turn_detection`
//!   `null` so the turn boundary is the user's key and not a server VAD;
//! * stream base64 PCM16 via `input_audio_buffer.append`, end the turn with
//!   `input_audio_buffer.commit`;
//! * read `conversation.item.input_audio_transcription.delta` /
//!   `…completed` events back.
//!
//! The microphone runs on its own OS thread: cpal streams are driven by a
//! realtime audio callback and are not `Send`, and the callback must never
//! block — chunks are handed to the websocket task through a bounded channel
//! and dropped on overflow rather than stalling the device.
//!
//! The Azure flavour (owner request, 2026-08-03) speaks the same GA realtime
//! protocol against a Foundry deployment of the same model. Its wire facts
//! were probed live: the realtime gateway answers only on the resource's
//! `*.openai.azure.com` alias host, authenticates with an `api-key` header,
//! and the batch fallback uses the classic deployment-scoped
//! `audio/transcriptions` route with a preview `api-version`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::{SinkExt as _, StreamExt as _};
use secrecy::{ExposeSecret as _, SecretString};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_util::sync::CancellationToken;

use aibo_ui::{DictationFailure, UiEvent};

/// The transcription-intent realtime endpoint.
const REALTIME_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";

/// The live transcription model the owner asked for by name.
const MODEL: &str = "gpt-live-transcribe";

/// The sample rate the endpoint expects.
const TARGET_RATE: u32 = 24_000;

/// ~100 ms of 24 kHz mono i16 per websocket frame: small enough to feel live,
/// large enough that base64 and JSON framing stay negligible.
const CHUNK_SAMPLES: usize = 2_400;

/// How long the post-commit drain waits for the transcriber's last word.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// A running dictation. Dropping it (app shutdown, panel teardown) commits the
/// turn exactly like [`DictationHandle::finish`]; the task bounds its own
/// drain, so nothing outlives [`DRAIN_TIMEOUT`].
pub struct DictationHandle {
    stop: CancellationToken,
    finished: Arc<AtomicBool>,
}

impl DictationHandle {
    /// Commit the turn: the task sends `input_audio_buffer.commit`, drains the
    /// final deltas, then reports [`UiEvent::DictationEnded`].
    pub fn finish(&self) {
        self.stop.cancel();
    }

    /// Whether the worker has fully drained and published its terminal event.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl Drop for DictationHandle {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Start a dictation turn. Failure is reported through `events`, never
/// returned: the caller is the request loop, and §13 puts every user-visible
/// error on the event channel.
pub fn start(key: SecretString, events: mpsc::Sender<UiEvent>, turn: u64) -> DictationHandle {
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let task_finished = Arc::clone(&finished);
    tokio::spawn(crate::diagnostics::supervise("dictation", async move {
        run(key, events, task_stop, turn).await;
        task_finished.store(true, Ordering::Release);
    }));
    DictationHandle { stop, finished }
}

/// Start a dictation turn against the ChatGPT plan's transcription endpoint
/// (owner request, 2026-08-02): record the whole turn, then one authenticated
/// upload to `chatgpt.com/backend-api/transcribe`.
///
/// Whisper-shaped rather than streaming — the text arrives after the second
/// `⌘L`, like the ChatGPT apps' own voice input. The endpoint is not part of
/// any published API, so failure is expected to be possible on any day and is
/// reported as an ordinary connection failure the user can act on by
/// switching the STT method in Settings.
pub fn start_chatgpt(
    tokens: std::sync::Arc<aibo_provider::auth::RefreshingTokenProvider>,
    events: mpsc::Sender<UiEvent>,
    turn: u64,
) -> DictationHandle {
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let task_finished = Arc::clone(&finished);
    tokio::spawn(crate::diagnostics::supervise(
        "dictation-chatgpt",
        async move {
            run_chatgpt(tokens, events, task_stop, turn).await;
            task_finished.store(true, Ordering::Release);
        },
    ));
    DictationHandle { stop, finished }
}

/// Everything the Azure dictation backends need (owner request, 2026-08-03).
pub struct AzureStt {
    /// The Foundry resource endpoint, either host spelling.
    pub endpoint: String,
    /// The realtime deployment; the model is named in-session.
    pub live_deployment: String,
    /// The batch deployment for the fallback upload.
    pub batch_deployment: String,
    /// The resource key (`api-key` header on both routes).
    pub key: SecretString,
}

/// Start a dictation turn against Azure: streaming via the realtime gateway,
/// falling back to a record-then-upload turn when the socket cannot open —
/// the user already granted the microphone, and "switch backends and press
/// ⌘L again" is a worse answer than a transcript that arrives at the end.
pub fn start_azure(azure: AzureStt, events: mpsc::Sender<UiEvent>, turn: u64) -> DictationHandle {
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let task_finished = Arc::clone(&finished);
    tokio::spawn(crate::diagnostics::supervise(
        "dictation-azure",
        async move {
            run_azure(azure, events, task_stop, turn).await;
            task_finished.store(true, Ordering::Release);
        },
    ));
    DictationHandle { stop, finished }
}

async fn run_azure(
    azure: AzureStt,
    events: mpsc::Sender<UiEvent>,
    stop: CancellationToken,
    turn: u64,
) {
    let emit = |event: UiEvent| {
        let events = events.clone();
        async move {
            let _ = events.send(event).await;
        }
    };

    let Some(mut chunks) = open_microphone(&events, &stop, turn).await else {
        return;
    };

    let connected = match azure_realtime_request(&azure.endpoint, &azure.key) {
        Ok(request) => connect_async(request).await,
        Err(error) => Err(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::other(error.to_string()),
        )),
    };
    match connected {
        Ok((ws, _response)) => {
            stream_turn(
                ws,
                &azure.live_deployment,
                &mut chunks,
                &events,
                &stop,
                turn,
            )
            .await;
        }
        Err(error) => {
            // The gateway said no (region, firewall, a renamed deployment) —
            // fall back to one upload per turn rather than failing a
            // microphone that is already live.
            tracing::warn!(%error, "azure realtime unavailable; batch transcription fallback");
            emit(UiEvent::DictationStarted { turn }).await;
            let Some(pcm) = buffer_turn(&mut chunks, &stop, &events, turn).await else {
                return;
            };
            if pcm.is_empty() {
                emit(UiEvent::DictationEnded { turn }).await;
                return;
            }
            match azure_transcribe_upload(&azure, wav_bytes(&pcm)).await {
                Some(text) if !text.is_empty() => {
                    emit(UiEvent::DictationDelta { turn, text }).await;
                    emit(UiEvent::DictationEnded { turn }).await;
                }
                Some(_) => emit(UiEvent::DictationEnded { turn }).await,
                None => {
                    emit(UiEvent::DictationFailed {
                        turn,
                        failure: DictationFailure::Connection,
                    })
                    .await;
                }
            }
        }
    }
}

/// The realtime websocket request for a Foundry resource.
///
/// Probed 2026-08-03: only the `*.openai.azure.com` alias serves the realtime
/// gateway — the `*.services.ai.azure.com` spelling 404s — and the model must
/// NOT ride the query string (400 `OperationNotSupported`); it is named
/// in-session by [`stream_turn`].
fn azure_realtime_request(
    endpoint: &str,
    key: &SecretString,
) -> anyhow::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let url = azure_realtime_url(endpoint)?;
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("api-key", key.expose_secret().parse()?);
    Ok(request)
}

/// `https://{res}.services.ai.azure.com` (or the alias itself, or a stray
/// trailing slash) → `wss://{res}.openai.azure.com/openai/v1/realtime?…`.
fn azure_realtime_url(endpoint: &str) -> anyhow::Result<String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let host = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed)
        .replace(".services.ai.azure.com", ".openai.azure.com");
    anyhow::ensure!(
        !host.is_empty() && !host.contains('/'),
        "the Azure endpoint should be a bare resource URL, got {endpoint:?}"
    );
    Ok(format!(
        "wss://{host}/openai/v1/realtime?intent=transcription"
    ))
}

/// POST the recorded turn to the deployment-scoped transcription route.
///
/// The `v1` audio path 404s on Foundry resources (probed 2026-08-03), so this
/// is the classic spelling with a preview `api-version`.
async fn azure_transcribe_upload(azure: &AzureStt, wav: Vec<u8>) -> Option<String> {
    let endpoint = azure.endpoint.trim().trim_end_matches('/');
    let url = format!(
        "{endpoint}/openai/deployments/{deployment}/audio/transcriptions?api-version=2025-03-01-preview",
        deployment = azure.batch_deployment,
    );
    let (content_type, body) = multipart_wav(wav);
    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .header("api-key", azure.key.expose_secret())
        .header("Content-Type", content_type)
        .body(body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), "azure transcribe refused the upload");
        return None;
    }
    let value: serde_json::Value = response.json().await.ok()?;
    value
        .get("text")
        .and_then(|text| text.as_str())
        .map(|text| text.trim().to_owned())
}

/// Spawn the microphone thread and wait for it to come up.
///
/// `None` means the failure has already been reported through `events`.
async fn open_microphone(
    events: &mpsc::Sender<UiEvent>,
    stop: &CancellationToken,
    turn: u64,
) -> Option<mpsc::Receiver<Vec<u8>>> {
    let (chunk_tx, chunks) = mpsc::channel::<Vec<u8>>(64);
    let (ready_tx, ready_rx) = oneshot::channel();
    let mic_stop = stop.clone();
    let spawned = std::thread::Builder::new()
        .name("aibo-dictation-mic".into())
        .spawn(move || capture_thread(chunk_tx, ready_tx, mic_stop));
    if spawned.is_err() || !matches!(ready_rx.await, Ok(Ok(()))) {
        let _ = events
            .send(UiEvent::DictationFailed {
                turn,
                failure: DictationFailure::Microphone,
            })
            .await;
        return None;
    }
    Some(chunks)
}

/// Buffer PCM until the user ends the turn. Bounded: at 48 KB/s the cap below
/// is several minutes of speech, and a dictation longer than that deserves
/// the streaming backend anyway.
///
/// `None` means the microphone died mid-turn and the failure is reported.
async fn buffer_turn(
    chunks: &mut mpsc::Receiver<Vec<u8>>,
    stop: &CancellationToken,
    events: &mpsc::Sender<UiEvent>,
    turn: u64,
) -> Option<Vec<u8>> {
    const MAX_PCM_BYTES: usize = 24_000 * 2 * 300;
    let mut pcm: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            chunk = chunks.recv() => {
                let Some(bytes) = chunk else {
                    let _ = events
                        .send(UiEvent::DictationFailed {
                            turn,
                            failure: DictationFailure::Microphone,
                        })
                        .await;
                    return None;
                };
                if pcm.len() + bytes.len() <= MAX_PCM_BYTES {
                    pcm.extend_from_slice(&bytes);
                }
            }
        }
    }
    Some(pcm)
}

/// One `multipart/form-data` body holding `wav` as its `file` field.
fn multipart_wav(wav: Vec<u8>) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "aibo-dictation-boundary";
    let mut body = Vec::with_capacity(wav.len() + 256);
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"dictation.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&wav);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// The upload flavour: same microphone pipeline, buffered instead of
/// streamed.
async fn run_chatgpt(
    tokens: std::sync::Arc<aibo_provider::auth::RefreshingTokenProvider>,
    events: mpsc::Sender<UiEvent>,
    stop: CancellationToken,
    turn: u64,
) {
    use aibo_core::types::TokenProvider as _;

    let emit = |event: UiEvent| {
        let events = events.clone();
        async move {
            let _ = events.send(event).await;
        }
    };

    let Some(mut chunks) = open_microphone(&events, &stop, turn).await else {
        return;
    };

    emit(UiEvent::DictationStarted { turn }).await;

    let Some(pcm) = buffer_turn(&mut chunks, &stop, &events, turn).await else {
        return;
    };
    if pcm.is_empty() {
        emit(UiEvent::DictationEnded { turn }).await;
        return;
    }

    // Phase 2 — one upload. The token comes fresh from the refresh flow; the
    // account id rides in the header the backend keys plans off.
    let outcome = async {
        let token = tokens.token().await.ok()?;
        let account = tokens.account_id().await;
        transcribe_upload(&token, account.as_deref(), wav_bytes(&pcm)).await
    }
    .await;
    match outcome {
        Some(text) if !text.is_empty() => {
            emit(UiEvent::DictationDelta { turn, text }).await;
            emit(UiEvent::DictationEnded { turn }).await;
        }
        Some(_) => emit(UiEvent::DictationEnded { turn }).await,
        None => {
            emit(UiEvent::DictationFailed {
                turn,
                failure: DictationFailure::Connection,
            })
            .await;
        }
    }
}

/// POST the recorded turn to the ChatGPT backend; `Some(text)` on success.
async fn transcribe_upload(
    token: &SecretString,
    account: Option<&str>,
    wav: Vec<u8>,
) -> Option<String> {
    let (content_type, body) = multipart_wav(wav);
    let client = reqwest::Client::new();
    let response = client
        .post("https://chatgpt.com/backend-api/transcribe")
        .bearer_auth(token.expose_secret())
        .header("Content-Type", content_type)
        .header("chatgpt-account-id", account.unwrap_or_default())
        .body(body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), "chatgpt transcribe refused the upload");
        return None;
    }
    let value: serde_json::Value = response.json().await.ok()?;
    value
        .get("text")
        .or_else(|| value.get("transcription"))
        .and_then(|text| text.as_str())
        .map(|text| text.trim().to_owned())
}

/// A minimal 16-bit mono PCM WAV container around the captured samples.
fn wav_bytes(pcm: &[u8]) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let byte_rate = TARGET_RATE * 2;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&TARGET_RATE.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

async fn run(key: SecretString, events: mpsc::Sender<UiEvent>, stop: CancellationToken, turn: u64) {
    let emit = |event: UiEvent| {
        let events = events.clone();
        async move {
            let _ = events.send(event).await;
        }
    };

    // Microphone first: failing fast on a missing device or permission means
    // no socket is opened for audio that will never exist.
    let Some(mut chunks) = open_microphone(&events, &stop, turn).await else {
        return;
    };

    let request = match client_request(&key) {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(%error, "dictation could not build the websocket request");
            emit(UiEvent::DictationFailed {
                turn,
                failure: DictationFailure::Connection,
            })
            .await;
            return;
        }
    };
    let (ws, _response) = match connect_async(request).await {
        Ok(connected) => connected,
        Err(error) => {
            tracing::warn!(%error, "dictation could not reach the transcriber");
            emit(UiEvent::DictationFailed {
                turn,
                failure: DictationFailure::Connection,
            })
            .await;
            return;
        }
    };
    stream_turn(ws, MODEL, &mut chunks, &events, &stop, turn).await;
}

/// The streaming turn: configure the session, relay audio until the user's
/// key ends it, drain the final fragments. Shared verbatim between the OpenAI
/// and Azure realtime backends — both speak the GA transcription protocol,
/// and the only differences (URL, auth header) live in the callers.
async fn stream_turn(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    model: &str,
    chunks: &mut mpsc::Receiver<Vec<u8>>,
    events: &mpsc::Sender<UiEvent>,
    stop: &CancellationToken,
    turn: u64,
) {
    let emit = |event: UiEvent| {
        let events = events.clone();
        async move {
            let _ = events.send(event).await;
        }
    };
    let (mut sink, mut source) = ws.split();

    let configure = serde_json::json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": TARGET_RATE },
                    "transcription": { "model": model },
                    // The turn boundary is the user's key, not a server VAD.
                    "turn_detection": null,
                }
            }
        }
    });
    if sink
        .send(WsMessage::Text(configure.to_string()))
        .await
        .is_err()
    {
        emit(UiEvent::DictationFailed {
            turn,
            failure: DictationFailure::Connection,
        })
        .await;
        return;
    }

    emit(UiEvent::DictationStarted { turn }).await;

    // Whether any delta text reached the UI, across BOTH phases. Tracking
    // only the post-commit drain re-emitted the entire transcript after live
    // deltas had already typed it — every dictation appeared twice (owner
    // report, 2026-08-01).
    let mut saw_delta_text = false;

    // Phase 1 — stream audio until the user finishes the turn.
    let mut appended = false;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => {
                // A commit with no audio behind it is a server error, not a
                // turn — the toggle was pressed twice before the first chunk
                // left the machine. End quietly instead of asking the
                // transcriber to transcribe nothing.
                if !appended {
                    let _ = sink.close().await;
                    emit(UiEvent::DictationEnded { turn }).await;
                    return;
                }
                let commit = serde_json::json!({ "type": "input_audio_buffer.commit" });
                if sink.send(WsMessage::Text(commit.to_string())).await.is_err() {
                    emit(UiEvent::DictationFailed {
                        turn,
                        failure: DictationFailure::Connection,
                    })
                    .await;
                    return;
                }
                break;
            }
            chunk = chunks.recv() => {
                let Some(pcm) = chunk else {
                    // The microphone thread died mid-turn.
                    emit(UiEvent::DictationFailed {
                        turn,
                        failure: DictationFailure::Microphone,
                    })
                    .await;
                    return;
                };
                let append = serde_json::json!({
                    "type": "input_audio_buffer.append",
                    "audio": base64::engine::general_purpose::STANDARD.encode(&pcm),
                });
                appended = true;
                if sink.send(WsMessage::Text(append.to_string())).await.is_err() {
                    emit(UiEvent::DictationFailed {
                        turn,
                        failure: DictationFailure::Connection,
                    })
                    .await;
                    return;
                }
            }
            message = source.next() => {
                match transcription_event(message) {
                    Transcribed::Delta(text) => {
                        saw_delta_text = true;
                        emit(UiEvent::DictationDelta { turn, text }).await;
                    }
                    Transcribed::Completed(_) | Transcribed::Other => {}
                    Transcribed::Error => {
                        emit(UiEvent::DictationFailed {
                            turn,
                            failure: DictationFailure::Connection,
                        })
                        .await;
                        return;
                    }
                    Transcribed::Closed => {
                        emit(UiEvent::DictationFailed {
                            turn,
                            failure: DictationFailure::Connection,
                        })
                        .await;
                        return;
                    }
                }
            }
        }
    }

    // Phase 2 — drain the committed turn's final fragments, bounded. An
    // "error" here (for instance, a commit of near-silence) still ends the
    // dictation quietly: the user asked to stop, and there is nothing to fix.
    let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
    while let Ok(message) = tokio::time::timeout_at(deadline, source.next()).await {
        match transcription_event(message) {
            Transcribed::Delta(text) => {
                saw_delta_text = true;
                emit(UiEvent::DictationDelta { turn, text }).await;
            }
            Transcribed::Completed(transcript) => {
                // Some paths deliver the whole transcript only here.
                if !saw_delta_text && !transcript.is_empty() {
                    emit(UiEvent::DictationDelta {
                        turn,
                        text: transcript,
                    })
                    .await;
                }
                break;
            }
            Transcribed::Other | Transcribed::Error => {}
            Transcribed::Closed => break,
        }
    }
    let _ = sink.close().await;
    emit(UiEvent::DictationEnded { turn }).await;
}

/// What one websocket message means for the transcript.
enum Transcribed {
    Delta(String),
    Completed(String),
    Error,
    Other,
    Closed,
}

type WsResult = Result<WsMessage, tokio_tungstenite::tungstenite::Error>;

fn transcription_event(message: Option<WsResult>) -> Transcribed {
    let Some(Ok(message)) = message else {
        return Transcribed::Closed;
    };
    let WsMessage::Text(text) = message else {
        return Transcribed::Other;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Transcribed::Other;
    };
    match value.get("type").and_then(|t| t.as_str()) {
        Some(kind) if kind.ends_with("input_audio_transcription.delta") => Transcribed::Delta(
            value
                .get("delta")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_owned(),
        ),
        Some(kind) if kind.ends_with("input_audio_transcription.completed") => {
            Transcribed::Completed(
                value
                    .get("transcript")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            )
        }
        Some("error") => {
            tracing::warn!(payload = %text, "transcriber reported an error");
            Transcribed::Error
        }
        _ => Transcribed::Other,
    }
}

fn client_request(
    key: &SecretString,
) -> anyhow::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut request = REALTIME_URL.into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", key.expose_secret()).parse()?,
    );
    Ok(request)
}

/// Owns the cpal stream for the turn's lifetime.
///
/// The audio callback runs on the OS's realtime thread: it must not block and
/// must not allocate more than it can help, which is why overflowing chunks
/// are dropped (a 100 ms gap in a dictation is survivable; a glitching input
/// device is not).
fn capture_thread(
    chunks: mpsc::Sender<Vec<u8>>,
    ready: oneshot::Sender<Result<(), ()>>,
    stop: CancellationToken,
) {
    let failed = Arc::new(AtomicBool::new(false));
    let built = build_stream(chunks, Arc::clone(&failed));
    let stream = match built {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "the microphone could not be opened");
            let _ = ready.send(Err(()));
            return;
        }
    };
    if let Err(error) = stream.play() {
        tracing::warn!(%error, "the microphone stream would not start");
        let _ = ready.send(Err(()));
        return;
    }
    let _ = ready.send(Ok(()));
    while !stop.is_cancelled() && !failed.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
}

fn build_stream(
    chunks: mpsc::Sender<Vec<u8>>,
    failed: Arc<AtomicBool>,
) -> anyhow::Result<cpal::Stream> {
    use anyhow::Context as _;

    let device = cpal::default_host()
        .default_input_device()
        .context("no default input device")?;
    let config = device
        .default_input_config()
        .context("no default input configuration")?;
    let channels = usize::from(config.channels());
    let mut state = CaptureState::new(f64::from(config.sample_rate().0), chunks);

    let err_fn = || {
        let failed = Arc::clone(&failed);
        move |error| {
            tracing::warn!(%error, "microphone stream error");
            failed.store(true, Ordering::Release);
        }
    };
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                state.push_converted(data, channels, |sample| sample)
            },
            err_fn(),
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                state.push_converted(data, channels, |sample| f32::from(sample) / 32_768.0);
            },
            err_fn(),
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config.into(),
            move |data: &[u16], _: &cpal::InputCallbackInfo| {
                state.push_converted(data, channels, |sample| {
                    (f32::from(sample) - 32_768.0) / 32_768.0
                });
            },
            err_fn(),
            None,
        )?,
        other => anyhow::bail!("unsupported microphone sample format {other:?}"),
    };
    Ok(stream)
}

/// Interleaved device samples → mono 24 kHz i16 chunks on the channel.
struct CaptureState {
    resampler: MonoResampler,
    pending: Vec<i16>,
    pending_start: usize,
    chunks: mpsc::Sender<Vec<u8>>,
}

impl CaptureState {
    fn new(in_rate: f64, chunks: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            resampler: MonoResampler::new(in_rate, f64::from(TARGET_RATE)),
            pending: Vec::with_capacity(CHUNK_SAMPLES * 2),
            pending_start: 0,
            chunks,
        }
    }

    fn push_converted<T: Copy>(
        &mut self,
        interleaved: &[T],
        channels: usize,
        convert: impl Fn(T) -> f32,
    ) {
        let channels = channels.max(1);
        for frame in interleaved.chunks_exact(channels) {
            #[expect(clippy::cast_precision_loss, reason = "channel counts are tiny")]
            let mono = frame.iter().copied().map(&convert).sum::<f32>() / channels as f32;
            self.resampler.push(mono, &mut self.pending);
        }
        while self.pending.len().saturating_sub(self.pending_start) >= CHUNK_SAMPLES {
            let end = self.pending_start + CHUNK_SAMPLES;
            let mut bytes = Vec::with_capacity(CHUNK_SAMPLES * 2);
            for sample in &self.pending[self.pending_start..end] {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            self.pending_start = end;
            // Dropped on overflow rather than blocking the audio callback.
            let _ = self.chunks.try_send(bytes);
        }
        // Compact occasionally instead of shifting the whole pending buffer
        // for every 100 ms chunk.
        if self.pending_start >= CHUNK_SAMPLES * 8 {
            self.pending.drain(..self.pending_start);
            self.pending_start = 0;
        }
    }
}

/// Linear resampler, one sample at a time, carrying phase across callbacks.
///
/// Linear interpolation is audibly imperfect and analytically fine here: the
/// consumer is a speech model, not an ear, and the alternative is shipping a
/// windowed-sinc dependency for a push-to-talk feature.
struct MonoResampler {
    /// Output spacing in input-sample units: `in_rate / out_rate`.
    step: f64,
    /// Position of the next output between `prev` (0.0) and the incoming
    /// sample (1.0).
    pos: f64,
    prev: f32,
    primed: bool,
}

impl MonoResampler {
    fn new(in_rate: f64, out_rate: f64) -> Self {
        Self {
            step: in_rate / out_rate,
            pos: 0.0,
            prev: 0.0,
            primed: false,
        }
    }

    fn push(&mut self, sample: f32, out: &mut Vec<i16>) {
        if !self.primed {
            self.prev = sample;
            self.primed = true;
            return;
        }
        while self.pos < 1.0 {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the interpolation factor is in [0, 1)"
            )]
            let factor = self.pos as f32;
            let value = self.prev + (sample - self.prev) * factor;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "clamped to i16's range first"
            )]
            out.push((f64::from(value.clamp(-1.0, 1.0)) * 32_767.0) as i16);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = sample;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_48k_to_24k_keeps_every_other_sample() {
        let mut resampler = MonoResampler::new(48_000.0, 24_000.0);
        let mut out = Vec::new();
        for i in 0..8 {
            #[expect(clippy::cast_precision_loss, reason = "tiny test values")]
            resampler.push(i as f32 / 10.0, &mut out);
        }
        // One output for every two inputs, minus priming.
        assert_eq!(out.len(), 4, "{out:?}");
    }

    #[test]
    fn odd_ratios_stay_close_to_the_expected_count() {
        let mut resampler = MonoResampler::new(44_100.0, 24_000.0);
        let mut out = Vec::new();
        for _ in 0..44_100 {
            resampler.push(0.25, &mut out);
        }
        let expected = 24_000;
        assert!(
            (out.len() as i64 - expected).abs() <= 2,
            "one second of 44.1 kHz should yield ~{expected}, got {}",
            out.len()
        );
    }

    /// The probe's hard-won facts, pinned: alias host, wss scheme, no model
    /// in the query string — and portal paste variants all normalize.
    #[test]
    fn azure_realtime_url_normalizes_every_portal_spelling() {
        for endpoint in [
            "https://res.services.ai.azure.com",
            "https://res.services.ai.azure.com/",
            "https://res.openai.azure.com",
            "res.services.ai.azure.com",
        ] {
            assert_eq!(
                azure_realtime_url(endpoint).unwrap(),
                "wss://res.openai.azure.com/openai/v1/realtime?intent=transcription",
                "{endpoint}"
            );
        }
        assert!(azure_realtime_url("https://res.services.ai.azure.com/api/projects/x").is_err());
    }

    #[test]
    fn delta_and_completed_events_parse() {
        let delta = Some(Ok(WsMessage::Text(
            r#"{"type":"conversation.item.input_audio_transcription.delta","delta":"hel"}"#
                .to_owned(),
        )));
        assert!(matches!(
            transcription_event(delta),
            Transcribed::Delta(text) if text == "hel"
        ));

        let completed = Some(Ok(WsMessage::Text(
            r#"{"type":"conversation.item.input_audio_transcription.completed","transcript":"hello"}"#
                .to_owned(),
        )));
        assert!(matches!(
            transcription_event(completed),
            Transcribed::Completed(text) if text == "hello"
        ));

        assert!(matches!(transcription_event(None), Transcribed::Closed));
    }
}
