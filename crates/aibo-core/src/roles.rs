//! The default role bindings shipped in onboarding (§4).
//!
//! §4 makes [`Role`] the routing substrate and [`RoleChain`] the thing a role
//! binds to, but neither the router nor `types.rs` says what the chains
//! actually *are*. This module is that table, as data:
//!
//! | Role | Default chain |
//! |---|---|
//! | [`Role::Fast`] | Cerebras → Groq → OpenAI (small). **Never Codex.** |
//! | [`Role::Smart`] | Codex (if authed) → Anthropic → OpenAI → Vertex |
//! | [`Role::Cheap`] | Ollama (if detected) → Cerebras |
//! | [`Role::Vision`] | OpenAI → Anthropic → Vertex |
//! | [`Role::Agent`] | `codex app-server` (if `codex` on `PATH`) → `NativeLoop` on `Smart` |
//!
//! # `Fast` must never bind Codex, and that is enforced, not documented
//!
//! §3a measured the Codex endpoint at a **435 ms TTFT floor** across every id
//! on its allowlist — `gpt-5.5` 435 ms, `gpt-5.6-terra` 446 ms,
//! `gpt-5.3-codex-spark` 499 ms, `gpt-5.6-luna` 515 ms, `gpt-5.6-sol` 623 ms —
//! with prefill negligible at this scale, so the floor is fixed overhead and
//! there is no small fast model on that path. `Complete`'s budget is 250 ms.
//! Codex misses it outright and sits at the edge of `Transform`'s.
//!
//! A comment saying so would not survive the first person who edits the table,
//! so the invariant has three enforcement points, in increasing order of
//! reach:
//!
//! 1. **Compile time.** [`assert_fast_never_binds_codex`] is a `const fn`
//!    evaluated in a `const` item below. Adding a Codex entry to
//!    [`FAST_CHAIN`] fails the build with the reason in the message.
//! 2. **Test time.** The tests at the bottom of this file re-check it through
//!    the public API, including via [`RoleBindings::seed`] under every
//!    combination of [`Availability`].
//! 3. **Run time.** [`RoleBindings::validate`] rejects the same thing in a
//!    *user-supplied* chain, which is the case the first two cannot see —
//!    §4's chains are user-editable, and a Codex entry typed into settings
//!    would otherwise silently blow the Complete budget on every keystroke.
//!
//! # The model ids here are a shipped default that will drift
//!
//! §10: "model catalogues rot… a v1.0 shipped with a hardcoded default will
//! start failing for users months later with an opaque 400". Everything in
//! [`DefaultEntry::model`] is a *seed*, resolved against `aibo-provider`'s
//! `ModelCatalogue` before dispatch so a retired id surfaces as "the model you
//! selected no longer exists, here's the closest". The Codex column is the one
//! exception: those five ids are measured, not guessed (§3a).

use std::collections::BTreeSet;

use crate::error::{AiboError, Result};
use crate::types::{ModelBinding, ProviderId, Role, RoleChain};

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// What must be true before a default chain entry is usable (§4's
/// parenthesised conditions).
///
/// The defaults are a single fixed table, but three of its rows are
/// conditional — "Codex (if authed)", "Ollama (if detected)", "`codex
/// app-server` (if `codex` on `PATH`)". Modelling the condition keeps the
/// table one piece of data instead of five hand-written variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Precondition {
    /// The provider merely has to be configured with a credential.
    Configured,
    /// §4 "(if authed)" — Codex needs a completed device-code login (§3a).
    Authenticated,
    /// §4 "(if detected)" — a local Ollama has to have answered.
    Detected,
    /// §4 "(if `codex` on `PATH`)" — the `app-server` engine is a subprocess.
    OnPath,
}

/// One row of a default chain.
///
/// `&'static str` rather than [`ModelBinding`] so the whole table is a `const`
/// and the compile-time invariant below can look at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultEntry {
    /// The provider id, matching a [`ProviderId`] constant.
    pub provider: &'static str,
    /// The seed model id. Drifts; see the module docs.
    pub model: &'static str,
    /// What must hold before this entry is offered.
    pub requires: Precondition,
}

