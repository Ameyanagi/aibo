//! Model routing (§4) and surface inference (§1) — pure, no I/O.
//!
//! Two independent decisions live here, in the order the product makes them:
//!
//! 1. [`infer_surface`] resolves one hotkey into one [`Surface`] (§1). It runs
//!    **once**, when context capture settles, and the answer is then frozen for
//!    the session. If capture times out the answer is [`Surface::Ask`] — never a
//!    guess that changes under the user.
//! 2. [`Router::route`] resolves a [`RouteInput`] into a [`Role`] (§4). The user
//!    still sees "auto"; there is no classifier call in the hot path.
//!
//! **Why a rule list and not an `if`-chain.** §4 is explicit: the eight v1 rules
//! are correct, but per-app defaults ("Anthropic in VS Code"), saved actions
//! with a pinned model (§12 `actions`), "local model when offline" and "cheap
//! model after ¥3000 this month" all arrive within weeks. Each of those turns a
//! hardcoded function into a rules engine with precedence. An ordered
//! `Vec<Box<dyn Rule>>` evaluated first-match-wins absorbs every one of them via
//! [`Router::prepend`]/[`Router::insert`], keeps the built-ins as the default
//! seed, and stays exhaustively testable. The tests at the bottom of this file
//! brute-force the whole input space against a literal transcription of the
//! plan's table.

use crate::types::{Role, RouteInput, Surface, Verb};

// ---------------------------------------------------------------------------
// Thresholds — the plan's table, as named constants
// ---------------------------------------------------------------------------

/// Rule 5: a Transform payload at or below this many estimated tokens is small
/// enough for `Fast` (§4).
///
/// Counted with [`estimate_tokens`], **not** `bytes / 4` — see that function.
pub const TRANSFORM_FAST_MAX_PAYLOAD_TOKENS: usize = 400;

/// Rule 7: an Ask prompt at or below this many estimated tokens is short enough
/// for `Fast`, provided the verb is also in [`ASK_FAST_VERBS`] (§4).
pub const ASK_FAST_MAX_PROMPT_TOKENS: usize = 60;

/// Rule 7: the lookup-shaped verbs that keep a short Ask on `Fast` (§4).
///
/// Deliberately narrow. `Explain` and `Summarise` are *not* here: they read a
/// payload and produce judgement, which is what `Smart` is for.
pub const ASK_FAST_VERBS: [Verb; 4] = [Verb::Define, Verb::Translate, Verb::Spell, Verb::Convert];

/// The role used when **no** rule matches.
///
/// The built-in list is exhaustive over [`Surface`], so this is unreachable for
/// [`Router::with_defaults`] — a test proves it. It only becomes reachable once
/// a caller builds a [`Router`] from a partial custom rule list, and biasing
/// that toward quality rather than latency is the safe failure direction.
pub const FALLBACK_ROLE: Role = Role::Smart;

// ---------------------------------------------------------------------------
// The rule list
// ---------------------------------------------------------------------------

/// One routing rule: a pure predicate over [`RouteInput`] that either claims the
/// request for a [`Role`] or declines and lets the next rule look.
///
/// Rules must be side-effect free and cheap — the whole list is evaluated inside
/// the surface's first-token budget (250 ms for Complete, §1).
///
/// [`name`](Rule::name) returns `&'static str` on purpose: it is reported in
/// [`Routed::rule`] for logging and for the "why this model?" affordance, and
/// keeping it static means routing allocates nothing. Config-driven rules should
/// name their *kind* (`"per_app_default"`, `"saved_action"`, `"budget_cap"`),
/// not their instance.
pub trait Rule: std::fmt::Debug + Send + Sync {
    /// Stable identifier for this rule, reported in [`Routed::rule`].
    fn name(&self) -> &'static str;

    /// `Some(role)` claims the request; `None` defers to the next rule.
    fn evaluate(&self, input: &RouteInput) -> Option<Role>;
}

/// The eight v1 rules from §4's table, in table order.
///
/// Each variant is one row. They are one `enum` rather than eight unit structs
/// so that [`BuiltinRule::ALL`] can be a `const` array and tests can name a row
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinRule {
    /// 1 — `user_override.is_some()` → that role. `@model` / `⌘1..4` wins over
    /// everything, including `has_image`; an explicit choice is never overruled.
    UserOverride,
    /// 2 — `has_image` → `Vision`. Above the surface rules because no other role
    /// can accept the input at all.
    Image,
    /// 3 — `surface == Do` → `Agent`.
    DoIsAgent,
    /// 4 — `surface == Complete` → `Fast`. Complete has the tightest budget in
    /// the product and never escalates implicitly.
    CompleteIsFast,
    /// 5 — `surface == Transform && payload_tokens <= 400 && !has_code` → `Fast`.
    ShortProseTransformIsFast,
    /// 6 — `surface == Transform` → `Smart`. Long, or code-bearing.
    TransformIsSmart,
    /// 7 — `surface == Ask && prompt_tokens <= 60 && verb ∈ {Define, Translate,
    /// Spell, Convert}` → `Fast`.
    ShortLookupAskIsFast,
    /// 8 — `surface == Ask` → `Smart`.
    AskIsSmart,
}

impl BuiltinRule {
    /// The v1 rules in evaluation order. The order **is** the semantics.
    pub const ALL: [BuiltinRule; 8] = [
        BuiltinRule::UserOverride,
        BuiltinRule::Image,
        BuiltinRule::DoIsAgent,
        BuiltinRule::CompleteIsFast,
        BuiltinRule::ShortProseTransformIsFast,
        BuiltinRule::TransformIsSmart,
        BuiltinRule::ShortLookupAskIsFast,
        BuiltinRule::AskIsSmart,
    ];
}

