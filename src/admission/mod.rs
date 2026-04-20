//! Admission chain — pre-auth request gating (Phase 2).
//!
//! The admission chain runs before SigV4 verification and decides whether a
//! request should proceed as anonymous, continue to authentication, or be
//! rejected outright. It is the first of the five request-processing layers
//! (admission → identity → IAM → parameters → routing) in the configuration
//! architecture.
//!
//! # What Phase 2 ships
//!
//! This phase is intentionally narrow. The admission chain today is
//! synthesised entirely from existing bucket policy data — specifically the
//! per-bucket `public_prefixes` list. It replaces the inline
//! public-prefix-bypass logic that used to live in the SigV4 middleware,
//! but makes no new behaviour observable to operators beyond the trace
//! endpoint.
//!
//! # What's deferred
//!
//! The enums here deliberately ship with ONLY the variants the evaluator
//! currently emits — [`Action::AllowAnonymous`] and [`Action::Continue`],
//! and a single [`Match::PublicPrefixGrant`] predicate. Future admission
//! features (IP denylist, maintenance mode, sliding-window rate limiting,
//! explicit deny, custom reject status) will add variants in later phases.
//! When they do, new variants are additive: [`Action`]'s `#[serde(tag =
//! "action")]` rename-all-kebab-case layout means a YAML
//! `action: allow-anonymous` survives intact across the enum growing.
//!
//! Callers that `match` on these enums should include a wildcard arm —
//! today that's unreachable, tomorrow it's the path the new variants land
//! on before the evaluator learns about them.
//!
//! # Design invariants
//!
//! - The module does not depend on `crate::api::auth` — admission must not
//!   reach into the SigV4 code, and vice versa beyond an agreed-upon
//!   request-extension marker. Keeping this separation makes it possible
//!   to unit test admission without a full axum request pipeline.
//! - Chain lookups are lock-free at read time via [`arc_swap::ArcSwap`],
//!   matching the hot-swap pattern already in use for the public-prefix
//!   snapshot.
//! - The chain is rebuilt from scratch on every config change rather than
//!   mutated in place. Building is cheap relative to the lifetime of a
//!   config, and it avoids the entire class of partial-update bugs.

use crate::bucket_policy::PublicPrefixSnapshot;
use serde::{Deserialize, Serialize};

pub mod evaluator;
pub mod middleware;

pub use evaluator::{evaluate, RequestInfo};
pub use middleware::{admission_middleware, AdmissionAllowAnonymous};

/// Ordered list of admission blocks plus the snapshot needed to evaluate
/// public-prefix matches. The order matters — the evaluator returns on the
/// first match (RRR semantics).
///
/// An `AdmissionChain` is immutable once built; runtime updates swap a whole
/// new chain via the shared [`ArcSwap`](arc_swap::ArcSwap) on `AdminState`.
#[derive(Debug, Clone, Default)]
pub struct AdmissionChain {
    blocks: Vec<AdmissionBlock>,
    /// Snapshot of public-prefix matches. Kept on the chain (rather than
    /// consulted separately) so the evaluator has everything it needs in
    /// one place — useful for unit tests that want to construct a chain
    /// without going through live config.
    public_prefixes: std::sync::Arc<PublicPrefixSnapshot>,
}

/// One admission rule. Stable shape across phases.
#[derive(Debug, Clone)]
pub struct AdmissionBlock {
    /// Human-readable identifier used by the trace endpoint and logs.
    /// Derived from bucket config for synthesized blocks (e.g.
    /// `"public-prefix:my-bucket"`), or operator-supplied in later phases.
    pub name: String,
    /// Predicate the block fires on.
    pub match_: Match,
    /// What to do when the predicate fires.
    pub action: Action,
}

/// Predicate side of an admission block. Phase 2 ships exactly one
/// variant. Future variants (IP denylist, method/path globs, maintenance
/// mode) are additive — when they land, existing match sites should pick
/// them up via a wildcard arm, so the evaluator can evolve without
/// retrofitting every caller.
#[derive(Debug, Clone)]
pub enum Match {
    /// "Does this request target a publicly-readable location on the named
    /// bucket?" The bucket name is lowercased on construction. Both object
    /// GET/HEAD and bucket LIST variants are covered by the underlying
    /// [`PublicPrefixSnapshot`] — the admission chain delegates to that
    /// data structure so the overlap semantics stay in one place.
    PublicPrefixGrant { bucket: String },
}

/// Decision side of an admission block. Phase 2 only emits
/// [`Action::AllowAnonymous`] (from synthesised public-prefix blocks) and
/// [`Action::Continue`] (explicit fall-through, or the implicit default
/// when nothing matched).
///
/// Future phases will add operator-authored variants — `deny`,
/// `rate-limit`, `reject` with a custom status — at which point the
/// evaluator grows arms for them. The serde attributes here (`tag =
/// "action"`, `rename_all = "kebab-case"`) are chosen so that YAML like
/// `action: allow-anonymous` deserialises the same way before and after
/// the enum grows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "action")]
pub enum Action {
    /// Pre-admit the request as anonymous. SigV4 verification is skipped;
    /// the request continues as the `$anonymous` principal with exactly
    /// the scoped permissions that justified the match.
    AllowAnonymous,
    /// Fall through to authentication. This is the default terminal
    /// action and covers the common case of "no special admission rule
    /// fired — let SigV4 decide".
    Continue,
}

