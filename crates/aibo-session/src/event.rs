//! What goes into one panel invocation, and what comes back out.
//!
//! The engine is a stream transformer: a [`Submission`] in, a sequence of
//! [`SessionEvent`]s out, and an [`Outcome`] as the return value. Keeping the
//! outcome out of the event stream is deliberate — §13's invariant that *"a
//! partial stream is never auto-inserted"* is expressed as
//! [`Outcome::insertable_text`], and a caller cannot forget to check a return
//! value as easily as it can forget to match one more event variant.

use std::sync::Arc;

use aibo_core::AiboError;
use aibo_core::context::Chars;
use aibo_core::cost::Micros;
use aibo_core::prompts::attachable_clipboard_text;
use aibo_core::types::{
    AppInfo, Attachment, ClipboardItem, FieldContext, Health, ProviderId, Role, StopReason,
    StreamEvent, Surface, Usage,
};
use uuid::Uuid;

/// The §8 capture, as far as orchestration cares.
///
/// Every field is optional because §8 requires the panel to tolerate context
/// *"arriving late, empty, or never"* — an engine that needed a selection to be
/// present would turn a slow AX read into a failed request.
#[derive(Debug, Clone, Default)]
pub struct Capture {
    /// The frontmost app, when it could be identified.
    pub app: Option<AppInfo>,
    /// The focused text field. Prompt assembly drops it again if it turns out
    /// to be secure or mid-IME-composition (§5, §9).
    pub field: Option<FieldContext>,
    /// The selection being transformed or attached.
    pub selection: Option<String>,
    /// The clipboard, when it was consulted.
    pub clipboard: Option<ClipboardItem>,
}

/// Length of `s` in **characters**, for §13's large-selection cap.
///
/// §13 states the unit and then states the bug it is there to prevent:
/// *"refuse above 200k **characters — counted as characters, not `str::len()`
/// bytes.** A Japanese selection averages ~3 bytes per character, so a
/// byte-based cap refuses CJK users at ~66k characters and reports a nonsense
/// number in the error."*
///
/// So this is `chars().count()` and never `len()` — and the return type is
/// [`Chars`], not `usize`, so the compiler now enforces what this doc comment
/// used to only assert. It agrees with the buckets
/// [`aibo_core::context::estimate`] counts — `ascii_chars + cjk_chars +
/// other_chars` is exactly this number — which is why the cap and the §4 token
/// estimate two lines away in the engine can no longer disagree about what a
/// character is.
///
/// The unit is the Unicode scalar value, not the grapheme cluster: it is what
/// §13 asks for, it is what the token estimate counts, and it is the
/// conservative direction — a decomposed `é` or an emoji ZWJ sequence counts
/// for more than one, never less, so the cap can never be *under*-enforced.
///
/// ```
/// # use aibo_core::context::Chars;
/// # use aibo_session::event::char_len;
/// assert_eq!(char_len("hello"), Chars::new(5));
/// // 5 kana are 15 bytes and 5 characters.
/// assert_eq!(char_len("こんにちは"), Chars::new(5));
/// ```
pub fn char_len(s: &str) -> Chars {
    Chars::of(s)
}

impl Capture {
    /// Characters of captured payload, for §13's large-selection cap.
    ///
    /// Characters, not bytes — see [`char_len`].
    ///
    /// **Includes the clipboard**, on the same terms as §4's `payload_tokens`
    /// and for the same reason: a clipboard attachment is content aibo is about
    /// to put on the wire (§5 priority 4), so it counts against a cap whose job
    /// is to refuse before a request is built. Omitting it let a 200k-character
    /// clipboard through a cap that exists to stop exactly that.
    /// [`attachable_clipboard_text`] is the shared predicate, so what is
    /// measured here is precisely what prompt assembly will send — a concealed
    /// password-manager item counts for nothing because it is never attached.
    pub fn payload_chars(&self) -> Chars {
        self.selection.as_deref().map_or(Chars::ZERO, char_len)
            + self
                .field
                .as_ref()
                .map_or(Chars::ZERO, |f| char_len(&f.prefix) + char_len(&f.suffix))
            + self.clipboard_chars()
    }