impl Rule for BuiltinRule {
    fn name(&self) -> &'static str {
        match self {
            BuiltinRule::UserOverride => "user_override",
            BuiltinRule::Image => "image",
            BuiltinRule::DoIsAgent => "do_is_agent",
            BuiltinRule::CompleteIsFast => "complete_is_fast",
            BuiltinRule::ShortProseTransformIsFast => "short_prose_transform_is_fast",
            BuiltinRule::TransformIsSmart => "transform_is_smart",
            BuiltinRule::ShortLookupAskIsFast => "short_lookup_ask_is_fast",
            BuiltinRule::AskIsSmart => "ask_is_smart",
        }
    }

    fn evaluate(&self, input: &RouteInput) -> Option<Role> {
        match self {
            BuiltinRule::UserOverride => input.user_override,

            BuiltinRule::Image => input.has_image.then_some(Role::Vision),

            BuiltinRule::DoIsAgent => (input.surface == Surface::Do).then_some(Role::Agent),

            BuiltinRule::CompleteIsFast => {
                (input.surface == Surface::Complete).then_some(Role::Fast)
            }

            BuiltinRule::ShortProseTransformIsFast => (input.surface == Surface::Transform
                && input.payload_tokens <= TRANSFORM_FAST_MAX_PAYLOAD_TOKENS
                && !input.has_code)
                .then_some(Role::Fast),

            BuiltinRule::TransformIsSmart => {
                (input.surface == Surface::Transform).then_some(Role::Smart)
            }

            BuiltinRule::ShortLookupAskIsFast => (input.surface == Surface::Ask
                && input.prompt_tokens <= ASK_FAST_MAX_PROMPT_TOKENS
                && input
                    .verb
                    .is_some_and(|verb| ASK_FAST_VERBS.contains(&verb)))
            .then_some(Role::Fast),

            BuiltinRule::AskIsSmart => (input.surface == Surface::Ask).then_some(Role::Smart),
        }
    }
}

/// A routing decision, with the rule that produced it.
///
/// The provenance is not decoration: "auto" is only acceptable to a user if aibo
/// can answer "why this model?", and a support report that says `smart` is far
/// less useful than one that says `smart via transform_is_smart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routed {
    /// The role to resolve against a provider chain.
    pub role: Role,
    /// [`Rule::name`] of the rule that matched, or `"fallback"` if none did.
    pub rule: &'static str,
    /// Index of the matching rule in the router's list; `None` on fallback.
    pub index: Option<usize>,
}

impl Routed {
    /// The decision taken when no rule matched: [`FALLBACK_ROLE`].
    const FALLBACK: Routed = Routed {
        role: FALLBACK_ROLE,
        rule: "fallback",
        index: None,
    };
}

/// An ordered, first-match-wins rule list (§4).
///
/// Build the shipped configuration with [`Router::with_defaults`], then layer
/// user rules on top with [`Router::prepend`] (higher precedence than the
/// built-ins, which is what a per-app default or a pinned saved action needs) or
/// [`Router::push`] (lower — a catch-all).
///
/// ```
/// use aibo_core::router::Router;
/// use aibo_core::types::{Role, RouteInput, Surface};
///
/// let router = Router::with_defaults();
/// let input = RouteInput {
///     surface: Surface::Complete,
///     prompt_tokens: 0,
///     payload_tokens: 120,
///     has_code: false,
///     has_image: false,
///     verb: None,
///     user_override: None,
/// };
/// assert_eq!(router.route(&input).role, Role::Fast);
/// ```
#[derive(Debug)]
pub struct Router {
    rules: Vec<Box<dyn Rule>>,
}

impl Router {
    /// The rules shipped in v1: §4's table, in order.
    pub fn with_defaults() -> Self {
        Self {
            rules: BuiltinRule::ALL
                .iter()
                .map(|rule| Box::new(*rule) as Box<dyn Rule>)
                .collect(),
        }
    }

    /// A router with no rules at all. Every input takes [`FALLBACK_ROLE`] until
    /// rules are added. Useful for tests and for a fully user-defined list.
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// A router over exactly these rules, in this order.
    pub fn from_rules(rules: Vec<Box<dyn Rule>>) -> Self {
        Self { rules }
    }

    /// Append a rule at the lowest precedence.
    pub fn push(&mut self, rule: Box<dyn Rule>) -> &mut Self {
        self.rules.push(rule);
        self
    }

    /// Insert a rule at the **highest** precedence, above the built-ins.
    ///
    /// This is the hook §4 asks for: per-app defaults, saved actions with a
    /// pinned role, offline and budget rules all need to win over the table.
    pub fn prepend(&mut self, rule: Box<dyn Rule>) -> &mut Self {
        self.rules.insert(0, rule);
        self
    }

    /// Insert a rule at `index`, shifting later rules down.
    ///
    /// # Panics
    ///
    /// Panics if `index` is greater than [`Router::len`].
    pub fn insert(&mut self, index: usize, rule: Box<dyn Rule>) -> &mut Self {
        self.rules.insert(index, rule);
        self
    }

