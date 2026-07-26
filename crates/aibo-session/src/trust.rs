//! §14: *"Fallback is a spend and privacy decision, not just a reliability
//! one."*
//!
//! > *"A role chain that silently retries elsewhere can double-spend **and**
//! > send the user's selected text to a provider they didn't choose —
//! > unacceptable for the Azure/Bedrock 'secure tier' audience the plan
//! > targets. Fallback chains must be explicitly enabled per role, must never
//! > cross a provider's trust boundary without consent, and must be visible
//! > when they fire."*
//!
//! Three separate controls, all of which this crate honours:
//!
//! 1. *explicitly enabled per role* — [`aibo_core::types::RoleChain::fallback_enabled`],
//!    already enforced by `RoleBindings::dispatch_order`.
//! 2. *never cross a trust boundary without consent* — this module, gated on
//!    [`aibo_core::types::RoleChain::allow_crossing_trust_boundary`].
//! 3. *visible when they fire* — `SessionEvent::Dispatched::substituted_for`.

use std::collections::BTreeMap;

use aibo_core::types::ProviderId;

/// Which side of the privacy line a provider sits on.
///
/// This is about *where the user's selected text ends up*, not about the
/// provider's security engineering. A tenant-scoped endpoint the user
/// administers is not the same trust decision as a shared multi-tenant API,
/// and §14 says aibo may not silently move between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrustBoundary {
    /// On the user's own machine or inside infrastructure they administer:
    /// Ollama and llama.cpp locally, Azure OpenAI in their tenant, Bedrock in
    /// their AWS account, Vertex in their GCP project.
    Private,
    /// A shared multi-tenant API the user does not administer.
    Public,
}

/// The shipped classification.
///
/// Deliberately a small, explicit table rather than a heuristic: getting this
/// wrong in the permissive direction is the exact failure §14 names, and a
/// provider aibo has never heard of is assumed [`TrustBoundary::Public`],
/// which is the conservative answer.
pub fn default_boundary(provider: &ProviderId) -> TrustBoundary {
    match provider.as_str() {
        "ollama" | "azure-openai" | "bedrock" | "vertex" => TrustBoundary::Private,
        _ => TrustBoundary::Public,
    }
}

/// The classification in force, with any user overrides applied.
///
/// The override map exists because §10 leaves the provider set open — a
/// self-hosted vLLM behind `ProviderKind::Custom` is `Private` to its owner and
/// aibo has no way to infer that.
#[derive(Debug, Clone, Default)]
pub struct TrustMap {
    overrides: BTreeMap<ProviderId, TrustBoundary>,
}

impl TrustMap {
    /// The shipped table with no overrides.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a provider's boundary explicitly.
    pub fn set(&mut self, provider: ProviderId, boundary: TrustBoundary) -> &mut Self {
        self.overrides.insert(provider, boundary);
        self
    }

    /// Where a provider sits.
    pub fn boundary(&self, provider: &ProviderId) -> TrustBoundary {
        self.overrides
            .get(provider)
            .copied()
            .unwrap_or_else(|| default_boundary(provider))
    }

    /// Whether moving from `from` to `to` crosses the line **outwards**.
    ///
    /// Only `Private → Public` counts. Falling back from a public API onto the
    /// user's own Ollama does not leak anything they had not already accepted,
    /// so it needs no consent.
    pub fn crosses_outwards(&self, from: &ProviderId, to: &ProviderId) -> bool {
        self.boundary(from) == TrustBoundary::Private && self.boundary(to) == TrustBoundary::Public
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_secure_tier_providers_are_private() {
        for id in [
            ProviderId::AZURE_OPENAI,
            ProviderId::BEDROCK,
            ProviderId::VERTEX,
            ProviderId::OLLAMA,
        ] {
            assert_eq!(default_boundary(&id), TrustBoundary::Private, "{id}");
        }
    }

    #[test]
    fn an_unknown_provider_is_assumed_public() {
        assert_eq!(
            default_boundary(&ProviderId::new("someones-new-endpoint")),
            TrustBoundary::Public
        );
    }

    #[test]
    fn only_the_outward_crossing_needs_consent() {
        let map = TrustMap::new();
        assert!(map.crosses_outwards(&ProviderId::AZURE_OPENAI, &ProviderId::OPENAI));
        assert!(!map.crosses_outwards(&ProviderId::OPENAI, &ProviderId::AZURE_OPENAI));
        assert!(!map.crosses_outwards(&ProviderId::OPENAI, &ProviderId::ANTHROPIC));
        assert!(!map.crosses_outwards(&ProviderId::BEDROCK, &ProviderId::VERTEX));
    }

    #[test]
    fn an_override_wins() {
        let mut map = TrustMap::new();
        map.set(ProviderId::new("self-hosted"), TrustBoundary::Private);
        assert!(!map.crosses_outwards(&ProviderId::BEDROCK, &ProviderId::new("self-hosted")));
    }
}
