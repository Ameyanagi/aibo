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

/// Largest raw server-sent event accepted before parsing.
///
/// This includes field names, comments and payload data (but not line
/// terminators). Bounding the raw frame matters because the eventsource parser
/// otherwise has to retain an unterminated `data:` field indefinitely.
pub const MAX_SSE_EVENT_BYTES: usize = 1 << 20;

/// Largest non-streaming JSON response accepted from a provider.
pub const MAX_JSON_BODY_BYTES: usize = 256 << 10;

/// Largest prefix retained from a provider error response.
pub const MAX_ERROR_BODY_BYTES: usize = 16 << 10;

/// Most logical stream events one raw SSE frame may fan out into.
///
/// The raw frame itself is bounded, but a compatible endpoint can still place
/// a large `choices` array in one JSON object. Capping the decoded batch keeps
/// the per-stream queue finite and turns hostile amplification into a normal
/// provider error.
pub const MAX_DECODED_EVENTS_PER_SSE_EVENT: usize = 64;

/// Most tool calls a model may assemble in one response.
pub const MAX_TOOL_CALLS_PER_RESPONSE: usize = 32;

/// Most bytes retained while assembling fragmented tool-call fields.
pub const MAX_BUFFERED_TOOL_BYTES: usize = 1 << 20;

/// Failure while reading the byte stream beneath the SSE parser.
#[derive(Debug, thiserror::Error)]
pub enum SseReadError {
    /// The HTTP response body failed.
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    /// A server-sent event did not terminate within the configured bound.
    #[error("SSE event exceeded the {limit}-byte limit")]
    EventTooLarge {
        /// Configured raw-frame limit.
        limit: usize,
    },
}

/// Maps an SSE byte-stream transport failure into aibo's provider errors.
///
/// `decode` remains generic over this error so callers that already have an
/// `EventStreamError<reqwest::Error>` do not need to wrap it merely to use the
/// shared decoder.
pub trait SseTransportError: std::error::Error + Send + Sync + 'static {
    /// Convert the transport-layer failure for `provider`.
    fn into_aibo_error(self, provider: &ProviderId) -> AiboError;
}

impl SseTransportError for reqwest::Error {
    fn into_aibo_error(self, provider: &ProviderId) -> AiboError {
        crate::http::map_transport_error(provider, &self)
    }
}

impl SseTransportError for SseReadError {
    fn into_aibo_error(self, provider: &ProviderId) -> AiboError {
        match self {
            Self::Transport(error) => crate::http::map_transport_error(provider, &error),
            Self::EventTooLarge { limit } => AiboError::Internal(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("provider SSE event exceeded the {limit}-byte limit"),
            ))),
        }
    }
}

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
    /// The default rejects an unexpected EOF. Providers that are documented to
    /// use connection-close as their terminator override this explicitly; doing
    /// it by default would turn a truncated response into a successful one.
    fn on_end(&mut self, out: &mut Vec<Result<StreamEvent>>) {
        out.push(Err(unexpected_eof()));
    }
}

/// A one-item stream used when cancellation wins before an HTTP response has
/// been established.
pub fn cancelled_stream() -> BoxStream<'static, Result<StreamEvent>> {
    futures_util::stream::once(futures_util::future::ready(Ok(StreamEvent::Done(
        StopReason::Cancelled,
    ))))
    .boxed()
}

/// The common failure for a protocol whose required terminal marker never
/// arrived before the transport closed cleanly.
pub fn unexpected_eof() -> AiboError {
    AiboError::Internal(Box::new(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "provider stream closed before its terminal event",
    )))
}