impl DefaultEntry {
    /// The binding this entry seeds.
    pub fn binding(&self) -> ModelBinding {
        ModelBinding {
            provider: ProviderId::new(self.provider),
            model: self.model.to_string(),
        }
    }
}

/// The provider id Codex is reached under. A `&str` rather than
/// [`ProviderId::CODEX`] because the compile-time check needs it in `const`
/// position and [`ProviderId`] wraps a `Cow`, which is not `const`-comparable.
pub const CODEX_PROVIDER: &str = "codex";

/// §3a's measured TTFT floor for the Codex endpoint, in milliseconds. Quoted
/// in the error [`RoleBindings::validate`] produces so the refusal explains
/// itself rather than reading as an arbitrary policy.
///
/// §4's own table says 461 ms; §3a's measurement table, which is the one with
/// the numbers behind it, says 435 ms. The lower — more generous to Codex —
/// figure is used here, and it still misses `Complete`'s 250 ms budget by
/// 185 ms.
pub const CODEX_TTFT_FLOOR_MS: u32 = 435;

/// `Fast`: Cerebras → Groq → OpenAI (small). **Never Codex** (§4).
pub const FAST_CHAIN: &[DefaultEntry] = &[
    DefaultEntry {
        provider: "cerebras",
        model: "llama-3.3-70b",
        requires: Precondition::Configured,
    },
    DefaultEntry {
        provider: "groq",
        model: "llama-3.3-70b-versatile",
        requires: Precondition::Configured,
    },
    // §4 says "OpenAI (small)" — the small model specifically, not whatever
    // `Smart` uses. A frontier model here would miss the latency budget by the
    // same mechanism Codex does.
    DefaultEntry {
        provider: "openai",
        model: "gpt-5-mini",
        requires: Precondition::Configured,
    },
];

/// `Smart`: Codex (if authed) → Anthropic → OpenAI → Vertex (§4).
pub const SMART_CHAIN: &[DefaultEntry] = &[
    DefaultEntry {
        provider: CODEX_PROVIDER,
        // On the ChatGPT-plan allowlist (§3a). Slowest of the five at 623 ms
        // TTFT, which is irrelevant for `Smart` and disqualifying for `Fast`.
        model: "gpt-5.6-sol",
        requires: Precondition::Authenticated,
    },
    DefaultEntry {
        provider: "anthropic",
        model: "claude-sonnet-4-5",
        requires: Precondition::Configured,
    },
    DefaultEntry {
        provider: "openai",
        model: "gpt-5",
        requires: Precondition::Configured,
    },
    DefaultEntry {
        provider: "vertex",
        model: "gemini-3-pro",
        requires: Precondition::Configured,
    },
];

/// `Cheap`: Ollama (if detected) → Cerebras (§4).
pub const CHEAP_CHAIN: &[DefaultEntry] = &[
    DefaultEntry {
        provider: "ollama",
        model: "qwen3:8b",
        requires: Precondition::Detected,
    },
    DefaultEntry {
        provider: "cerebras",
        model: "llama-3.3-70b",
        requires: Precondition::Configured,
    },
];

/// `Vision`: OpenAI → Anthropic → Vertex (§4).
pub const VISION_CHAIN: &[DefaultEntry] = &[
    DefaultEntry {
        provider: "openai",
        model: "gpt-5",
        requires: Precondition::Configured,
    },
    DefaultEntry {
        provider: "anthropic",
        model: "claude-sonnet-4-5",
        requires: Precondition::Configured,
    },
    DefaultEntry {
        provider: "vertex",
        model: "gemini-3-pro",
        requires: Precondition::Configured,
    },
];

/// `Agent`: the model bindings [`AgentEngine::NativeLoop`] runs on.
///
/// §4's Agent row is *not* a chain of `(provider, model)` pairs — it is a chain
/// of engines: `codex app-server` (if `codex` on `PATH`) → `NativeLoop` on
/// `Smart`. The engine order lives in [`DEFAULT_AGENT_ENGINES`], because
/// [`RoleChain`] cannot express "delegate to a subprocess that picks its own
/// model". This constant is the other half: what `NativeLoop` puts in
/// [`crate::types::AgentTask::binding`], which §4 defines as the `Smart` chain.
pub const AGENT_CHAIN: &[DefaultEntry] = SMART_CHAIN;