    /// Characters of the clipboard attachment that will actually be sent.
    fn clipboard_chars(&self) -> Chars {
        self.clipboard
            .as_ref()
            .and_then(attachable_clipboard_text)
            .map_or(Chars::ZERO, char_len)
    }
}

/// One request from the panel.
#[derive(Debug, Clone)]
pub struct Submission {
    /// The panel invocation this belongs to (§13 "one panel, one session").
    pub session: Uuid,
    /// Exactly what the user typed. §5: the only content permitted to
    /// authorise a tool call.
    pub instruction: String,
    /// The surface, when the panel has already frozen one (§1). `None` asks
    /// the engine to infer it from the capture.
    pub surface: Option<Surface>,
    /// `@model` / `⌘1..4`; wins over every routing rule (§4 rule 1).
    pub role_override: Option<Role>,
    /// What §8 managed to read.
    pub capture: Capture,
    /// What the user **deliberately attached** (§2 modalities).
    ///
    /// # This field, and not [`Capture`], decides §4 rule 2
    ///
    /// [`crate::engine`]'s `has_image` is
    /// `attachments.iter().any(Attachment::is_image)` and nothing else. It is
    /// separate from `capture` because `capture` is *ambient* — whatever
    /// happened to be on screen and on the pasteboard when the hotkey fired —
    /// and ambient state must never make a routing decision. Deriving
    /// `has_image` from the clipboard meant taking any screenshot silently
    /// rerouted every later request to [`Role::Vision`], which nothing binds,
    /// and the user saw "No provider is configured yet" beside a signed-in,
    /// healthy provider.
    ///
    /// A clipboard image is still context; it reaches the prompt through
    /// `capture` at §5's priority 4. Putting it *here* requires a gesture.
    ///
    /// # Untrusted (§5)
    ///
    /// Attacker-controlled on exactly the terms a selection is, and harder to
    /// filter — see [`aibo_core::types::AttachmentSource::origin`]. Nothing
    /// derived from one may authorise a tool call.
    pub attachments: Vec<Attachment>,
    /// Conversation to append to, for Ask.
    pub conversation_id: Option<Uuid>,
    /// Prior turns, oldest first. The §5 budget drops whole turns from the
    /// oldest end if they do not fit.
    pub history: Vec<aibo_core::context::Turn>,
}

impl Submission {
    /// A submission with nothing captured — the "typed into an empty panel"
    /// case, and what most tests want.
    pub fn new(session: Uuid, instruction: impl Into<String>) -> Self {
        Self {
            session,
            instruction: instruction.into(),
            surface: None,
            role_override: None,
            capture: Capture::default(),
            attachments: Vec::new(),
            conversation_id: None,
            history: Vec::new(),
        }
    }

    /// Attach an item — the deliberate act, in builder form.
    #[must_use]
    pub fn with_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Whether the user attached an image. **The only** source of §4 rule 2's
    /// `has_image`.
    pub fn has_image_attachment(&self) -> bool {
        self.attachments.iter().any(Attachment::is_image)
    }

    /// Summed raw bytes of every attachment.
    ///
    /// A second unit alongside [`Submission::total_chars`], and deliberately
    /// **not** added to it. Bytes and characters are different units; summing
    /// them is the exact bug class [`Chars`] exists to end, and 3 MB of PNG is
    /// not "3 million characters" of anything. §13's payload cap is enforced in
    /// characters over the text and in bytes over the attachments — see
    /// [`aibo_core::types::validate_attachments`], which owns the byte ceiling.
    pub fn attachment_bytes(&self) -> usize {
        self.attachments
            .iter()
            .map(Attachment::byte_len)
            .fold(0usize, usize::saturating_add)
    }

    /// Everything §13's large-selection cap measures, in **characters**.
    ///
    /// The capture plus the typed instruction. It lives here rather than at the
    /// call site because the previous arrangement — the engine adding
    /// `instruction.len()` to [`Capture::payload_chars`] — is precisely how the
    /// cap came to be measured in bytes on one half of the sum: two places had
    /// to agree on the unit and one of them was wrong. Now there is one place,
    /// and [`Chars`] means the two halves cannot be in different units even if
    /// there were two.
    pub fn total_chars(&self) -> Chars {
        self.capture.payload_chars() + char_len(&self.instruction)
    }
}

