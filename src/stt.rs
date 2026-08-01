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
}

impl DictationHandle {
    /// Commit the turn: the task sends `input_audio_buffer.commit`, drains the
    /// final deltas, then reports [`UiEvent::DictationEnded`].
    pub fn finish(&self) {
        self.stop.cancel();
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
pub fn start(key: SecretString, events: mpsc::Sender<UiEvent>) -> DictationHandle {
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    tokio::spawn(crate::diagnostics::supervise("dictation", async move {
        run(key, events, task_stop).await;
    }));
    DictationHandle { stop }
}

async fn run(key: SecretString, events: mpsc::Sender<UiEvent>, stop: CancellationToken) {
    let emit = |event: UiEvent| {
        let events = events.clone();
        async move {
            let _ = events.send(event).await;
        }
    };

    // Microphone first: failing fast on a missing device or permission means
    // no socket is opened for audio that will never exist.
    let (chunk_tx, mut chunks) = mpsc::channel::<Vec<u8>>(64);
    let (ready_tx, ready_rx) = oneshot::channel();
    let mic_stop = stop.clone();
    let spawned = std::thread::Builder::new()
        .name("aibo-dictation-mic".into())
        .spawn(move || capture_thread(chunk_tx, ready_tx, mic_stop));
    if spawned.is_err() || !matches!(ready_rx.await, Ok(Ok(()))) {
        emit(UiEvent::DictationFailed {
            failure: DictationFailure::Microphone,
        })
        .await;
        return;
    }

    let request = match client_request(&key) {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(%error, "dictation could not build the websocket request");
            emit(UiEvent::DictationFailed {
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
                failure: DictationFailure::Connection,
            })
            .await;
            return;
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
                    "transcription": { "model": MODEL },
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
            failure: DictationFailure::Connection,
        })
        .await;
        return;
    }

    emit(UiEvent::DictationStarted).await;

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
                    emit(UiEvent::DictationEnded).await;
                    return;
                }
                let commit = serde_json::json!({ "type": "input_audio_buffer.commit" });
                if sink.send(WsMessage::Text(commit.to_string())).await.is_err() {
                    emit(UiEvent::DictationFailed {
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
                        emit(UiEvent::DictationDelta { text }).await;
                    }
                    Transcribed::Completed(_) | Transcribed::Other => {}
                    Transcribed::Closed => {
                        emit(UiEvent::DictationFailed {
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
    while let Ok(message) = tokio::time::timeout(DRAIN_TIMEOUT, source.next()).await {
        match transcription_event(message) {
            Transcribed::Delta(text) => {
                saw_delta_text = true;
                emit(UiEvent::DictationDelta { text }).await;
            }
            Transcribed::Completed(transcript) => {
                // Some paths deliver the whole transcript only here.
                if !saw_delta_text && !transcript.is_empty() {
                    emit(UiEvent::DictationDelta { text: transcript }).await;
                }
                break;
            }
            Transcribed::Other => {}
            Transcribed::Closed => break,
        }
    }
    let _ = sink.close().await;
    emit(UiEvent::DictationEnded).await;
}

/// What one websocket message means for the transcript.
enum Transcribed {
    Delta(String),
    Completed(String),
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
            Transcribed::Completed(String::new())
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
    let built = build_stream(chunks);
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
    while !stop.is_cancelled() {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
}

fn build_stream(chunks: mpsc::Sender<Vec<u8>>) -> anyhow::Result<cpal::Stream> {
    use anyhow::Context as _;

    let device = cpal::default_host()
        .default_input_device()
        .context("no default input device")?;
    let config = device
        .default_input_config()
        .context("no default input configuration")?;
    let channels = usize::from(config.channels());
    let mut state = CaptureState::new(f64::from(config.sample_rate().0), chunks);

    let err_fn = |error| tracing::warn!(%error, "microphone stream error");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| state.push(data, channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let converted: Vec<f32> = data.iter().map(|&s| f32::from(s) / 32_768.0).collect();
                state.push(&converted, channels);
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config.into(),
            move |data: &[u16], _: &cpal::InputCallbackInfo| {
                let converted: Vec<f32> = data
                    .iter()
                    .map(|&s| (f32::from(s) - 32_768.0) / 32_768.0)
                    .collect();
                state.push(&converted, channels);
            },
            err_fn,
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
    chunks: mpsc::Sender<Vec<u8>>,
}

impl CaptureState {
    fn new(in_rate: f64, chunks: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            resampler: MonoResampler::new(in_rate, f64::from(TARGET_RATE)),
            pending: Vec::with_capacity(CHUNK_SAMPLES * 2),
            chunks,
        }
    }

    fn push(&mut self, interleaved: &[f32], channels: usize) {
        let channels = channels.max(1);
        for frame in interleaved.chunks_exact(channels) {
            #[expect(clippy::cast_precision_loss, reason = "channel counts are tiny")]
            let mono = frame.iter().sum::<f32>() / channels as f32;
            self.resampler.push(mono, &mut self.pending);
        }
        while self.pending.len() >= CHUNK_SAMPLES {
            let chunk: Vec<i16> = self.pending.drain(..CHUNK_SAMPLES).collect();
            let mut bytes = Vec::with_capacity(chunk.len() * 2);
            for sample in chunk {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            // Dropped on overflow rather than blocking the audio callback.
            let _ = self.chunks.try_send(bytes);
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