/// An execution engine for [`Role::Agent`] (§4, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentEngine {
    /// `codex app-server` over stdio. Owns its own model, prompt and tools, so
    /// no [`ModelBinding`] applies (§3a, §5).
    CodexAppServer,
    /// aibo's own loop, running on the [`Role::Smart`] chain.
    NativeLoop,
}

/// §4's Agent row, in order, with the condition each engine carries.
pub const DEFAULT_AGENT_ENGINES: &[(AgentEngine, Precondition)] = &[
    (AgentEngine::CodexAppServer, Precondition::OnPath),
    // The floor of the chain: always available, so `Do` never has *no* engine.
    (AgentEngine::NativeLoop, Precondition::Configured),
];

/// The default chain for a role, before availability is applied.
pub const fn default_chain(role: Role) -> &'static [DefaultEntry] {
    match role {
        Role::Fast => FAST_CHAIN,
        Role::Smart => SMART_CHAIN,
        Role::Cheap => CHEAP_CHAIN,
        Role::Vision => VISION_CHAIN,
        Role::Agent => AGENT_CHAIN,
    }
}

/// Every role, in a stable order, so callers can iterate the table.
pub const ALL_ROLES: [Role; 5] = [
    Role::Fast,
    Role::Smart,
    Role::Cheap,
    Role::Vision,
    Role::Agent,
];

// ---------------------------------------------------------------------------
// The compile-time invariant
// ---------------------------------------------------------------------------

/// `const`-evaluable byte equality. `str::eq` is not `const`.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether a chain contains a Codex entry. `const fn` so the assertion below
/// runs at compile time.
pub const fn chain_binds_codex(chain: &[DefaultEntry]) -> bool {
    let mut i = 0;
    while i < chain.len() {
        if str_eq(chain[i].provider, CODEX_PROVIDER) {
            return true;
        }
        i += 1;
    }
    false
}

/// The `Fast`-never-Codex invariant of §4, as a `const fn`.
///
/// Called from a `const` item, so a Codex entry added to [`FAST_CHAIN`] is a
/// **compile error**, not a latency regression discovered in the field.
pub const fn assert_fast_never_binds_codex() {
    assert!(
        !chain_binds_codex(FAST_CHAIN),
        "§4: the Fast role must never bind Codex. §3a measured a 435 ms TTFT \
         floor across the whole ChatGPT-plan allowlist with prefill negligible, \
         so it is fixed overhead and there is no small model on that path. \
         Complete's budget is 250 ms. Bind Codex to Smart/Ask instead."
    );
}

const _: () = assert_fast_never_binds_codex();

// The same check for the derived chains, so `Cheap` and `Vision` cannot pick up
// a Codex entry either. `Smart` and `Agent` are *expected* to have one, and are
// asserted the other way so a silent removal is caught too.
const _: () = assert!(!chain_binds_codex(CHEAP_CHAIN));
const _: () = assert!(!chain_binds_codex(VISION_CHAIN));
const _: () = assert!(chain_binds_codex(SMART_CHAIN));

// ---------------------------------------------------------------------------
// Availability
// ---------------------------------------------------------------------------

/// What the app knows at seed time about which providers can actually be used
/// (§4's "if authed" / "if detected" / "if on `PATH`").
///
/// Gathered once during onboarding and again at startup. Entries whose
/// [`Precondition`] is not met are dropped from the seeded chain rather than
/// left in to fail on first use — a chain whose primary is a provider the user
/// never configured spends its first request discovering that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Availability {
    /// Providers holding a usable credential.
    pub configured: BTreeSet<ProviderId>,
    /// Codex has a live device-code token pair (§3a).
    pub codex_authenticated: bool,
    /// A local Ollama answered its health probe (§13).
    pub ollama_detected: bool,
    /// The `codex` binary is on `PATH`.
    pub codex_cli_on_path: bool,
}

impl Availability {
    /// Nothing configured — the onboarding cold start.
    pub fn none() -> Self {
        Self::default()
    }