    /// Number of rules in the list.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether the list is empty (every input would take [`FALLBACK_ROLE`]).
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rule names in evaluation order — for diagnostics and the settings UI.
    pub fn rule_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.rules.iter().map(|rule| rule.name())
    }

    /// Evaluate the list top to bottom and return the first match (§4).
    ///
    /// Pure and allocation-free. Always returns a decision: if nothing matches,
    /// [`FALLBACK_ROLE`]. Routing never fails, because there is nothing useful a
    /// caller could do with a routing error inside a 250 ms budget.
    pub fn route(&self, input: &RouteInput) -> Routed {
        for (index, rule) in self.rules.iter().enumerate() {
            if let Some(role) = rule.evaluate(input) {
                return Routed {
                    role,
                    rule: rule.name(),
                    index: Some(index),
                };
            }
        }
        Routed::FALLBACK
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Escalation is explicit, never automatic (§4). `⌘↩` re-runs the same input at
/// [`Role::Smart`] and shows both answers; there is no silent de-escalation and
/// no silent double-spend.
///
/// This returns the role a manual escalation resolves to, given what already
/// ran. Escalating something that already ran on `Smart` is a no-op rather than
/// an error — the UI should not have offered it.
pub const fn escalated(from: Role) -> Role {
    match from {
        Role::Fast | Role::Cheap | Role::Smart => Role::Smart,
        // Escalating these to Smart would drop the capability that selected the
        // role in the first place.
        Role::Vision => Role::Vision,
        Role::Agent => Role::Agent,
    }
}

/// Whether aibo should proactively *offer* escalation after a `Fast` answer.
///
/// §4 allows exactly two signals, both cheap and objective: the answer hit a
/// `length` stop reason, or it came back under 10 tokens. Anything softer is a
/// quality judgement aibo is not entitled to make with the user's money.
pub fn should_offer_escalation(role: Role, stopped_on_length: bool, output_tokens: usize) -> bool {
    role == Role::Fast && (stopped_on_length || output_tokens < 10)
}

// ---------------------------------------------------------------------------
// Token estimation — §4's CJK correction
// ---------------------------------------------------------------------------

/// Estimate tokens for routing, as `non_cjk_chars / 4 + cjk_chars` (§4).
///
/// **This is deliberately not `bytes / 4`.** `bytes / 4` is calibrated on
/// English. A Japanese character is three UTF-8 bytes but is worth roughly one
/// token or more, so `bytes / 4` under-counts CJK by about a factor of three and
/// every threshold in §4's table mis-routes Japanese: a long Japanese selection
/// scores under [`TRANSFORM_FAST_MAX_PAYLOAD_TOKENS`] and lands on `Fast` when
/// the equivalent English would have gone to `Smart`. `tests::cjk_misrouting_hazard`
/// pins that.
///
/// It stays a heuristic on purpose — a real tokenizer never runs on the hot
/// path. The estimate only has to be right enough to pick a role, and §4 tunes
/// the thresholds against real Japanese and English samples in P3.
///
/// Scripts that are neither ASCII nor CJK (Cyrillic, Devanagari, emoji) fall in
/// the `/4` bucket, which under-counts them too; §4 specifies only the two
/// buckets, and widening it needs the same P3 sample data.
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    // Round the non-CJK bucket up: a three-character input is one token, not
    // zero, and a zero estimate would make every threshold vacuously true.
    cjk + other.div_ceil(4)
}

/// Whether a character is counted one-token-per-character by [`estimate_tokens`].
///
/// Covers the ranges that actually appear in aibo's Japanese, Chinese and Korean
/// traffic: kana, CJK ideographs and extension A, CJK punctuation, fullwidth
/// forms, compatibility ideographs and Hangul. Extension B and beyond are
/// included because a rare ideograph costs at least as much.
const fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF     // Hangul jamo
        | 0x2E80..=0x2EFF   // CJK radicals supplement
        | 0x3000..=0x303F   // CJK symbols and punctuation
        | 0x3040..=0x309F   // Hiragana
        | 0x30A0..=0x30FF   // Katakana
        | 0x3100..=0x312F   // Bopomofo
        | 0x3130..=0x318F   // Hangul compatibility jamo
        | 0x31F0..=0x31FF   // Katakana phonetic extensions
        | 0x3400..=0x4DBF   // CJK unified ideographs extension A
        | 0x4E00..=0x9FFF   // CJK unified ideographs
        | 0xA960..=0xA97F   // Hangul jamo extended-A
        | 0xAC00..=0xD7AF   // Hangul syllables
        | 0xD7B0..=0xD7FF   // Hangul jamo extended-B
        | 0xF900..=0xFAFF   // CJK compatibility ideographs
        | 0xFE30..=0xFE4F   // CJK compatibility forms
        | 0xFF00..=0xFFEF   // Halfwidth and fullwidth forms
        | 0x20000..=0x2FA1F // CJK ideograph extensions B..F, compatibility supplement
    )
}

// ---------------------------------------------------------------------------
// Surface inference (§1)
// ---------------------------------------------------------------------------

/// Everything the §1 surface rule looks at.
///
/// Borrowed, because it is built from the capture snapshot and consumed
/// immediately; nothing here is stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SurfaceInput<'a> {
    /// What the user has typed into the panel, verbatim.
    pub panel_input: &'a str,
    /// The selection captured from the source app, if any.
    pub selection: &'a str,
    /// Text before the caret in the focused field (`FieldContext::prefix`).
    pub field_prefix: &'a str,
    /// Context capture hit its deadline (§8) and the snapshot is incomplete.
    ///
    /// When true the answer is forced to [`Surface::Ask`]: §1 requires that
    /// capture latency never silently decides behaviour.
    pub capture_timed_out: bool,
}

/// Resolve one hotkey press into one [`Surface`] (§1).
///
/// ```text
/// if panel input starts with a known verb        → Do
/// else if selection is non-empty                 → Transform
/// else if focused field has text before caret    → Complete
/// else                                           → Ask
/// ```
///
/// Call this **once**, when context capture settles, and freeze the result for
/// the session; the panel displays it and `⇥` overrides it. Re-evaluating as
/// later capture data arrives is the bug this rule exists to prevent.
///
/// `do_verbs` is the registry of trigger words for the Do surface — see
/// [`DoVerbRegistry`]. Note that it is *not* [`Verb`]: [`Verb`] is the text
/// operation parsed for §4's rule 7 (`Define`, `Translate`, …), which routes an
/// **Ask**, and must not drag the request onto Do.
///
/// "Non-empty" is whitespace-insensitive throughout: a selection of three
/// spaces, or a field prefix of one newline, is not context.
pub fn infer_surface(input: &SurfaceInput<'_>, do_verbs: &DoVerbRegistry) -> Surface {
    // The verb test comes first because the panel input is typed by the user and
    // is always trustworthy. The timeout test comes next, ahead of the selection
    // and field branches, because those are exactly the fields a timed-out
    // capture may have left stale or half-filled (§1: never a guess).
    if do_verbs.matches_leading_verb(input.panel_input) {
        Surface::Do
    } else if input.capture_timed_out {
        Surface::Ask
    } else if !input.selection.trim().is_empty() {
        Surface::Transform
    } else if !input.field_prefix.trim().is_empty() {
        Surface::Complete
    } else {
        Surface::Ask
    }
}