/// Why a chain entry was passed over, for the log and the §14 "visible when
/// they fire" requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The provider is not in the registry — configured once, removed since.
    NotConfigured,
    /// §13: degraded, and the probe backoff has not expired.
    Degraded,
    /// §13: the health probe ran and failed. §4 lists this explicitly as a
    /// fallback trigger.
    FailedHealthProbe,
    /// §14: falling here would move the user's text from a provider they
    /// administer to one they do not, and consent was not given.
    TrustBoundary,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotConfigured => "not configured",
            Self::Degraded => "degraded",
            Self::FailedHealthProbe => "failed health probe",
            Self::TrustBoundary => "would cross a trust boundary without consent",
        })
    }
}

/// Everything the engine tells its caller while a request is in flight.
///
/// Maps onto `aibo_ui::UiEvent` almost one to one; the binary owns that
/// translation so `aibo-session` stays free of any UI dependency.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SessionEvent {
    /// §4 picked a role. Emitted before any network call, so the log shows the
    /// routing decision even for a request that never dispatches.
    Routed {
        /// The surface in force, inferred or supplied.
        surface: Surface,
        /// The role chosen.
        role: Role,
        /// Name of the rule that chose it.
        rule: &'static str,
    },

    /// A chain entry was passed over (§4, §13, §14).
    Skipped {
        /// The provider that was skipped.
        provider: ProviderId,
        /// Why.
        reason: SkipReason,
    },

    /// The request went out. §14 requires fallback to be *visible*, which is
    /// what `substituted_for` carries.
    Dispatched {
        /// Provider that took it.
        provider: ProviderId,
        /// Wire model id.
        model: String,
        /// Set when this is not the chain's primary entry: the provider the
        /// user would have expected. §13 renders it as a subtle footnote.
        substituted_for: Option<ProviderId>,
    },

    /// First token latency, measured from [`crate::Engine::run`] entry (§15).
    FirstToken {
        /// Milliseconds.
        elapsed_ms: u64,
    },

    /// A provider stream event, forwarded verbatim (§7).
    Stream(Box<StreamEvent>),

    /// A provider's health changed (§13). Emitted once per transition.
    ProviderHealth {
        /// The provider.
        provider: ProviderId,
        /// Its new health.
        health: Health,
    },

    /// §14's reconciled spend, once the real `Usage` has landed.
    Cost {
        /// Reported token accounting.
        usage: Usage,
        /// Reconciled cost, `None` when the model is unpriced.
        cost_micros: Option<Micros>,
        /// Settled plus still-reserved, i.e. what the user is on the hook for.
        committed_micros: Micros,
    },

    /// The request failed. The caller maps the error to its §13 treatment.
    Failed(Arc<AiboError>),
}

/// Default number of session events allowed to wait for the UI event pump.
///
/// Provider chunks are untrusted-volume input. A finite queue makes a slow UI
/// apply backpressure at the stream boundary instead of turning token cadence
/// into unbounded heap growth.
pub const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Where [`SessionEvent`]s go.
///
/// Provider-driven [`SessionEvent::Stream`] events use
/// [`EventSink::emit_stream`], which waits for capacity while also observing
/// cancellation. Low-volume lifecycle events use [`EventSink::emit`], a
/// non-blocking best-effort send: a full or closed queue means the panel is not
/// keeping up, and accounting/persistence must still finish without waiting on
/// UI work.
#[derive(Debug, Clone)]
pub struct EventSink {
    tx: tokio::sync::mpsc::Sender<SessionEvent>,
}

impl EventSink {
    /// Wrap an existing sender.
    pub fn new(tx: tokio::sync::mpsc::Sender<SessionEvent>) -> Self {
        Self { tx }
    }

    /// A sink and its receiver. What tests use.
    pub fn channel() -> (Self, tokio::sync::mpsc::Receiver<SessionEvent>) {
        Self::channel_with_capacity(DEFAULT_EVENT_CHANNEL_CAPACITY)
    }

    /// A sink and receiver with an explicit positive queue capacity.
    ///
    /// Small capacities are useful for embedding and for deterministic
    /// backpressure tests. Tokio rejects zero-capacity channels.
    pub fn channel_with_capacity(
        capacity: usize,
    ) -> (Self, tokio::sync::mpsc::Receiver<SessionEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        (Self::new(tx), rx)
    }