    /// Every provider in `ids` is configured; the three conditional flags stay
    /// false unless set.
    pub fn configured<I>(ids: I) -> Self
    where
        I: IntoIterator<Item = ProviderId>,
    {
        Self {
            configured: ids.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Whether a default entry may be seeded.
    pub fn allows(&self, entry: &DefaultEntry) -> bool {
        let id = ProviderId::new(entry.provider);
        match entry.requires {
            Precondition::Configured => self.configured.contains(&id),
            // Authentication *is* the credential for Codex — there is no API
            // key to configure separately (§3a), so `configured` is not also
            // required.
            Precondition::Authenticated => self.codex_authenticated,
            // Likewise: a detected Ollama needs no credential (§10).
            Precondition::Detected => self.ollama_detected,
            Precondition::OnPath => self.codex_cli_on_path,
        }
    }

    /// Whether an [`AgentEngine`] is available.
    pub fn allows_engine(&self, engine: AgentEngine, requires: Precondition) -> bool {
        match engine {
            AgentEngine::CodexAppServer => {
                matches!(requires, Precondition::OnPath) && self.codex_cli_on_path
            }
            // `NativeLoop` is aibo's own code; it needs a usable `Smart` chain,
            // which the caller checks, not a precondition of its own.
            AgentEngine::NativeLoop => true,
        }
    }
}

// ---------------------------------------------------------------------------
// The seeded bindings
// ---------------------------------------------------------------------------

/// Position of a role in [`ALL_ROLES`], and so in [`RoleBindings`]'s storage.
///
/// [`Role`] is deliberately not `Ord` in `types.rs`, so a `BTreeMap` keyed by
/// it is not available. A fixed array indexed by this is better anyway: there
/// are exactly five roles, the slot for each always exists, and iteration order
/// is §4's table order rather than an accident of a derive.
const fn role_index(role: Role) -> usize {
    match role {
        Role::Fast => 0,
        Role::Smart => 1,
        Role::Cheap => 2,
        Role::Vision => 3,
        Role::Agent => 4,
    }
}

/// The user's role → chain map (§4), seeded from the table above.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleBindings {
    /// One slot per role, in [`ALL_ROLES`] order. `None` means the role has no
    /// chain at all, which is distinct from a chain with no usable entries.
    chains: [Option<RoleChain>; ALL_ROLES.len()],
}

impl RoleBindings {
    /// Seed the defaults, keeping only entries [`Availability`] permits.
    ///
    /// `fallback_enabled` is **false** on every seeded chain. §14: fallback is
    /// a spend *and* privacy decision — a silent retry can double-spend and can
    /// send the user's selected text to a provider they did not choose — so it
    /// is opt-in per role. The chain is still seeded in full so the setting has
    /// something to turn on.
    pub fn seed(availability: &Availability) -> Self {
        let seeded = Self::build(|entry| availability.allows(entry));
        // Cannot fail for the shipped table — the compile-time assertion above
        // covers it — but seeding is also the path a future config-driven table
        // would take, and a debug assertion is free.
        debug_assert!(seeded.validate().is_ok());
        seeded
    }

    /// Seed the full table with no availability filtering. For tests and for
    /// the settings UI, which shows entries the user has not configured yet so
    /// they know what turning a provider on would buy them.
    pub fn seed_unfiltered() -> Self {
        Self::build(|_| true)
    }

    fn build(mut keep: impl FnMut(&DefaultEntry) -> bool) -> Self {
        let mut chains: [Option<RoleChain>; ALL_ROLES.len()] = Default::default();
        for role in ALL_ROLES {
            let entries: Vec<ModelBinding> = default_chain(role)
                .iter()
                .filter(|e| keep(e))
                .map(DefaultEntry::binding)
                .collect();
            chains[role_index(role)] = Some(RoleChain {
                role,
                entries,
                fallback_enabled: false,
                allow_crossing_trust_boundary: false,
            });
        }
        Self { chains }
    }

    /// Build from explicit chains — the settings path.
    ///
    /// Validated, because this is the only way a Codex entry can reach
    /// [`Role::Fast`] once the shipped table is compile-time clean. A later
    /// chain for the same role replaces an earlier one.
    pub fn from_chains<I>(chains: I) -> Result<Self>
    where
        I: IntoIterator<Item = RoleChain>,
    {
        let mut bindings = Self::default();
        for chain in chains {
            let slot = role_index(chain.role);
            bindings.chains[slot] = Some(chain);
        }
        bindings.validate()?;
        Ok(bindings)
    }

