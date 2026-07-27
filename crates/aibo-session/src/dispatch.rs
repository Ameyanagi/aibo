//! Driving one attempt's stream: §13's cancellation and partial-result rules.
//!
//! This is the piece the rest of the engine is built around, because two of
//! §13's hardest constraints live here and nowhere else:
//!
//! * **Cancellation is not an afterthought.** The [`CancellationToken`] is
//!   selected against on every poll, so `esc` aborts within one event rather
//!   than at the end of the response. Dropping the stream is what actually
//!   aborts the HTTP request; the token is also passed to `Provider::chat` so
//!   the implementation can abort its own work.
//! * **"Never retry after a partial stream."** [`StreamOutcome::tokens_seen`]
//!   is the fact the caller needs to honour that: before the first token a
//!   failure may move down the role chain, after it the request is over. §13:
//!   *"a retry risks double-billing and duplicated output."*

use std::time::Instant;

use aibo_core::error::{AiboError, Result};
use aibo_core::types::{BoxStream, StopReason, StreamEvent, Usage};
use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::event::{EventSink, SessionEvent};

/// Largest cumulative text-plus-reasoning payload forwarded from one attempt.
///
/// Provider-side token limits are advisory and several compatible endpoints do
/// not accept one at all. This is the final local bound protecting both the
/// retained result and the UI event queue from an endless successful stream.
pub(crate) const MAX_STREAM_OUTPUT_BYTES: usize = 4 << 20;

/// Everything one attempt produced.
#[derive(Debug, Default)]
pub(crate) struct StreamOutcome {
    /// Assistant text, concatenated. Reasoning is excluded — §7 puts it on its
    /// own channel precisely so it is never inserted.
    pub text: String,
    /// The terminal stop reason, when the stream reached one.
    pub stop: Option<StopReason>,
    /// The last `Usage` event, when the provider sent one. §14: it *"never
    /// arrives on a cancelled or failed stream"*, which is why the cost was
    /// reserved before dispatch.
    pub usage: Option<Usage>,
    /// The error that ended the stream, if it ended badly.
    pub error: Option<AiboError>,
    /// Whether any assistant text arrived. The retry gate (§13).
    pub tokens_seen: bool,
    /// The user pressed `esc`, or a newer submission superseded this one.
    pub cancelled: bool,
    /// Milliseconds from `started` to the first text event (§15).
    pub first_token_ms: Option<u64>,
}

impl StreamOutcome {
    /// Whether this attempt may be retried on the next chain entry.
    ///
    /// Three separate reasons it may not, and they are not the same reason:
    /// tokens already reached the user, the user cancelled, or the error is
    /// not one §4 falls back on.
    pub(crate) fn may_fall_back(&self) -> bool {
        if self.tokens_seen || self.cancelled {
            return false;
        }
        self.error
            .as_ref()
            .is_some_and(AiboError::is_fallback_eligible)
    }
}