/// The trigger words that select the Do surface (§1).
///
/// These are **not** [`Verb`]. §12's `actions.verb` column makes the Do
/// vocabulary user-extensible ("optional trigger word" on a saved action), so
/// this is a registry rather than an enum, and it is matched case-insensitively
/// against the first word of the panel input.
///
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoVerbRegistry {
    verbs: Vec<String>,
}

impl DoVerbRegistry {
    /// The seed vocabulary shipped with aibo. Empty on purpose.
    ///
    /// The shipped trigger is the `/agent` slash command, parsed by the shell
    /// before routing (owner decision, 2026-08-01: a bare verb like `do`
    /// would misfire on ordinary questions — "do you think…"). This registry
    /// remains the seam for §12 saved-action trigger words.
    pub fn builtin() -> Self {
        Self::default()
    }

    /// A registry over exactly these trigger words.
    pub fn new<I, S>(verbs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut registry = Self::default();
        for verb in verbs {
            registry.insert(verb);
        }
        registry
    }

    /// Register one trigger word. Blank words are ignored: a blank
    /// `actions.verb` must not make every panel input agentic.
    pub fn insert<S: Into<String>>(&mut self, verb: S) -> &mut Self {
        let verb = verb.into().trim().to_lowercase();
        if !verb.is_empty() && !self.verbs.contains(&verb) {
            self.verbs.push(verb);
        }
        self
    }

    /// Whether the registry holds no trigger words.
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    /// The registered trigger words, lowercased, in insertion order.
    pub fn verbs(&self) -> impl Iterator<Item = &str> {
        self.verbs.iter().map(String::as_str)
    }

    /// Whether the first word of `panel_input` is a registered trigger word.
    ///
    /// Matching is case-insensitive and ignores a trailing `:` or `,`, so both
    /// `deploy the branch` and `Deploy: the branch` trigger.
    pub fn matches_leading_verb(&self, panel_input: &str) -> bool {
        let Some(word) = leading_word(panel_input) else {
            return false;
        };
        let word = word.to_lowercase();
        self.verbs.contains(&word)
    }
}