    /// The chain bound to a role.
    pub fn chain(&self, role: Role) -> Option<&RoleChain> {
        self.chains[role_index(role)].as_ref()
    }

    /// The primary binding for a role — the first entry of its chain.
    pub fn primary(&self, role: Role) -> Option<&ModelBinding> {
        self.chain(role)?.entries.first()
    }

    /// The bindings a request may actually use, in order.
    ///
    /// One entry when `fallback_enabled` is false, which is the default (§14).
    pub fn dispatch_order(&self, role: Role) -> &[ModelBinding] {
        match self.chain(role) {
            Some(c) if c.fallback_enabled => &c.entries,
            Some(c) => &c.entries[..c.entries.len().min(1)],
            None => &[],
        }
    }

    /// Every chain, in §4's table order.
    pub fn chains(&self) -> impl Iterator<Item = &RoleChain> {
        self.chains.iter().flatten()
    }

    /// Replace one role's chain, re-validating.
    ///
    /// On rejection the previous chain is restored, so a bad edit in settings
    /// cannot leave a role half-configured.
    pub fn set_chain(&mut self, chain: RoleChain) -> Result<()> {
        let slot = role_index(chain.role);
        let previous = self.chains[slot].replace(chain);
        if let Err(e) = self.validate() {
            self.chains[slot] = previous;
            return Err(e);
        }
        Ok(())
    }

    /// Roles left with no usable binding, in order.
    ///
    /// The onboarding readout: with nothing configured every role is empty, and
    /// with only Cerebras configured `Fast` and `Cheap` work while `Smart`,
    /// `Vision` and `Agent` do not.
    pub fn unbound_roles(&self) -> Vec<Role> {
        ALL_ROLES
            .into_iter()
            .filter(|r| self.chain(*r).is_none_or(|c| c.entries.is_empty()))
            .collect()
    }

    /// §4's `Fast`-never-Codex invariant, applied to *user-supplied* chains.
    ///
    /// The compile-time assertion covers the shipped table; this covers the
    /// settings UI, an imported config and a future server-pushed manifest —
    /// the cases where the check cannot be static.
    pub fn validate(&self) -> Result<()> {
        if let Some(chain) = self.chain(Role::Fast)
            && let Some(bad) = chain
                .entries
                .iter()
                .find(|b| b.provider.as_str() == CODEX_PROVIDER)
        {
            return Err(AiboError::Internal(Box::new(FastRoleBindsCodex {
                model: bad.model.clone(),
            })));
        }
        Ok(())
    }
}

/// The `Fast`-never-Codex violation, as an error a caller can match on.
///
/// A named type rather than a string so the settings UI can render its own copy
/// while the `Display` here stays the diagnostic form (§13).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the Fast role cannot bind codex/{model}: §3a measured a {floor} ms TTFT floor \
     across the whole ChatGPT-plan allowlist, and Complete's budget is 250 ms. \
     Bind Codex to Smart or Ask instead.",
    floor = CODEX_TTFT_FLOOR_MS
)]
pub struct FastRoleBindsCodex {
    /// The Codex model that was bound.
    pub model: String,
}