/// Consume a provider stream to completion, cancellation, the wall-clock
/// ceiling or failure.
///
/// Forwards every event to `events` verbatim, including `Reasoning` (the UI
/// renders it collapsed) and `Usage` (the spend meter reads the reconciled
/// figure from the return value instead, but the UI shows it live).
///
/// `deadline` is `EngineConfig::request_deadline` as an absolute instant. It is
/// enforced *here* rather than by wrapping this call in a `timeout`, because a
/// timeout would drop the future and take the partial text with it — and §13 is
/// explicit that a stream that stops mid-way leaves *"the partial text in the
/// panel marked truncated, with retry and copy actions"*. Expiry is therefore an
/// ordinary arm of the same select: it records
/// `Timeout { phase: Stream }` and breaks, keeping everything received so far.
pub(crate) async fn drive(
    mut stream: BoxStream<'static, Result<StreamEvent>>,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
    started: Instant,
    events: &EventSink,
) -> StreamOutcome {
    let mut out = StreamOutcome::default();
    let mut output_bytes = 0_usize;

    loop {
        let next = tokio::select! {
            // Biased so a cancellation that arrives at the same moment as a
            // chunk wins. §13 wants `esc` to feel immediate, and delivering one
            // more chunk after the panel has closed is pure waste.
            biased;
            () = cancel.cancelled() => {
                out.cancelled = true;
                break;
            }
            // The ceiling. Below cancellation because `esc` is the user's
            // decision and expiry is only the clock's, and above the stream so a
            // provider emitting a token every millisecond cannot starve it.
            () = tokio::time::sleep_until(deadline) => {
                // Cancelling is what aborts the in-flight HTTP request; the
                // `drop` below only stops us polling it.
                cancel.cancel();
                out.error = Some(AiboError::Timeout {
                    phase: aibo_core::error::TimeoutPhase::Stream,
                });
                break;
            }
            next = stream.next() => next,
        };

        let Some(item) = next else {
            // The stream ended without a `Done`. Treat it as a stall rather
            // than a success: a truncated response silently accepted is
            // exactly the partial insertion §13 forbids.
            if out.stop.is_none() {
                out.error = Some(AiboError::Timeout {
                    phase: aibo_core::error::TimeoutPhase::Stream,
                });
            }
            break;
        };

        match item {
            Ok(mut event) => {
                let output_limit_hit = limit_generated_output(&mut event, &mut output_bytes);

                if let StreamEvent::Text(chunk) = &event
                    && !chunk.is_empty()
                    && !out.tokens_seen
                {
                    out.tokens_seen = true;
                    let ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    out.first_token_ms = Some(ms);
                    if !events
                        .emit_stream(SessionEvent::FirstToken { elapsed_ms: ms }, cancel)
                        .await
                        && cancel.is_cancelled()
                    {
                        out.cancelled = true;
                        break;
                    }
                }

                match &event {
                    StreamEvent::Text(chunk) => out.text.push_str(chunk),
                    StreamEvent::Usage(usage) => out.usage = Some(*usage),
                    StreamEvent::Done(stop) => out.stop = Some(stop.clone()),
                    StreamEvent::Reasoning(_) | StreamEvent::ToolCall { .. } => {}
                }

                let done = matches!(event, StreamEvent::Done(_));
                let discard_limited_event = output_limit_hit
                    && (matches!(
                        &event,
                        StreamEvent::Text(chunk) | StreamEvent::Reasoning(chunk) if chunk.is_empty()
                    ) || matches!(&event, StreamEvent::ToolCall { .. }));
                if !discard_limited_event
                    && !events
                        .emit_stream(SessionEvent::Stream(Box::new(event)), cancel)
                        .await
                    && cancel.is_cancelled()
                {
                    out.cancelled = true;
                    break;
                }
                if output_limit_hit {
                    cancel.cancel();
                    out.error = Some(AiboError::Internal(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "provider output exceeded the {MAX_STREAM_OUTPUT_BYTES}-byte limit"
                        ),
                    ))));
                    break;
                }
                if done {
                    break;
                }
            }
            Err(error) => {
                out.error = Some(error);
                break;
            }
        }
    }

    // Dropping the stream is what aborts the underlying request; make that
    // explicit rather than relying on the end of the function.
    drop(stream);
    out
}

fn limit_generated_output(event: &mut StreamEvent, used: &mut usize) -> bool {
    let chunk = match event {
        StreamEvent::Text(chunk) | StreamEvent::Reasoning(chunk) => chunk,
        StreamEvent::ToolCall { id, name, args } => {
            // Count a fixed envelope as well as the variable fields so a
            // malicious stream of empty tool calls cannot bypass the byte
            // ceiling by producing zero-length values forever.
            let bytes = 64_usize
                .saturating_add(id.len())
                .saturating_add(name.len())
                .saturating_add(json_value_bytes(args));
            if used.saturating_add(bytes) > MAX_STREAM_OUTPUT_BYTES {
                return true;
            }
            *used += bytes;
            return false;
        }
        StreamEvent::Usage(_) | StreamEvent::Done(_) => {
            return false;
        }
    };
    let remaining = MAX_STREAM_OUTPUT_BYTES.saturating_sub(*used);
    if chunk.len() <= remaining {
        *used += chunk.len();
        return false;
    }

    let mut boundary = remaining;
    while !chunk.is_char_boundary(boundary) {
        boundary -= 1;
    }
    chunk.truncate(boundary);
    *used += chunk.len();
    true
}