/// Drive `decoder` over `events`, honouring `cancel`.
///
/// Cancellation is not an afterthought (§7, §13): `esc` must abort in-flight
/// work immediately. On cancel the stream emits `Done(Cancelled)` and ends —
/// the partial text already delivered stays in the panel marked truncated and
/// is never auto-inserted. Dropping the returned stream drops the underlying
/// response, which is what actually closes the socket.
pub fn decode<S, D, E>(
    events: S,
    decoder: D,
    provider: ProviderId,
    cancel: CancellationToken,
) -> BoxStream<'static, Result<StreamEvent>>
where
    S: Stream<Item = std::result::Result<Event, EventStreamError<E>>> + Send + 'static,
    D: SseDecoder + 'static,
    E: SseTransportError,
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

            if out.len() > MAX_DECODED_EVENTS_PER_SSE_EVENT {
                st.finished = true;
                out.clear();
                out.push(Err(AiboError::Internal(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "provider SSE frame decoded into more than \
                         {MAX_DECODED_EVENTS_PER_SSE_EVENT} events"
                    ),
                )))));
            }
            st.queue.extend(out);
        }
    })
    .boxed()
}

fn stream_error<E: SseTransportError>(
    provider: &ProviderId,
    err: EventStreamError<E>,
) -> AiboError {
    match err {
        EventStreamError::Transport(error) => error.into_aibo_error(provider),
        // A malformed frame or invalid UTF-8 mid-stream. Not retryable at this
        // layer: the provider sent something aibo cannot parse.
        other => AiboError::Internal(Box::new(std::io::Error::other(other.to_string()))),
    }
}

#[derive(Debug, Default)]
struct FrameLimit {
    bytes: usize,
    line_has_data: bool,
    previous_was_cr: bool,
}