    /// A sink whose events go nowhere.
    pub fn null() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        // Keeping the receiver alive would leak; dropping it makes every send a
        // no-op, which is exactly the intent.
        drop(rx);
        Self::new(tx)
    }

    /// Emit one low-volume lifecycle event without waiting on the UI.
    ///
    /// Returns `false` if the bounded queue is full or its receiver has closed.
    /// Either case is deliberately non-fatal: the engine must still reconcile
    /// spend and persist the exchange.
    pub(crate) fn emit(&self, event: SessionEvent) -> bool {
        self.tx.try_send(event).is_ok()
    }

    /// Emit provider-driven output with bounded, cancellation-aware
    /// backpressure.
    ///
    /// A closed receiver returns immediately so a vanished panel cannot stall
    /// the request's accounting tail. Cancellation wins over queue capacity so
    /// a slow-but-live UI cannot make `esc` hang behind a full channel.
    pub(crate) async fn emit_stream(
        &self,
        event: SessionEvent,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> bool {
        tokio::select! {
            biased;
            () = cancel.cancelled() => false,
            result = self.tx.send(event) => result.is_ok(),
        }
    }
}

impl From<tokio::sync::mpsc::Sender<SessionEvent>> for EventSink {
    fn from(tx: tokio::sync::mpsc::Sender<SessionEvent>) -> Self {
        Self::new(tx)
    }
}

/// Why a stream stopped short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartialReason {
    /// `esc`, or a newer submission superseded this one (§13).
    Cancelled,
    /// The stream failed after tokens had already arrived. §13 forbids
    /// retrying this — a retry risks double-billing and duplicated output.
    StreamFailed,
    /// The provider ended the turn with a non-terminal stop reason.
    StoppedEarly(StopReason),
}

/// How one submission ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// A complete answer.
    Completed(Box<Completion>),

    /// **Text arrived, but the stream did not finish.**
    ///
    /// §13, stated as an invariant because three other decisions depend on it:
    /// *"A partial stream is never auto-inserted. If a stream fails mid-way,
    /// the partial text stays in the panel marked truncated, with retry and
    /// copy actions. Silent insertion of half a rewrite over a user's selection
    /// is the worst failure this product can have."*
    ///
    /// [`Outcome::insertable_text`] returns `None` for this variant. That is
    /// the invariant in the type system rather than in a comment.
    Partial {
        /// What arrived. Show it, marked truncated. Never insert it.
        text: String,
        /// Why it stopped.
        reason: PartialReason,
        /// Provider that served it, when one was reached.
        provider: Option<ProviderId>,
    },

    /// Nothing usable was produced.
    Failed(Arc<AiboError>),
}

/// A finished answer.
#[derive(Debug, Clone)]
pub struct Completion {
    /// The assistant's text, after the §5 anti-preamble and prefix-repetition
    /// filter.
    pub text: String,
    /// The text exactly as the model produced it, before the filter. Kept so
    /// quality drift is observable rather than invisible (§5).
    pub raw_text: String,
    /// Which §5 filter patterns fired, if any.
    pub filtered: Vec<aibo_core::prompts::PreamblePattern>,
    /// Why the model stopped.
    pub stop: StopReason,
    /// Provider that served it.
    pub provider: ProviderId,
    /// Wire model id.
    pub model: String,
    /// Reported usage; all zeroes when the provider sent none.
    pub usage: Usage,
    /// Reconciled cost, `None` when unpriced.
    pub cost_micros: Option<Micros>,
    /// Dispatch to last event, in milliseconds.
    pub latency_ms: u64,
    /// §4: offer `⌘↩` escalation to `Smart`. True only on the two objective
    /// signals — a `length` stop, or fewer than ten output tokens.
    pub offer_escalation: bool,
    /// The conversation the exchange was written to (§12), when history is on.
    pub conversation_id: Option<Uuid>,
}

impl Outcome {
    /// The text aibo is allowed to write into someone else's app.
    ///
    /// `Some` **only** for [`Outcome::Completed`]. This is §13's partial-stream
    /// invariant, and it is the only function the insert path may consult.
    pub fn insertable_text(&self) -> Option<&str> {
        match self {
            Self::Completed(c) => Some(&c.text),
            Self::Partial { .. } | Self::Failed(_) => None,
        }
    }