/// The first whitespace-delimited word of `input`, with trailing `:`/`,`
/// stripped. `None` if the input is blank or the word is punctuation only.
fn leading_word(input: &str) -> Option<&str> {
    let word = input.split_whitespace().next()?;
    let word = word.trim_end_matches([':', ',']);
    (!word.is_empty()).then_some(word)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every surface, for exhaustive enumeration.
    const SURFACES: [Surface; 4] = [
        Surface::Complete,
        Surface::Transform,
        Surface::Ask,
        Surface::Do,
    ];

    /// Every role, for exhaustive enumeration of `user_override`.
    const ROLES: [Role; 5] = [
        Role::Fast,
        Role::Smart,
        Role::Cheap,
        Role::Vision,
        Role::Agent,
    ];

    /// Every verb, for exhaustive enumeration of `verb`.
    const VERBS: [Verb; 10] = [
        Verb::Translate,
        Verb::Define,
        Verb::Fix,
        Verb::Explain,
        Verb::Summarise,
        Verb::Spell,
        Verb::Convert,
        Verb::Rewrite,
        Verb::Shorten,
        Verb::Expand,
    ];

    /// Values chosen around every threshold in the table, plus the extremes.
    const TOKEN_SAMPLES: [usize; 8] = [0, 1, 59, 60, 61, 399, 400, 401];

    fn input(surface: Surface) -> RouteInput {
        RouteInput {
            surface,
            prompt_tokens: 0,
            payload_tokens: 0,
            has_code: false,
            has_image: false,
            verb: None,
            user_override: None,
        }
    }

    fn role_of(input: &RouteInput) -> Role {
        Router::with_defaults().route(input).role
    }

    fn verbs_and_none() -> impl Iterator<Item = Option<Verb>> {
        VERBS.map(Some).into_iter().chain([None])
    }

    fn roles_and_none() -> impl Iterator<Item = Option<Role>> {
        ROLES.map(Some).into_iter().chain([None])
    }

    // -- rule 1: user_override ---------------------------------------------

    #[test]
    fn rule_1_user_override_wins_for_every_role_on_every_surface() {
        for surface in SURFACES {
            for role in ROLES {
                let mut i = input(surface);
                i.user_override = Some(role);
                let routed = Router::with_defaults().route(&i);
                assert_eq!(routed.role, role, "{surface:?} + override {role:?}");
                assert_eq!(routed.rule, "user_override");
                assert_eq!(routed.index, Some(0));
            }
        }
    }

    #[test]
    fn rule_1_user_override_beats_even_an_image() {
        // An explicit @model choice is never overruled — including by rule 2.
        // If the chosen role cannot see the image, that is the provider chain's
        // problem to report, not the router's to paper over.
        let mut i = input(Surface::Ask);
        i.has_image = true;
        i.user_override = Some(Role::Cheap);
        assert_eq!(role_of(&i), Role::Cheap);
    }

    #[test]
    fn rule_1_none_override_does_not_match() {
        // The rule returns `Option<Role>` straight through; a `None` override
        // must fall through rather than claim the request.
        assert_eq!(
            BuiltinRule::UserOverride.evaluate(&input(Surface::Ask)),
            None
        );
    }

    // -- rule 2: has_image --------------------------------------------------

    #[test]
    fn rule_2_image_routes_to_vision_on_every_surface() {
        for surface in SURFACES {
            let mut i = input(surface);
            i.has_image = true;
            let routed = Router::with_defaults().route(&i);
            assert_eq!(routed.role, Role::Vision, "{surface:?} with image");
            assert_eq!(routed.rule, "image");
        }
    }

    #[test]
    fn rule_2_image_outranks_do() {
        // Documented consequence of first-match-wins: an agentic request that
        // carries an image lands on Vision, not Agent. That is what §4's table
        // says, and it is pinned here so changing it has to be deliberate.
        let mut i = input(Surface::Do);
        i.has_image = true;
        assert_eq!(role_of(&i), Role::Vision);
    }

    // -- rule 3: Do ---------------------------------------------------------

    #[test]
    fn rule_3_do_is_agent_regardless_of_size_code_or_verb() {
        for verb in verbs_and_none() {
            for has_code in [false, true] {
                let mut i = input(Surface::Do);
                i.verb = verb;
                i.has_code = has_code;
                i.prompt_tokens = 100_000;
                i.payload_tokens = 100_000;
                assert_eq!(role_of(&i), Role::Agent, "{verb:?} code={has_code}");
            }
        }
    }

    // -- rule 4: Complete ---------------------------------------------------

    #[test]
    fn rule_4_complete_is_always_fast() {
        // Complete has a 250 ms first-token budget (§1). Nothing about payload
        // size or code-ness may promote it to Smart.
        for has_code in [false, true] {
            let mut i = input(Surface::Complete);
            i.has_code = has_code;
            i.payload_tokens = 100_000;
            i.prompt_tokens = 100_000;
            let routed = Router::with_defaults().route(&i);
            assert_eq!(routed.role, Role::Fast);
            assert_eq!(routed.rule, "complete_is_fast");
        }
    }

    // -- rules 5 and 6: Transform ------------------------------------------

    #[test]
    fn rule_5_short_prose_transform_is_fast() {
        let mut i = input(Surface::Transform);
        i.payload_tokens = 399;
        let routed = Router::with_defaults().route(&i);
        assert_eq!(routed.role, Role::Fast);
        assert_eq!(routed.rule, "short_prose_transform_is_fast");
    }

    #[test]
    fn rule_5_payload_boundary_is_inclusive_at_400() {
        let mut at = input(Surface::Transform);
        at.payload_tokens = TRANSFORM_FAST_MAX_PAYLOAD_TOKENS; // 400
        assert_eq!(role_of(&at), Role::Fast, "400 must still be Fast");

        let mut over = input(Surface::Transform);
        over.payload_tokens = TRANSFORM_FAST_MAX_PAYLOAD_TOKENS + 1; // 401
        assert_eq!(role_of(&over), Role::Smart, "401 must be Smart");
    }

    #[test]
    fn rule_5_zero_payload_transform_is_fast() {
        assert_eq!(role_of(&input(Surface::Transform)), Role::Fast);
    }

    #[test]
    fn rule_5_ignores_prompt_tokens() {
        // Rule 5 keys on payload, not prompt: a long instruction over a short
        // selection is still a short Transform.
        let mut i = input(Surface::Transform);
        i.prompt_tokens = 100_000;
        i.payload_tokens = 10;
        assert_eq!(role_of(&i), Role::Fast);
    }

    #[test]
    fn rule_6_code_transform_is_smart_even_when_tiny() {
        let mut i = input(Surface::Transform);
        i.payload_tokens = 1;
        i.has_code = true;
        let routed = Router::with_defaults().route(&i);
        assert_eq!(routed.role, Role::Smart);
        assert_eq!(routed.rule, "transform_is_smart");
    }

    #[test]
    fn rule_6_long_transform_is_smart_even_without_code() {
        let mut i = input(Surface::Transform);
        i.payload_tokens = 100_000;
        assert_eq!(role_of(&i), Role::Smart);
    }

    // -- rules 7 and 8: Ask -------------------------------------------------

    #[test]
    fn rule_7_short_lookup_verbs_are_fast() {
        for verb in ASK_FAST_VERBS {
            let mut i = input(Surface::Ask);
            i.verb = Some(verb);
            i.prompt_tokens = 10;
            let routed = Router::with_defaults().route(&i);
            assert_eq!(routed.role, Role::Fast, "{verb:?}");
            assert_eq!(routed.rule, "short_lookup_ask_is_fast");
        }
    }

    #[test]
    fn rule_7_non_lookup_verbs_are_smart_however_short() {
        for verb in VERBS {
            if ASK_FAST_VERBS.contains(&verb) {
                continue;
            }
            let mut i = input(Surface::Ask);
            i.verb = Some(verb);
            i.prompt_tokens = 0;
            assert_eq!(role_of(&i), Role::Smart, "{verb:?} must not be Fast");
        }
    }

    #[test]
    fn rule_7_prompt_boundary_is_inclusive_at_60() {
        let mut at = input(Surface::Ask);
        at.verb = Some(Verb::Define);
        at.prompt_tokens = ASK_FAST_MAX_PROMPT_TOKENS; // 60
        assert_eq!(role_of(&at), Role::Fast, "60 must still be Fast");

        let mut over = input(Surface::Ask);
        over.verb = Some(Verb::Define);
        over.prompt_tokens = ASK_FAST_MAX_PROMPT_TOKENS + 1; // 61
        assert_eq!(role_of(&over), Role::Smart, "61 must be Smart");
    }

    #[test]
    fn rule_7_needs_a_verb() {
        let mut i = input(Surface::Ask);
        i.prompt_tokens = 1;
        assert_eq!(role_of(&i), Role::Smart, "no verb → rule 8");
    }

    #[test]
    fn rule_7_ignores_payload_tokens() {
        // Rule 7 keys on prompt, not payload: "Define" with a huge attached
        // clipboard is still a short lookup by the table's own wording.
        let mut i = input(Surface::Ask);
        i.verb = Some(Verb::Define);
        i.prompt_tokens = 5;
        i.payload_tokens = 100_000;
        assert_eq!(role_of(&i), Role::Fast);
    }

    #[test]
    fn rule_7_ignores_has_code() {
        // Unlike rule 5, rule 7 has no `!has_code` term. Pinned so that nobody
        // "fixes" the asymmetry without changing the plan first.
        let mut i = input(Surface::Ask);
        i.verb = Some(Verb::Convert);
        i.prompt_tokens = 5;
        i.has_code = true;
        assert_eq!(role_of(&i), Role::Fast);
    }

    #[test]
    fn rule_8_plain_ask_is_smart() {
        let routed = Router::with_defaults().route(&input(Surface::Ask));
        assert_eq!(routed.role, Role::Smart);
        assert_eq!(routed.rule, "ask_is_smart");
        assert_eq!(routed.index, Some(7));
    }

    // -- the whole table, brute-forced -------------------------------------

    /// A literal transcription of §4's table as the if-chain the plan rejects.
    /// The rule list must agree with it on every input; this is the test that
    /// makes the rule-list shape safe to adopt.
    ///
    /// Rows 4, 5 and 7 all yield `Fast`, which clippy reads as duplicated
    /// branches. Collapsing them would stop this being a transcription and would
    /// destroy the independence that makes it a useful oracle.
    #[allow(clippy::if_same_then_else)]
    fn table_reference(i: &RouteInput) -> Role {
        if let Some(role) = i.user_override {
            role
        } else if i.has_image {
            Role::Vision
        } else if i.surface == Surface::Do {
            Role::Agent
        } else if i.surface == Surface::Complete {
            Role::Fast
        } else if i.surface == Surface::Transform && i.payload_tokens <= 400 && !i.has_code {
            Role::Fast
        } else if i.surface == Surface::Transform {
            Role::Smart
        } else if i.surface == Surface::Ask
            && i.prompt_tokens <= 60
            && matches!(
                i.verb,
                Some(Verb::Define | Verb::Translate | Verb::Spell | Verb::Convert)
            )
        {
            Role::Fast
        } else {
            Role::Smart
        }
    }

    #[test]
    fn rule_list_matches_the_table_on_every_input() {
        let router = Router::with_defaults();
        let mut cases = 0usize;

        for user_override in roles_and_none() {
            for surface in SURFACES {
                for verb in verbs_and_none() {
                    for has_code in [false, true] {
                        for has_image in [false, true] {
                            for prompt_tokens in TOKEN_SAMPLES {
                                for payload_tokens in TOKEN_SAMPLES {
                                    let i = RouteInput {
                                        surface,
                                        prompt_tokens,
                                        payload_tokens,
                                        has_code,
                                        has_image,
                                        verb,
                                        user_override,
                                    };
                                    let routed = router.route(&i);
                                    assert_eq!(routed.role, table_reference(&i), "{i:?}");
                                    // The built-in list is exhaustive over
                                    // Surface: the fallback is unreachable.
                                    assert!(routed.index.is_some(), "fell through: {i:?}");
                                    cases += 1;
                                }
                            }
                        }
                    }
                }
            }
        }

        assert_eq!(cases, 6 * 4 * 11 * 2 * 2 * 8 * 8);
    }

    #[test]
    fn every_builtin_rule_is_reachable() {
        // A rule that can never match is dead code masquerading as policy.
        let router = Router::with_defaults();
        let mut hit = [false; BuiltinRule::ALL.len()];

        for user_override in roles_and_none() {
            for surface in SURFACES {
                for verb in verbs_and_none() {
                    for has_code in [false, true] {
                        for has_image in [false, true] {
                            for tokens in TOKEN_SAMPLES {
                                let i = RouteInput {
                                    surface,
                                    prompt_tokens: tokens,
                                    payload_tokens: tokens,
                                    has_code,
                                    has_image,
                                    verb,
                                    user_override,
                                };
                                if let Some(index) = router.route(&i).index {
                                    hit[index] = true;
                                }
                            }
                        }
                    }
                }
            }
        }

        for (index, reached) in hit.iter().enumerate() {
            assert!(
                reached,
                "rule {index} ({}) unreachable",
                BuiltinRule::ALL[index].name()
            );
        }
    }

    #[test]
    fn rule_order_and_names_are_stable() {
        let names: Vec<_> = Router::with_defaults().rule_names().collect();
        assert_eq!(
            names,
            [
                "user_override",
                "image",
                "do_is_agent",
                "complete_is_fast",
                "short_prose_transform_is_fast",
                "transform_is_smart",
                "short_lookup_ask_is_fast",
                "ask_is_smart",
            ]
        );
    }

    // -- extensibility: the reason this is a list, not an if-chain ----------

    #[derive(Debug)]
    struct PinnedRule(Role);

    impl Rule for PinnedRule {
        fn name(&self) -> &'static str {
            "per_app_default"
        }
        fn evaluate(&self, _input: &RouteInput) -> Option<Role> {
            Some(self.0)
        }
    }

    #[derive(Debug)]
    struct NeverRule;

    impl Rule for NeverRule {
        fn name(&self) -> &'static str {
            "never"
        }
        fn evaluate(&self, _input: &RouteInput) -> Option<Role> {
            None
        }
    }

    #[test]
    fn prepended_rule_outranks_the_builtins() {
        // "Anthropic in VS Code" — a per-app default must beat rule 4.
        let mut router = Router::with_defaults();
        router.prepend(Box::new(PinnedRule(Role::Smart)));
        let routed = router.route(&input(Surface::Complete));
        assert_eq!(routed.role, Role::Smart);
        assert_eq!(routed.rule, "per_app_default");
        assert_eq!(routed.index, Some(0));
        assert_eq!(router.len(), 9);
    }

    #[test]
    fn pushed_rule_is_a_catch_all_below_the_builtins() {
        let mut router = Router::with_defaults();
        router.push(Box::new(PinnedRule(Role::Cheap)));
        // The built-ins are exhaustive, so a trailing catch-all never fires.
        assert_eq!(router.route(&input(Surface::Ask)).role, Role::Smart);
        assert_eq!(router.route(&input(Surface::Ask)).rule, "ask_is_smart");
    }

    #[test]
    fn insert_places_a_rule_at_an_exact_precedence() {
        // Below the user override, above `has_image`: a budget cap that must not
        // silently overrule an explicit @model choice.
        let mut router = Router::with_defaults();
        router.insert(1, Box::new(PinnedRule(Role::Cheap)));

        let mut i = input(Surface::Ask);
        i.has_image = true;
        assert_eq!(router.route(&i).role, Role::Cheap);

        i.user_override = Some(Role::Smart);
        assert_eq!(router.route(&i).role, Role::Smart);
    }

    #[test]
    fn empty_router_falls_back() {
        let router = Router::empty();
        assert!(router.is_empty());
        let routed = router.route(&input(Surface::Do));
        assert_eq!(routed.role, FALLBACK_ROLE);
        assert_eq!(routed.rule, "fallback");
        assert_eq!(routed.index, None);
    }

    #[test]
    fn declining_rules_are_skipped_not_terminal() {
        let router = Router::from_rules(vec![
            Box::new(NeverRule),
            Box::new(NeverRule),
            Box::new(PinnedRule(Role::Vision)),
        ]);
        let routed = router.route(&input(Surface::Ask));
        assert_eq!(routed.role, Role::Vision);
        assert_eq!(routed.index, Some(2));
    }

    #[test]
    fn router_is_send_and_sync() {
        // The router is shared between the capture thread and the runtime.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Router>();
    }

    // -- escalation ---------------------------------------------------------

    #[test]
    fn escalation_is_to_smart_and_idempotent() {
        assert_eq!(escalated(Role::Fast), Role::Smart);
        assert_eq!(escalated(Role::Cheap), Role::Smart);
        assert_eq!(escalated(Role::Smart), Role::Smart);
        // Escalating these away would drop the capability that selected them.
        assert_eq!(escalated(Role::Vision), Role::Vision);
        assert_eq!(escalated(Role::Agent), Role::Agent);
    }

    #[test]
    fn escalation_is_offered_only_on_the_two_documented_signals() {
        assert!(should_offer_escalation(Role::Fast, true, 500));
        assert!(should_offer_escalation(Role::Fast, false, 9));
        assert!(!should_offer_escalation(Role::Fast, false, 10));
        // Never for a Smart answer: there is nothing to escalate to.
        assert!(!should_offer_escalation(Role::Smart, true, 0));
    }

    // -- token estimation and the CJK hazard --------------------------------

    #[test]
    fn estimate_tokens_on_ascii_is_chars_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1); // rounds up: never report zero
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(&"x".repeat(400)), 100);
    }

    #[test]
    fn estimate_tokens_counts_cjk_one_per_character() {
        assert_eq!(estimate_tokens("日本語"), 3);
        assert_eq!(estimate_tokens("ひらがな"), 4);
        assert_eq!(estimate_tokens("カタカナ"), 4);
        assert_eq!(estimate_tokens("한국어"), 3);
        assert_eq!(estimate_tokens("。、「」"), 4); // CJK punctuation counts too
    }

    #[test]
    fn estimate_tokens_handles_mixed_scripts() {
        // 4 ASCII (→1) + 2 CJK (→2) = 3
        assert_eq!(estimate_tokens("abcd日本"), 3);
    }

    /// The mis-routing hazard §4 documents, made concrete.
    ///
    /// `bytes / 4` on Japanese under-counts by ~3x because each character is
    /// three UTF-8 bytes. A 500-character Japanese selection is 1500 bytes →
    /// 375 "tokens" → under the 400 threshold → **Fast**, where the equivalent
    /// English content would have gone to Smart. The corrected estimate puts it
    /// at 500 and routes it correctly.
    #[test]
    fn cjk_misrouting_hazard() {
        let japanese = "本".repeat(500);
        assert_eq!(japanese.len(), 1500, "3 bytes per character");

        let naive_bytes_over_four = japanese.len() / 4;
        assert_eq!(naive_bytes_over_four, 375);
        assert!(
            naive_bytes_over_four <= TRANSFORM_FAST_MAX_PAYLOAD_TOKENS,
            "the naive heuristic keeps this under the threshold — that is the bug"
        );

        let corrected = estimate_tokens(&japanese);
        assert_eq!(corrected, 500);
        assert!(corrected > TRANSFORM_FAST_MAX_PAYLOAD_TOKENS);

        // Same selection, two heuristics, two different roles.
        let mut naive = input(Surface::Transform);
        naive.payload_tokens = naive_bytes_over_four;
        assert_eq!(role_of(&naive), Role::Fast, "the mis-route");

        let mut fixed = input(Surface::Transform);
        fixed.payload_tokens = corrected;
        assert_eq!(role_of(&fixed), Role::Smart, "the fix");
    }

    #[test]
    fn cjk_hazard_also_bites_the_ask_threshold() {
        // 80 Japanese characters: 240 bytes → 60 naive "tokens" → exactly on the
        // rule-7 boundary, so a long Japanese question keeps a Fast model.
        let question = "質".repeat(80);
        assert_eq!(question.len() / 4, ASK_FAST_MAX_PROMPT_TOKENS);
        assert_eq!(estimate_tokens(&question), 80);

        let mut naive = input(Surface::Ask);
        naive.verb = Some(Verb::Translate);
        naive.prompt_tokens = question.len() / 4;
        assert_eq!(role_of(&naive), Role::Fast, "the mis-route");

        let mut fixed = input(Surface::Ask);
        fixed.verb = Some(Verb::Translate);
        fixed.prompt_tokens = estimate_tokens(&question);
        assert_eq!(role_of(&fixed), Role::Smart, "the fix");
    }

    #[test]
    fn ascii_estimate_is_unchanged_by_the_correction() {
        // The fix must not move English routing: for pure ASCII, chars/4 and
        // bytes/4 agree.
        let english = "word ".repeat(320); // 1600 ASCII bytes
        assert_eq!(english.len() / 4, 400);
        assert_eq!(estimate_tokens(&english), 400);
    }

    // -- surface inference (§1) ---------------------------------------------

    fn deploy_verbs() -> DoVerbRegistry {
        DoVerbRegistry::new(["deploy", "refactor"])
    }

    #[test]
    fn surface_do_when_input_starts_with_a_known_verb() {
        let input = SurfaceInput {
            panel_input: "deploy the staging branch",
            selection: "some selected text",
            field_prefix: "typed text",
            capture_timed_out: false,
        };
        // Do outranks both Transform and Complete context.
        assert_eq!(infer_surface(&input, &deploy_verbs()), Surface::Do);
    }

    #[test]
    fn surface_verb_matching_is_case_insensitive_and_punctuation_tolerant() {
        for panel_input in [
            "Deploy now",
            "DEPLOY",
            "deploy:",
            "Deploy, now",
            "  deploy x",
        ] {
            let input = SurfaceInput {
                panel_input,
                ..Default::default()
            };
            assert_eq!(
                infer_surface(&input, &deploy_verbs()),
                Surface::Do,
                "{panel_input:?}"
            );
        }
    }

    #[test]
    fn surface_verb_must_lead() {
        // "known verb" means *starts with*; a verb mid-sentence is prose.
        let input = SurfaceInput {
            panel_input: "should I deploy this?",
            ..Default::default()
        };
        assert_eq!(infer_surface(&input, &deploy_verbs()), Surface::Ask);
    }

    #[test]
    fn surface_verb_must_be_a_whole_word() {
        let input = SurfaceInput {
            panel_input: "deployment plan",
            ..Default::default()
        };
        assert_eq!(infer_surface(&input, &deploy_verbs()), Surface::Ask);
    }

    #[test]
    fn surface_transform_when_selection_is_non_empty() {
        let input = SurfaceInput {
            panel_input: "make it shorter",
            selection: "a paragraph",
            field_prefix: "typed text",
            capture_timed_out: false,
        };
        // Selection outranks field prefix.
        assert_eq!(infer_surface(&input, &deploy_verbs()), Surface::Transform);
    }

    #[test]
    fn surface_complete_when_only_a_field_prefix_exists() {
        let input = SurfaceInput {
            field_prefix: "Dear Dr. Tanaka,",
            ..Default::default()
        };
        assert_eq!(infer_surface(&input, &deploy_verbs()), Surface::Complete);
    }

    #[test]
    fn surface_ask_with_no_context_at_all() {
        assert_eq!(
            infer_surface(&SurfaceInput::default(), &deploy_verbs()),
            Surface::Ask
        );
    }

    #[test]
    fn surface_whitespace_is_not_context() {
        // A selection of spaces or a prefix of a newline must not flip the
        // surface; the user selected nothing.
        let input = SurfaceInput {
            selection: "   \t",
            field_prefix: "\n  ",
            ..Default::default()
        };
        assert_eq!(infer_surface(&input, &deploy_verbs()), Surface::Ask);
    }

    #[test]
    fn surface_timeout_is_ask_never_a_guess() {
        // §1: if capture times out the surface is Ask. Any selection/prefix left
        // in the struct is stale or partial and must not be trusted.
        let input = SurfaceInput {
            panel_input: "",
            selection: "half-captured text",
            field_prefix: "half-captured prefix",
            capture_timed_out: true,
        };
        assert_eq!(infer_surface(&input, &deploy_verbs()), Surface::Ask);
    }

    #[test]
    fn surface_timeout_still_honours_a_typed_verb() {
        // The panel input is typed by the user, not captured, so a capture
        // timeout says nothing about it.
        let input = SurfaceInput {
            panel_input: "deploy it",
            capture_timed_out: true,
            ..Default::default()
        };
        assert_eq!(infer_surface(&input, &deploy_verbs()), Surface::Do);
    }

    #[test]
    fn surface_text_verbs_do_not_trigger_do() {
        // The §4 `Verb` vocabulary (Translate, Define, …) routes an Ask; it must
        // not be confused with the Do trigger registry. This is the one place
        // where §1 and §4 could be read as contradicting each other.
        for word in ["translate", "define", "summarise", "fix", "explain"] {
            let panel_input = format!("{word} this");
            let input = SurfaceInput {
                panel_input: &panel_input,
                ..Default::default()
            };
            assert_eq!(
                infer_surface(&input, &DoVerbRegistry::builtin()),
                Surface::Ask,
                "{word}"
            );
        }
    }

    #[test]
    fn the_builtin_do_vocabulary_is_empty_because_the_trigger_is_slash_agent() {
        // The shell parses `/agent …` before routing; a verb here would make
        // ordinary sentences agentic ("do you think…"). Saved actions are the
        // only thing that populates this registry.
        let verbs = DoVerbRegistry::builtin();
        assert!(verbs.is_empty());
        let surface = |text: &str| {
            infer_surface(
                &SurfaceInput {
                    panel_input: text,
                    ..Default::default()
                },
                &verbs,
            )
        };
        assert_eq!(surface("do fix the failing test"), Surface::Ask);
        assert_eq!(
            surface("dotfiles cleanup?"),
            Surface::Ask,
            "whole word only"
        );
        assert_eq!(surface("anything at all"), Surface::Ask);
    }

    #[test]
    fn do_verb_registry_ignores_blank_and_duplicate_entries() {
        // §12 `actions.verb` is nullable; a blank trigger word must not make
        // every input agentic.
        let registry = DoVerbRegistry::new(["", "   ", "Deploy", "deploy", " DEPLOY "]);
        assert_eq!(registry.verbs().collect::<Vec<_>>(), ["deploy"]);
        assert!(registry.matches_leading_verb("deploy now"));
        assert!(!registry.matches_leading_verb(""));
        assert!(!registry.matches_leading_verb("   "));
        assert!(!registry.matches_leading_verb(":"));
    }

    #[test]
    fn surface_inference_is_pure_and_repeatable() {
        // §1 requires the answer be frozen once capture settles; the least this
        // module can guarantee is that identical input gives identical output.
        let input = SurfaceInput {
            panel_input: "rewrite this",
            selection: "text",
            field_prefix: "prefix",
            capture_timed_out: false,
        };
        let verbs = deploy_verbs();
        let first = infer_surface(&input, &verbs);
        for _ in 0..100 {
            assert_eq!(infer_surface(&input, &verbs), first);
        }
    }
}
