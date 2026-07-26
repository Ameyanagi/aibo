//! SSE framing, decoder plumbing and cancellation.
//!
//! §10 lists "per-provider SSE framing and terminator conventions" as one of
//! the things "OpenAI-compatible" does not cover. The byte-level parse is
//! shared ([`eventsource_stream`]); what differs — whether a `[DONE]` sentinel
//! is sent, whether events are named, where `usage` lands, whether a terminal
//! event exists at all — is expressed by the per-provider [`SseDecoder`] and
//! its quirk flags.
//!
//! The decoder is a **pure state machine**: `&Event` in, [`StreamEvent`]s out.
//! That is what lets `tests/golden.rs` replay recorded traffic with no network,
//! which §10 asks for explicitly.

use aibo_core::error::{AiboError, Result};
use aibo_core::types::{BoxStream, ProviderId, StopReason, StreamEvent};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

/// The `data:` payload every OpenAI-derived endpoint uses to close a stream.
pub const DONE_SENTINEL: &str = "[DONE]";

/// What a decoder wants to happen after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep reading.
    Continue,
    /// The logical stream is over; stop reading even if the socket is open.
    Stop,
}

/// A provider's SSE state machine.
///
/// Implementations push zero or more items into `out` and must be tolerant of
/// unknown event types — providers add them without notice, and a hard parse
/// failure on an unrecognised event turns a working stream into an outage.
pub trait SseDecoder: Send {
    /// Handle one server-sent event.
    fn on_event(&mut self, ev: &Event, out: &mut Vec<Result<StreamEvent>>) -> Flow;

    /// Called once when the underlying byte stream ends without the decoder
    /// having produced a terminal event.
    ///
    /// The default synthesises `Done(EndTurn)`: several endpoints in the §10
    /// matrix simply close the connection, and every consumer downstream is
    /// written against "exactly one terminal event".
    fn on_end(&mut self, out: &mut Vec<Result<StreamEvent>>) {
        out.push(Ok(StreamEvent::Done(StopReason::EndTurn)));
    }
}

/// Drive `decoder` over `events`, honouring `cancel`.
///
/// Cancellation is not an afterthought (§7, §13): `esc` must abort in-flight
/// work immediately. On cancel the stream emits `Done(Cancelled)` and ends —
/// the partial text already delivered stays in the panel marked truncated and
/// is never auto-inserted. Dropping the returned stream drops the underlying
/// response, which is what actually closes the socket.
pub fn decode<S, D>(
    events: S,
    decoder: D,
    provider: ProviderId,
    cancel: CancellationToken,
) -> BoxStream<'static, Result<StreamEvent>>
where
    S: Stream<Item = std::result::Result<Event, EventStreamError<reqwest::Error>>> + Send + 'static,
    D: SseDecoder + 'static,
{
    struct State<S, D> {
        events: std::pin::Pin<Box<S>>,
        decoder: D,
        queue: std::collections::VecDeque<Result<StreamEvent>>,
        provider: ProviderId,
        cancel: CancellationToken,
        finished: bool,
    }

    let state = State {
        events: Box::pin(events),
        decoder,
        queue: std::collections::VecDeque::new(),
        provider,
        cancel,
        finished: false,
    };

    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.queue.pop_front() {
                return Some((item, st));
            }
            if st.finished {
                return None;
            }

            let mut out: Vec<Result<StreamEvent>> = Vec::new();
            tokio::select! {
                biased;

                () = st.cancel.cancelled() => {
                    st.finished = true;
                    out.push(Ok(StreamEvent::Done(StopReason::Cancelled)));
                }

                next = st.events.next() => match next {
                    Some(Ok(ev)) => {
                        if st.decoder.on_event(&ev, &mut out) == Flow::Stop {
                            st.finished = true;
                        }
                    }
                    Some(Err(err)) => {
                        st.finished = true;
                        out.push(Err(stream_error(&st.provider, err)));
                    }
                    None => {
                        st.finished = true;
                        st.decoder.on_end(&mut out);
                    }
                },
            }

            st.queue.extend(out);
        }
    })
    .boxed()
}

fn stream_error(provider: &ProviderId, err: EventStreamError<reqwest::Error>) -> AiboError {
    match err {
        EventStreamError::Transport(e) => crate::http::map_transport_error(provider, &e),
        // A malformed frame or invalid UTF-8 mid-stream. Not retryable at this
        // layer: the provider sent something aibo cannot parse.
        other => AiboError::Internal(Box::new(std::io::Error::other(other.to_string()))),
    }
}

/// Turn a `reqwest` response body into a stream of server-sent events.
pub fn events_from_response(
    response: reqwest::Response,
) -> impl Stream<Item = std::result::Result<Event, EventStreamError<reqwest::Error>>> + Send {
    response.bytes_stream().eventsource()
}

/// Replay a recorded SSE body as events, for golden-fixture tests.
///
/// Uses the same parser as the live path so a fixture cannot pass through a
/// more forgiving code path than production. The body is delivered in small
/// chunks on purpose: a decoder that only works when every frame arrives whole
/// is a decoder that breaks against a real socket.
pub fn events_from_bytes(
    body: impl AsRef<[u8]>,
) -> impl Stream<Item = std::result::Result<Event, EventStreamError<reqwest::Error>>> + Send {
    const CHUNK: usize = 17; // deliberately not a frame boundary
    let chunks: Vec<std::result::Result<Vec<u8>, reqwest::Error>> = body
        .as_ref()
        .chunks(CHUNK)
        .map(|c| Ok(c.to_vec()))
        .collect();
    futures_util::stream::iter(chunks).eventsource()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::types::StopReason;

    struct Echo;

    impl SseDecoder for Echo {
        fn on_event(&mut self, ev: &Event, out: &mut Vec<Result<StreamEvent>>) -> Flow {
            if ev.data == DONE_SENTINEL {
                out.push(Ok(StreamEvent::Done(StopReason::EndTurn)));
                return Flow::Stop;
            }
            out.push(Ok(StreamEvent::Text(ev.data.clone())));
            Flow::Continue
        }
    }

    #[tokio::test]
    async fn events_decode_in_order_and_terminate_on_the_sentinel() {
        let body = b"data: one\n\ndata: two\n\ndata: [DONE]\n\n".to_vec();
        let out: Vec<_> = decode(
            events_from_bytes(body),
            Echo,
            ProviderId::CEREBRAS,
            CancellationToken::new(),
        )
        .collect()
        .await;
        let out: Vec<StreamEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(
            out,
            vec![
                StreamEvent::Text("one".into()),
                StreamEvent::Text("two".into()),
                StreamEvent::Done(StopReason::EndTurn),
            ]
        );
    }

    #[tokio::test]
    async fn an_unterminated_stream_still_produces_exactly_one_done() {
        let body = b"data: one\n\n".to_vec();
        let out: Vec<_> = decode(
            events_from_bytes(body),
            Echo,
            ProviderId::CEREBRAS,
            CancellationToken::new(),
        )
        .collect()
        .await;
        let out: Vec<StreamEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], StreamEvent::Done(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn cancellation_ends_the_stream_with_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let body = b"data: one\n\ndata: two\n\n".to_vec();
        let out: Vec<_> = decode(events_from_bytes(body), Echo, ProviderId::CEREBRAS, cancel)
            .collect()
            .await;
        let out: Vec<StreamEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(out, vec![StreamEvent::Done(StopReason::Cancelled)]);
    }
}