/// The engines available for [`Role::Agent`], in §4's order.
///
/// `NativeLoop` is always last and always present, so `Do` degrades to aibo's
/// own loop rather than to nothing when `codex` is not installed.
pub fn agent_engines(availability: &Availability) -> Vec<AgentEngine> {
    DEFAULT_AGENT_ENGINES
        .iter()
        .filter(|(engine, requires)| availability.allows_engine(*engine, *requires))
        .map(|(engine, _)| *engine)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    fn everything() -> Availability {
        Availability {
            configured: [
                ProviderId::CEREBRAS,
                ProviderId::GROQ,
                ProviderId::OPENAI,
                ProviderId::ANTHROPIC,
                ProviderId::VERTEX,
                ProviderId::OLLAMA,
                ProviderId::CODEX,
            ]
            .into_iter()
            .collect(),
            codex_authenticated: true,
            ollama_detected: true,
            codex_cli_on_path: true,
        }
    }

    fn providers(chain: &RoleChain) -> Vec<&str> {
        chain.entries.iter().map(|b| b.provider.as_str()).collect()
    }

    // -- the table is §4's table --------------------------------------------

    #[test]
    fn the_seeded_chains_are_the_section_4_table() {
        let b = RoleBindings::seed(&everything());
        assert_eq!(
            providers(b.chain(Role::Fast).unwrap()),
            ["cerebras", "groq", "openai"]
        );
        assert_eq!(
            providers(b.chain(Role::Smart).unwrap()),
            ["codex", "anthropic", "openai", "vertex"]
        );
        assert_eq!(
            providers(b.chain(Role::Cheap).unwrap()),
            ["ollama", "cerebras"]
        );
        assert_eq!(
            providers(b.chain(Role::Vision).unwrap()),
            ["openai", "anthropic", "vertex"]
        );
        // §4: Agent runs `NativeLoop` on `Smart`.
        assert_eq!(
            providers(b.chain(Role::Agent).unwrap()),
            providers(b.chain(Role::Smart).unwrap())
        );
    }

    #[test]
    fn every_role_gets_a_chain() {
        let b = RoleBindings::seed(&everything());
        for role in ALL_ROLES {
            assert!(b.chain(role).is_some(), "{role:?}");
            assert!(b.primary(role).is_some(), "{role:?}");
        }
        assert!(b.unbound_roles().is_empty());
    }

    // -- Fast is never Codex ------------------------------------------------

    #[test]
    fn fast_never_binds_codex_in_the_shipped_table() {
        // The compile-time assertion already proves this; asserting it again
        // through the public API means a refactor that drops the `const _`
        // still fails something.
        assert!(!chain_binds_codex(FAST_CHAIN));
        assert_fast_never_binds_codex();
    }

    #[test]
    fn fast_never_binds_codex_under_any_availability() {
        // Brute-force every combination of the three conditional flags against
        // every subset-of-one credential set. Codex must never appear in Fast.
        for authed in [false, true] {
            for detected in [false, true] {
                for on_path in [false, true] {
                    let a = Availability {
                        configured: everything().configured,
                        codex_authenticated: authed,
                        ollama_detected: detected,
                        codex_cli_on_path: on_path,
                    };
                    let b = RoleBindings::seed(&a);
                    let fast = b.chain(Role::Fast).unwrap();
                    assert!(
                        !fast.entries.iter().any(|e| e.provider == ProviderId::CODEX),
                        "Codex reached Fast with {a:?}"
                    );
                    assert!(b.validate().is_ok());
                }
            }
        }
    }

    #[test]
    fn a_user_supplied_codex_binding_on_fast_is_rejected() {
        // The case the compile-time check cannot see: settings, an imported
        // config, or a pushed manifest.
        let err = RoleBindings::from_chains([RoleChain {
            role: Role::Fast,
            entries: vec![ModelBinding {
                provider: ProviderId::CODEX,
                model: "gpt-5.5".into(),
            }],
            fallback_enabled: false,
            allow_crossing_trust_boundary: false,
        }])
        .unwrap_err();
        let rendered = format!("{}", err.source().unwrap());
        assert!(rendered.contains("435 ms"), "{rendered}");
        assert!(rendered.contains("250 ms"), "{rendered}");
    }

    #[test]
    fn a_rejected_set_chain_leaves_the_previous_binding_intact() {
        let mut b = RoleBindings::seed(&everything());
        let before = b.chain(Role::Fast).unwrap().clone();
        let err = b.set_chain(RoleChain {
            role: Role::Fast,
            entries: vec![ModelBinding {
                provider: ProviderId::CODEX,
                model: "gpt-5.6-sol".into(),
            }],
            fallback_enabled: false,
            allow_crossing_trust_boundary: false,
        });
        assert!(err.is_err());
        assert_eq!(b.chain(Role::Fast).unwrap(), &before);
    }

    #[test]
    fn codex_on_smart_is_allowed_and_is_the_primary() {
        let b = RoleBindings::seed(&everything());
        assert_eq!(
            b.primary(Role::Smart).unwrap().provider,
            ProviderId::CODEX,
            "§4 puts Codex first on Smart when authed"
        );
    }

    // -- availability gating ------------------------------------------------

    #[test]
    fn unauthenticated_codex_drops_out_of_smart() {
        let a = Availability {
            codex_authenticated: false,
            ..everything()
        };
        let b = RoleBindings::seed(&a);
        assert_eq!(
            providers(b.chain(Role::Smart).unwrap()),
            ["anthropic", "openai", "vertex"]
        );
    }

    #[test]
    fn undetected_ollama_drops_out_of_cheap() {
        let a = Availability {
            ollama_detected: false,
            ..everything()
        };
        let b = RoleBindings::seed(&a);
        assert_eq!(providers(b.chain(Role::Cheap).unwrap()), ["cerebras"]);
    }

    #[test]
    fn nothing_configured_leaves_every_role_unbound() {
        let b = RoleBindings::seed(&Availability::none());
        assert_eq!(b.unbound_roles(), ALL_ROLES.to_vec());
    }

    #[test]
    fn one_configured_provider_is_enough_for_fast_and_cheap() {
        let b = RoleBindings::seed(&Availability::configured([ProviderId::CEREBRAS]));
        assert_eq!(providers(b.chain(Role::Fast).unwrap()), ["cerebras"]);
        assert_eq!(providers(b.chain(Role::Cheap).unwrap()), ["cerebras"]);
        assert_eq!(
            b.unbound_roles(),
            vec![Role::Smart, Role::Vision, Role::Agent]
        );
    }

    // -- fallback is off by default (§14) -----------------------------------

    #[test]
    fn fallback_is_off_on_every_seeded_chain() {
        // §14: fallback is a spend *and* privacy decision, so it is opt-in.
        let b = RoleBindings::seed(&everything());
        for chain in b.chains() {
            assert!(!chain.fallback_enabled, "{:?}", chain.role);
            assert!(!chain.allow_crossing_trust_boundary, "{:?}", chain.role);
        }
    }

    #[test]
    fn dispatch_order_is_one_entry_until_fallback_is_enabled() {
        let mut b = RoleBindings::seed(&everything());
        assert_eq!(b.dispatch_order(Role::Smart).len(), 1);
        let mut chain = b.chain(Role::Smart).unwrap().clone();
        chain.fallback_enabled = true;
        b.set_chain(chain).unwrap();
        assert_eq!(b.dispatch_order(Role::Smart).len(), 4);
    }

    #[test]
    fn dispatch_order_of_an_unbound_role_is_empty() {
        let b = RoleBindings::seed(&Availability::none());
        assert!(b.dispatch_order(Role::Fast).is_empty());
    }

    // -- agent engines ------------------------------------------------------

    #[test]
    fn the_app_server_leads_when_codex_is_on_path() {
        assert_eq!(
            agent_engines(&everything()),
            [AgentEngine::CodexAppServer, AgentEngine::NativeLoop]
        );
    }

    #[test]
    fn the_native_loop_is_always_the_floor() {
        // §4's Agent row must never be empty: `Do` degrades to aibo's own loop.
        let a = Availability {
            codex_cli_on_path: false,
            ..everything()
        };
        assert_eq!(agent_engines(&a), [AgentEngine::NativeLoop]);
        assert_eq!(
            agent_engines(&Availability::none()),
            [AgentEngine::NativeLoop]
        );
    }

    // -- misc ---------------------------------------------------------------

    #[test]
    fn const_str_eq_matches_the_runtime_one() {
        for (a, b) in [
            ("codex", "codex"),
            ("codex", "cerebras"),
            ("", ""),
            ("openai", "openai-x"),
        ] {
            assert_eq!(str_eq(a, b), a == b, "{a} vs {b}");
        }
    }

    #[test]
    fn seed_unfiltered_shows_the_whole_table() {
        let b = RoleBindings::seed_unfiltered();
        assert!(b.unbound_roles().is_empty());
        assert_eq!(b.chain(Role::Smart).unwrap().entries.len(), 4);
    }
}