fn json_value_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(number) => number.to_string().len(),
        serde_json::Value::String(string) => string.len().saturating_add(2),
        serde_json::Value::Array(items) => items.iter().fold(2, |bytes, item| {
            bytes
                .saturating_add(1)
                .saturating_add(json_value_bytes(item))
        }),
        serde_json::Value::Object(entries) => entries.iter().fold(2, |bytes, (key, value)| {
            bytes
                .saturating_add(key.len())
                .saturating_add(3)
                .saturating_add(json_value_bytes(value))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::error::TimeoutPhase;

    /// A deadline that will not fire during a unit test.
    fn far_future() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(3_600)
    }

    fn events_of(items: Vec<Result<StreamEvent>>) -> BoxStream<'static, Result<StreamEvent>> {
        Box::pin(futures::stream::iter(items))
    }

    #[tokio::test]
    async fn a_clean_stream_reports_text_stop_and_usage() {
        let (sink, mut rx) = EventSink::channel();
        let out = drive(
            events_of(vec![
                Ok(StreamEvent::Text("hello ".into())),
                Ok(StreamEvent::Text("world".into())),
                Ok(StreamEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Usage::default()
                })),
                Ok(StreamEvent::Done(StopReason::EndTurn)),
            ]),
            &CancellationToken::new(),
            far_future(),
            Instant::now(),
            &sink,
        )
        .await;

        assert_eq!(out.text, "hello world");
        assert_eq!(out.stop, Some(StopReason::EndTurn));
        assert_eq!(out.usage.unwrap().output_tokens, 2);
        assert!(out.tokens_seen);
        assert!(!out.may_fall_back());

        // FirstToken must precede the first Stream event.
        let first = rx.recv().await.unwrap();
        assert!(matches!(first, SessionEvent::FirstToken { .. }));
    }

    #[tokio::test]
    async fn an_error_before_any_token_may_fall_back() {
        let (sink, _rx) = EventSink::channel();
        let out = drive(
            events_of(vec![Err(AiboError::ProviderUnavailable {
                provider: aibo_core::types::ProviderId::GROQ,
                status: 503,
            })]),
            &CancellationToken::new(),
            far_future(),
            Instant::now(),
            &sink,
        )
        .await;

        assert!(!out.tokens_seen);
        assert!(out.may_fall_back());
    }

    #[tokio::test]
    async fn an_error_after_a_token_may_not_fall_back() {
        let (sink, _rx) = EventSink::channel();
        let out = drive(
            events_of(vec![
                Ok(StreamEvent::Text("half a rewr".into())),
                Err(AiboError::ProviderUnavailable {
                    provider: aibo_core::types::ProviderId::GROQ,
                    status: 503,
                }),
            ]),
            &CancellationToken::new(),
            far_future(),
            Instant::now(),
            &sink,
        )
        .await;

        assert_eq!(out.text, "half a rewr");
        assert!(out.tokens_seen);
        assert!(
            !out.may_fall_back(),
            "§13: never retry after a partial stream — it double-bills and duplicates output"
        );
    }

    #[tokio::test]
    async fn a_stream_that_just_stops_is_a_stall_not_a_success() {
        let (sink, _rx) = EventSink::channel();
        let out = drive(
            events_of(vec![Ok(StreamEvent::Text("partial".into()))]),
            &CancellationToken::new(),
            far_future(),
            Instant::now(),
            &sink,
        )
        .await;

        assert!(out.stop.is_none());
        assert!(matches!(
            out.error,
            Some(AiboError::Timeout {
                phase: TimeoutPhase::Stream
            })
        ));
    }

    #[tokio::test]
    async fn cancellation_wins_over_a_pending_chunk() {
        let (sink, _rx) = EventSink::channel();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let out = drive(
            events_of(vec![Ok(StreamEvent::Text("should not arrive".into()))]),
            &cancel,
            far_future(),
            Instant::now(),
            &sink,
        )
        .await;

        assert!(out.cancelled);
        assert!(out.text.is_empty());
        assert!(!out.may_fall_back());
    }

    #[tokio::test]
    async fn cumulative_output_is_bounded_on_a_utf8_boundary() {
        let (sink, mut rx) = EventSink::channel();
        let cancel = CancellationToken::new();
        // The byte limit falls in the middle of the final two-byte character.
        let oversized = format!("{}é", "x".repeat(MAX_STREAM_OUTPUT_BYTES - 1));

        let out = drive(
            events_of(vec![
                Ok(StreamEvent::Text(oversized)),
                Ok(StreamEvent::Done(StopReason::EndTurn)),
            ]),
            &cancel,
            far_future(),
            Instant::now(),
            &sink,
        )
        .await;

        assert_eq!(out.text.len(), MAX_STREAM_OUTPUT_BYTES - 1);
        assert!(out.text.is_char_boundary(out.text.len()));
        assert!(matches!(out.error, Some(AiboError::Internal(_))));
        assert!(out.stop.is_none());
        assert!(cancel.is_cancelled());

        assert!(matches!(
            rx.recv().await,
            Some(SessionEvent::FirstToken { .. })
        ));
        let Some(SessionEvent::Stream(event)) = rx.recv().await else {
            panic!("the bounded partial text event must be forwarded");
        };
        assert!(matches!(
            *event,
            StreamEvent::Text(ref text) if text.len() == MAX_STREAM_OUTPUT_BYTES - 1
        ));
        assert!(
            rx.try_recv().is_err(),
            "the terminal event must not leak through"
        );
    }

    #[tokio::test]
    async fn oversized_tool_arguments_are_rejected_before_ui_fan_out() {
        let (sink, mut rx) = EventSink::channel();
        let cancel = CancellationToken::new();
        let out = drive(
            events_of(vec![
                Ok(StreamEvent::ToolCall {
                    id: "call-1".into(),
                    name: "run".into(),
                    args: serde_json::json!({
                        "input": "x".repeat(MAX_STREAM_OUTPUT_BYTES),
                    }),
                }),
                Ok(StreamEvent::Done(StopReason::ToolUse)),
            ]),
            &cancel,
            far_future(),
            Instant::now(),
            &sink,
        )
        .await;

        assert!(matches!(out.error, Some(AiboError::Internal(_))));
        assert!(out.stop.is_none());
        assert!(cancel.is_cancelled());
        assert!(
            rx.try_recv().is_err(),
            "the oversized tool call must not reach the bounded UI queue"
        );
    }
}