impl FrameLimit {
    fn observe(&mut self, chunk: &[u8]) -> std::result::Result<(), SseReadError> {
        for &byte in chunk {
            match byte {
                b'\r' => {
                    self.finish_line();
                    self.previous_was_cr = true;
                }
                b'\n' if self.previous_was_cr => {
                    // CRLF is one line ending; the CR already finished it.
                    self.previous_was_cr = false;
                }
                b'\n' => self.finish_line(),
                _ => {
                    self.previous_was_cr = false;
                    self.line_has_data = true;
                    self.bytes = self.bytes.saturating_add(1);
                    if self.bytes > MAX_SSE_EVENT_BYTES {
                        return Err(SseReadError::EventTooLarge {
                            limit: MAX_SSE_EVENT_BYTES,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn finish_line(&mut self) {
        if self.line_has_data {
            self.line_has_data = false;
        } else {
            // An empty line dispatches the current SSE event.
            self.bytes = 0;
        }
        self.previous_was_cr = false;
    }
}

fn bounded_sse_bytes<S, B>(
    stream: S,
) -> impl Stream<Item = std::result::Result<B, SseReadError>> + Send
where
    S: Stream<Item = std::result::Result<B, SseReadError>> + Send,
    B: AsRef<[u8]> + Send,
{
    struct State {
        limit: FrameLimit,
        stopped: bool,
    }

    stream.scan(
        State {
            limit: FrameLimit::default(),
            stopped: false,
        },
        |state, item| {
            if state.stopped {
                return futures_util::future::ready(None);
            }
            let result = match item {
                Ok(chunk) => state.limit.observe(chunk.as_ref()).map(|()| chunk),
                Err(error) => Err(error),
            };
            state.stopped = result.is_err();
            futures_util::future::ready(Some(result))
        },
    )
}

/// Turn a `reqwest` response body into a stream of server-sent events.
pub fn events_from_response(
    response: reqwest::Response,
) -> impl Stream<Item = std::result::Result<Event, EventStreamError<SseReadError>>> + Send {
    let bytes = response
        .bytes_stream()
        .map(|item| item.map_err(SseReadError::Transport));
    bounded_sse_bytes(bytes).eventsource()
}

/// Replay a recorded SSE body as events, for golden-fixture tests.
///
/// Uses the same parser as the live path so a fixture cannot pass through a
/// more forgiving code path than production. The body is delivered in small
/// chunks on purpose: a decoder that only works when every frame arrives whole
/// is a decoder that breaks against a real socket.
pub fn events_from_bytes(
    body: impl AsRef<[u8]>,
) -> impl Stream<Item = std::result::Result<Event, EventStreamError<SseReadError>>> + Send {
    const CHUNK: usize = 17; // deliberately not a frame boundary
    let chunks: Vec<std::result::Result<Vec<u8>, SseReadError>> = body
        .as_ref()
        .chunks(CHUNK)
        .map(|c| Ok(c.to_vec()))
        .collect();
    bounded_sse_bytes(futures_util::stream::iter(chunks)).eventsource()
}

#[derive(Debug)]
struct BodyPrefix {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl BodyPrefix {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8 << 10)),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        let take = remaining.min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..take]);
        self.truncated |= take < chunk.len();
    }

    fn into_lossy_text(self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    fn into_json_text(self) -> Result<String> {
        if self.truncated {
            return Err(AiboError::Internal(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("provider response exceeded the {}-byte limit", self.limit),
            ))));
        }
        String::from_utf8(self.bytes).map_err(|error| AiboError::Internal(Box::new(error)))
    }
}

async fn read_body_prefix(
    mut response: reqwest::Response,
    provider: &ProviderId,
    limit: usize,
) -> Result<BodyPrefix> {
    let mut body = BodyPrefix::new(limit);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| crate::http::map_transport_error(provider, &error))?
    {
        body.push(&chunk);
        if body.truncated {
            // Dropping the response here closes the connection instead of
            // downloading bytes the caller has explicitly refused to retain.
            break;
        }
    }
    Ok(body)
}

/// Read a bounded JSON response body and reject truncation or invalid UTF-8.
pub async fn read_json_body(response: reqwest::Response, provider: &ProviderId) -> Result<String> {
    read_body_prefix(response, provider, MAX_JSON_BODY_BYTES)
        .await?
        .into_json_text()
}

/// Read a bounded, lossy prefix from an error response.
///
/// Error mapping needs the beginning of an envelope, but must retain the HTTP
/// status even when a hostile endpoint sends an arbitrarily large body.
pub async fn read_error_body(response: reqwest::Response, provider: &ProviderId) -> Result<String> {
    Ok(read_body_prefix(response, provider, MAX_ERROR_BODY_BYTES)
        .await?
        .into_lossy_text())
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

    struct FanOut;

    impl SseDecoder for FanOut {
        fn on_event(&mut self, _ev: &Event, out: &mut Vec<Result<StreamEvent>>) -> Flow {
            for _ in 0..=MAX_DECODED_EVENTS_PER_SSE_EVENT {
                out.push(Ok(StreamEvent::Text("x".into())));
            }
            Flow::Stop
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
    async fn an_unterminated_stream_is_not_mistaken_for_success() {
        let body = b"data: one\n\n".to_vec();
        let out: Vec<_> = decode(
            events_from_bytes(body),
            Echo,
            ProviderId::CEREBRAS,
            CancellationToken::new(),
        )
        .collect()
        .await;
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], Ok(StreamEvent::Text(ref text)) if text == "one"));
        assert!(out[1].is_err());
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

    #[tokio::test]
    async fn an_unterminated_oversized_event_is_rejected_before_decoding() {
        let body = format!("data: {}\n\n", "x".repeat(MAX_SSE_EVENT_BYTES + 1));
        let chunks = futures_util::stream::iter([Ok::<_, SseReadError>(body.into_bytes())]);
        let events = bounded_sse_bytes(chunks).eventsource();
        let out: Vec<_> = decode(events, Echo, ProviderId::CEREBRAS, CancellationToken::new())
            .collect()
            .await;

        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Err(AiboError::Internal(_))));
    }

    #[tokio::test]
    async fn one_sse_frame_cannot_amplify_into_an_unbounded_event_queue() {
        let out: Vec<_> = decode(
            events_from_bytes(b"data: hostile\n\n"),
            FanOut,
            ProviderId::CEREBRAS,
            CancellationToken::new(),
        )
        .collect()
        .await;

        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Err(AiboError::Internal(_))));
    }

    #[test]
    fn body_prefix_never_retains_more_than_its_limit() {
        let mut body = BodyPrefix::new(8);
        body.push(b"12345");
        body.push(b"67890");

        assert_eq!(body.bytes, b"12345678");
        assert!(body.truncated);
    }

    #[test]
    fn bounded_json_body_rejects_truncation() {
        let mut body = BodyPrefix::new(4);
        body.push(b"{\"too long\":true}");
        assert!(matches!(body.into_json_text(), Err(AiboError::Internal(_))));
    }
}