    /// Whatever text exists, insertable or not, for the panel and the clipboard.
    pub fn displayable_text(&self) -> Option<&str> {
        match self {
            Self::Completed(c) => Some(&c.text),
            Self::Partial { text, .. } => Some(text),
            Self::Failed(_) => None,
        }
    }

    /// The error, when the outcome carries one.
    pub fn error(&self) -> Option<&Arc<AiboError>> {
        match self {
            Self::Failed(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::types::{ClipboardKind, FieldContext};
    use tokio_util::sync::CancellationToken;

    /// 100 kana. 300 bytes, 100 characters — the three-to-one ratio §13 names.
    fn japanese(chars: usize) -> String {
        "あ".repeat(chars)
    }

    fn field(prefix: &str, suffix: &str) -> FieldContext {
        FieldContext {
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
            caret: None,
            label: None,
            is_secure: false,
            ime_active: false,
            truncated: false,
            caret_bounds: None,
        }
    }

    fn clipboard(text: &str) -> ClipboardItem {
        ClipboardItem {
            kind: ClipboardKind::Text,
            text: Some(text.to_owned()),
            files: Vec::new(),
            concealed: false,
            transient: false,
            source_app: Some("Safari".into()),
            sequence: 1,
            restorable: true,
        }
    }

    #[tokio::test]
    async fn provider_events_backpressure_until_capacity_is_available() {
        let (sink, mut rx) = EventSink::channel_with_capacity(1);
        assert!(sink.emit(SessionEvent::Stream(Box::new(StreamEvent::Text(
            "queued".into(),
        )))));

        let cancel = CancellationToken::new();
        let blocked = {
            let sink = sink.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                sink.emit_stream(
                    SessionEvent::Stream(Box::new(StreamEvent::Text("next".into()))),
                    &cancel,
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            !blocked.is_finished(),
            "a full channel must push back on provider output"
        );

        assert!(rx.recv().await.is_some());
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
                .await
                .expect("capacity release must wake the producer")
                .unwrap()
        );
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn cancellation_and_receiver_closure_release_a_blocked_provider() {
        let (sink, mut rx) = EventSink::channel_with_capacity(1);
        assert!(sink.emit(SessionEvent::Stream(Box::new(StreamEvent::Text(
            "queued".into(),
        )))));

        let cancel = CancellationToken::new();
        let blocked = {
            let sink = sink.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                sink.emit_stream(
                    SessionEvent::Stream(Box::new(StreamEvent::Text("blocked".into()))),
                    &cancel,
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        cancel.cancel();
        assert!(
            !tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
                .await
                .expect("cancellation must not wait for UI capacity")
                .unwrap()
        );

        // Closing a receiver is normal when a panel disappears. It must be as
        // prompt as cancellation and must not require draining queued events.
        rx.close();
        assert!(
            !tokio::time::timeout(
                std::time::Duration::from_secs(1),
                sink.emit_stream(
                    SessionEvent::Stream(Box::new(StreamEvent::Text("closed".into()))),
                    &CancellationToken::new(),
                ),
            )
            .await
            .expect("receiver closure must release the producer")
        );
    }

    #[test]
    fn char_len_counts_characters_not_bytes() {
        // §13: "counted as characters, not `str::len()` bytes".
        let jp = japanese(100);
        assert_eq!(jp.len(), 300, "the premise: kana are three bytes each");
        assert_eq!(char_len(&jp), Chars::new(100));
    }

    #[test]
    fn char_len_counts_emoji_and_combining_marks_as_scalar_values() {
        // A non-BMP emoji is four bytes and one scalar value.
        assert_eq!("🎉".len(), 4);
        assert_eq!(char_len("🎉"), Chars::new(1));

        // A ZWJ sequence is one glyph and several scalar values. Counting the
        // scalar values over-counts relative to what the user sees, which is
        // the safe direction: the cap can be over-enforced, never under.
        assert_eq!(char_len("👩‍💻"), Chars::new(3));

        // A decomposed combining mark: one grapheme, two scalar values, three
        // bytes. The byte count is the only one of the three that is wrong.
        let combining = "e\u{0301}";
        assert_eq!(combining.len(), 3);
        assert_eq!(char_len(combining), Chars::new(2));
    }

    #[test]
    fn payload_chars_measures_every_captured_field_in_characters() {
        let capture = Capture {
            selection: Some(japanese(40)),
            field: Some(field(&japanese(30), &japanese(30))),
            ..Capture::default()
        };
        // 300 bytes each; 100 characters in total, not 300.
        assert_eq!(capture.payload_chars(), Chars::new(100));
    }

    /// §4 defines the payload as *"selection + clipboard + field prefix"*, and
    /// §5 gives the clipboard its own budget priority — it is content aibo
    /// sends. Leaving it out of the §13 cap let an arbitrarily large clipboard
    /// attachment past a limit whose whole purpose is to refuse before a
    /// request is built.
    #[test]
    fn payload_chars_counts_the_clipboard_attachment() {
        let mut capture = Capture {
            selection: Some(japanese(40)),
            ..Capture::default()
        };
        assert_eq!(capture.payload_chars(), Chars::new(40));

        capture.clipboard = Some(clipboard(&japanese(60)));
        assert_eq!(capture.payload_chars(), Chars::new(100));
    }

    /// …but only the clipboard that will actually be attached. §12: a
    /// concealed item is never sent, so it must not be able to trip a cap on
    /// what is sent.
    #[test]
    fn payload_chars_ignores_a_clipboard_that_will_never_be_sent() {
        let mut capture = Capture::default();

        let mut concealed = clipboard(&japanese(500));
        concealed.concealed = true;
        capture.clipboard = Some(concealed);
        assert_eq!(capture.payload_chars(), Chars::ZERO);

        let mut image = clipboard(&japanese(500));
        image.kind = ClipboardKind::ImageRef;
        capture.clipboard = Some(image);
        assert_eq!(capture.payload_chars(), Chars::ZERO);
    }

    #[test]
    fn total_chars_measures_the_instruction_in_the_same_unit() {
        // The instruction used to be added with `str::len()` while the capture
        // used another unit; the sum was meaningless for any non-ASCII input.
        let mut submission = Submission::new(Uuid::now_v7(), japanese(10));
        submission.capture.selection = Some(japanese(90));
        assert_eq!(submission.total_chars(), Chars::new(100));
    }

    /// Bytes and characters are different units, and §13's cap is stated in
    /// characters. Laundering 3 MB of PNG into the character count would report
    /// "3,000,000 characters" for an image that has none — the exact class of
    /// confusion [`Chars`] exists to end.
    #[test]
    fn attachment_bytes_are_counted_separately_from_characters() {
        let s = Submission::new(Uuid::now_v7(), japanese(10)).with_attachment(
            aibo_core::types::Attachment::image(
                aibo_core::types::AttachmentSource::Clipboard,
                vec![0u8; 3_000_000],
                "image/png",
                1568,
                882,
                "Screenshot",
            ),
        );
        assert_eq!(s.total_chars(), Chars::new(10));
        assert_eq!(s.attachment_bytes(), 3_000_000);
        assert!(s.has_image_attachment());
    }

    #[test]
    fn a_submission_with_nothing_attached_has_no_image() {
        // Including one whose clipboard holds an image: that is ambient state,
        // and ambient state is never an attachment.
        let mut s = Submission::new(Uuid::now_v7(), "hello");
        let mut item = clipboard("");
        item.kind = ClipboardKind::ImageRef;
        s.capture.clipboard = Some(item);
        assert!(!s.has_image_attachment());
        assert_eq!(s.attachment_bytes(), 0);
    }

    #[test]
    fn a_japanese_selection_at_the_shipped_cap_is_not_refused() {
        // The regression, stated against the shipped default rather than a
        // convenient small number: 200_000 kana is 600_000 bytes, and the
        // byte-based count refused it at ~66_667 characters.
        let cap = Chars::new(crate::engine::DEFAULT_MAX_PAYLOAD_CHARS);
        let mut submission = Submission::new(Uuid::now_v7(), String::new());
        submission.capture.selection = Some(japanese(cap.get()));
        assert_eq!(submission.total_chars(), cap);
        assert!(submission.total_chars() <= cap);
    }
}