/// Result of evaluating the chain against a request. The evaluator always
/// produces a decision; `Continue { matched: None }` is the implicit
/// default when no block fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "decision")]
pub enum Decision {
    /// The request was pre-admitted as anonymous by a specific block.
    AllowAnonymous {
        /// Name of the block that matched, surfaced in trace output.
        matched: String,
    },
    /// The request should proceed to authentication. `matched` is `None`
    /// when the default-terminal case applied and `Some(name)` when an
    /// operator-defined `Continue` block fired (Phase 3+).
    Continue {
        #[serde(skip_serializing_if = "Option::is_none")]
        matched: Option<String>,
    },
}

impl AdmissionChain {
    /// Build a chain from live bucket policies. Each bucket with at least
    /// one public prefix gets a synthesised [`Match::PublicPrefixGrant`]
    /// block with [`Action::AllowAnonymous`]. Buckets without public
    /// prefixes are not represented.
    ///
    /// Block ordering is deterministic (sorted by bucket name) so that
    /// trace output and audit logs don't depend on `BTreeMap` insertion
    /// order — they already are, since the field is `BTreeMap`, but the
    /// sort makes the property explicit.
    pub fn from_bucket_config(
        buckets: &std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
    ) -> Self {
        let snapshot = PublicPrefixSnapshot::from_config(buckets);
        let mut blocks: Vec<AdmissionBlock> = buckets
            .iter()
            .filter(|(_, policy)| !policy.public_prefixes.is_empty())
            .map(|(name, _)| AdmissionBlock {
                name: format!("public-prefix:{}", name.to_ascii_lowercase()),
                match_: Match::PublicPrefixGrant {
                    bucket: name.to_ascii_lowercase(),
                },
                action: Action::AllowAnonymous,
            })
            .collect();
        // BTreeMap already yields sorted keys; the sort is a belt-and-braces
        // guarantee against a future change to the map type.
        blocks.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            blocks,
            public_prefixes: std::sync::Arc::new(snapshot),
        }
    }

    /// Access the block list (read-only). Exposed for the trace endpoint
    /// and for unit tests that want to inspect what was synthesised.
    pub fn blocks(&self) -> &[AdmissionBlock] {
        &self.blocks
    }

    /// Access the public-prefix snapshot underlying the chain. Exposed
    /// primarily so the evaluator can do the overlap check without a
    /// separate lookup path.
    pub fn public_prefixes(&self) -> &std::sync::Arc<PublicPrefixSnapshot> {
        &self.public_prefixes
    }
}

/// Hot-swappable shared handle. Readers clone the inner `Arc` lock-free
/// via [`ArcSwap::load_full`](arc_swap::ArcSwap::load_full); writers replace
/// the whole chain via `store()` on config change.
pub type SharedAdmissionChain = std::sync::Arc<arc_swap::ArcSwap<AdmissionChain>>;

/// Build a [`SharedAdmissionChain`] from a bucket-config map. Convenience
/// wrapper used at startup and on every hot-reload site.
pub fn build_shared_chain(
    buckets: &std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
) -> SharedAdmissionChain {
    std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(
        AdmissionChain::from_bucket_config(buckets),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket_policy::BucketPolicyConfig;
    use std::collections::BTreeMap;

    fn with_public(prefixes: &[&str]) -> BucketPolicyConfig {
        BucketPolicyConfig {
            public_prefixes: prefixes.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn from_bucket_config_empty_yields_empty_chain() {
        let chain = AdmissionChain::from_bucket_config(&BTreeMap::new());
        assert!(chain.blocks().is_empty());
        assert!(chain.public_prefixes().is_empty());
    }

    #[test]
    fn from_bucket_config_skips_buckets_with_no_public_prefixes() {
        let mut cfg = BTreeMap::new();
        cfg.insert("private".to_string(), BucketPolicyConfig::default());
        cfg.insert("semi-public".to_string(), with_public(&["releases/"]));
        let chain = AdmissionChain::from_bucket_config(&cfg);
        assert_eq!(chain.blocks().len(), 1);
        assert_eq!(chain.blocks()[0].name, "public-prefix:semi-public");
    }

    #[test]
    fn from_bucket_config_produces_sorted_block_order() {
        let mut cfg = BTreeMap::new();
        cfg.insert("zeta".to_string(), with_public(&["z/"]));
        cfg.insert("alpha".to_string(), with_public(&["a/"]));
        cfg.insert("mu".to_string(), with_public(&["m/"]));
        let chain = AdmissionChain::from_bucket_config(&cfg);
        let names: Vec<&str> = chain.blocks().iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "public-prefix:alpha",
                "public-prefix:mu",
                "public-prefix:zeta",
            ]
        );
    }

    #[test]
    fn from_bucket_config_lowercases_bucket_in_match() {
        let mut cfg = BTreeMap::new();
        cfg.insert("MixedCase".to_string(), with_public(&["x/"]));
        let chain = AdmissionChain::from_bucket_config(&cfg);
        assert_eq!(chain.blocks().len(), 1);
        match &chain.blocks()[0].match_ {
            Match::PublicPrefixGrant { bucket } => {
                assert_eq!(bucket, "mixedcase");
            }
        }
    }
}
