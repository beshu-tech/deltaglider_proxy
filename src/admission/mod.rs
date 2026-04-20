//! Admission chain — pre-auth request gating (Phase 2).
//!
//! The admission chain runs before SigV4 verification and decides whether a
//! request should proceed as anonymous, continue to authentication, or be
//! rejected outright. It is the first of the five request-processing layers
//! (admission → identity → IAM → parameters → routing) in the configuration
//! architecture.
//!
//! # Runtime shape (Phase 3b.2.b)
//!
//! - Synthesised blocks from bucket `public_prefixes` — the Phase 2
//!   baseline — still run, now AFTER operator-authored blocks.
//! - Operator-authored blocks (parsed from `admission.blocks:` in
//!   sectioned YAML) compile to [`Match::Predicates`] +
//!   [`Action::AllowAnonymous`] / [`Action::Deny`] / [`Action::Reject`] /
//!   [`Action::Continue`]. The middleware returns `403` (S3-style XML)
//!   for Deny and the operator's status+body for Reject — both
//!   short-circuiting SigV4.
//!
//! # Still deferred
//!
//! - `RateLimit` action variant — Phase 3b.2.c, hooking the existing
//!   `RateLimiter`.
//! - `config_flag` predicate dispatch — today always evaluates false
//!   with a compile-time warn; Phase 3b.2.c adds a flag registry
//!   starting with `maintenance_mode`.
//! - The 5-block default chain from the plan's original Phase 2 scope
//!   (`deny-known-bad-ips`, `rate-limit-*`, etc.) — lands in 3b.2.c
//!   once the remaining variants exist.
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
pub mod spec;

pub use evaluator::{evaluate, RequestInfo};
pub use middleware::{admission_middleware, AdmissionAllowAnonymous};
pub use spec::{AdmissionBlockSpec, AdmissionSpec, MatchSpec};

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

/// Predicate side of an admission block. Two variants today:
///
/// - [`Match::PublicPrefixGrant`] — synthesised from bucket policies
///   (Phase 2 behavior, unchanged).
/// - [`Match::Predicates`] — operator-authored compound predicate, the
///   AND of every `Some(_)` field. Phase 3b.2.b compiles
///   [`crate::admission::MatchSpec`] into this at chain-build time.
///
/// The evaluator dispatches both via explicit arms — no wildcard,
/// because adding variants should force reviewers to look at every
/// matching site.
#[derive(Debug, Clone)]
pub enum Match {
    /// "Does this request target a publicly-readable location on the named
    /// bucket?" The bucket name is lowercased on construction. Both object
    /// GET/HEAD and bucket LIST variants are covered by the underlying
    /// [`PublicPrefixSnapshot`] — the admission chain delegates to that
    /// data structure so the overlap semantics stay in one place.
    PublicPrefixGrant { bucket: String },

    /// Compound predicate authored by the operator. All populated
    /// fields are AND'd together — the block fires only when every
    /// predicate matches the request.
    Predicates(Predicates),
}

/// Compiled form of [`crate::admission::MatchSpec`]. Immutable after
/// chain build; the evaluator only reads.
#[derive(Debug, Clone, Default)]
pub struct Predicates {
    /// Allowed HTTP methods (uppercased). `None` = any method.
    pub methods: Option<Vec<String>>,
    /// Networks the source IP must fall within. `None` = any IP.
    /// Bare IPs on the wire were promoted to `/32` / `/128` during
    /// parse, so the runtime uniformly treats everything as a CIDR.
    pub source_networks: Option<Vec<ipnet::IpNet>>,
    /// Bucket the request targets (lowercased). `None` = any bucket.
    pub bucket: Option<String>,
    /// Glob-style pattern matched against the full object key.
    /// Pre-compiled at chain-build time.
    pub path_glob: Option<globset::GlobMatcher>,
    /// Fire only on authenticated / unauthenticated requests. `None`
    /// = either.
    pub authenticated: Option<bool>,
    /// Reserved for a future `config_flag` predicate — Phase 3b.2.b
    /// carries the name but always evaluates false until a flag
    /// registry exists. Warn is logged at build time.
    pub config_flag: Option<String>,
}

/// Decision side of an admission block.
///
/// - [`Action::AllowAnonymous`] / [`Action::Continue`] — Phase 2
///   behaviour, unchanged.
/// - [`Action::Deny`] — new in Phase 3b.2.b. Short-circuits the
///   middleware with 403 Forbidden, no SigV4 verification.
/// - [`Action::Reject`] — new in Phase 3b.2.b. Short-circuits with a
///   custom 4xx/5xx status and optional body. Intended for
///   maintenance pages and rate-exceed responses.
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
    /// Short-circuit with 403 Forbidden. No SigV4 verification runs.
    Deny,
    /// Short-circuit with a custom HTTP status + body. Status must be
    /// 4xx or 5xx (validated at parse time in
    /// [`crate::admission::ActionSpec::validate`]).
    Reject {
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
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
    /// operator-defined `Continue` block fired.
    Continue {
        #[serde(skip_serializing_if = "Option::is_none")]
        matched: Option<String>,
    },
    /// The request is denied — middleware returns 403 without calling
    /// SigV4. `matched` names the block for logs/trace output.
    Deny { matched: String },
    /// The request is rejected with a custom HTTP status + optional
    /// body. Middleware returns this directly.
    Reject {
        matched: String,
        status: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
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
        Self::from_config_parts(buckets, &[])
    }

    /// Build a chain from live bucket policies AND operator-authored
    /// admission blocks (Phase 3b.2.b live dispatch).
    ///
    /// Order semantics:
    ///
    /// 1. Operator-authored blocks fire FIRST (RRR order within the
    ///    operator's list).
    /// 2. Synthesised `public-prefix:*` blocks follow.
    ///
    /// This ordering is deliberate: operator-authored `deny` rules
    /// (e.g. known-bad IP blocklists) must short-circuit before any
    /// `allow-anonymous` grant; operators who want public access to
    /// override a deny can move their allow block further down or
    /// express the intent directly.
    ///
    /// Blocks whose compilation fails (bad glob, unknown config_flag)
    /// are SKIPPED with a `tracing::warn!` naming the block; the rest
    /// of the chain still loads. Rationale: a single typo in one block
    /// should not break the entire chain and take the server down —
    /// validation already runs at config load time, so getting here
    /// with a compile error is a rare edge case.
    pub fn from_config_parts(
        buckets: &std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
        operator_blocks: &[crate::admission::AdmissionBlockSpec],
    ) -> Self {
        let snapshot = PublicPrefixSnapshot::from_config(buckets);

        // 1. Operator-authored blocks (compile → runtime form).
        let mut blocks: Vec<AdmissionBlock> = operator_blocks
            .iter()
            .filter_map(|spec| match compile_block(spec) {
                Ok(block) => Some(block),
                Err(e) => {
                    tracing::warn!(
                        target: "deltaglider_proxy::admission",
                        block = %spec.name,
                        error = %e,
                        "[admission] skipping block `{}` — compile error: {}",
                        spec.name,
                        e
                    );
                    None
                }
            })
            .collect();

        // 2. Synthesised public-prefix blocks, sorted by bucket name for
        //    deterministic trace output.
        let mut synthesised: Vec<AdmissionBlock> = buckets
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
        synthesised.sort_by(|a, b| a.name.cmp(&b.name));
        blocks.extend(synthesised);

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

/// Compile an [`crate::admission::AdmissionBlockSpec`] into its runtime
/// form. Returns an error when a glob or config_flag is malformed —
/// those are reported but don't kill the whole chain (see
/// [`AdmissionChain::from_config_parts`]).
fn compile_block(spec: &crate::admission::AdmissionBlockSpec) -> Result<AdmissionBlock, String> {
    use crate::admission::spec as specmod;

    let predicates = Predicates {
        methods: spec
            .match_
            .method
            .as_ref()
            .map(|ms| ms.iter().map(|m| m.to_ascii_uppercase()).collect()),
        source_networks: {
            // The spec supports EITHER `source_ip` (single) OR
            // `source_ip_list` (many). Validation already ran at
            // config-load time, so we assume at most one is set. If both
            // somehow arrive here, prefer the list form and warn.
            match (&spec.match_.source_ip, &spec.match_.source_ip_list) {
                (Some(_), Some(_)) => {
                    tracing::warn!(
                        target: "deltaglider_proxy::admission",
                        block = %spec.name,
                        "[admission] block `{}` has both source_ip and source_ip_list — \
                         using source_ip_list (validation should have caught this at \
                         config load)",
                        spec.name
                    );
                    Some(
                        spec.match_
                            .source_ip_list
                            .as_ref()
                            .unwrap()
                            .iter()
                            .map(|e| e.net)
                            .collect(),
                    )
                }
                (Some(addr), None) => {
                    let net = match addr {
                        std::net::IpAddr::V4(v4) => {
                            ipnet::IpNet::V4(ipnet::Ipv4Net::new(*v4, 32).unwrap())
                        }
                        std::net::IpAddr::V6(v6) => {
                            ipnet::IpNet::V6(ipnet::Ipv6Net::new(*v6, 128).unwrap())
                        }
                    };
                    Some(vec![net])
                }
                (None, Some(list)) => Some(list.iter().map(|e| e.net).collect()),
                (None, None) => None,
            }
        },
        bucket: spec.match_.bucket.as_ref().map(|b| b.to_ascii_lowercase()),
        path_glob: match &spec.match_.path_glob {
            Some(glob_str) => Some(
                globset::Glob::new(glob_str)
                    .map_err(|e| format!("invalid path_glob `{}`: {}", glob_str, e))?
                    .compile_matcher(),
            ),
            None => None,
        },
        authenticated: spec.match_.authenticated,
        config_flag: spec.match_.config_flag.clone(),
    };

    let action = match &spec.action {
        specmod::ActionSpec::Simple(specmod::SimpleAction::AllowAnonymous) => {
            Action::AllowAnonymous
        }
        specmod::ActionSpec::Simple(specmod::SimpleAction::Deny) => Action::Deny,
        specmod::ActionSpec::Simple(specmod::SimpleAction::Continue) => Action::Continue,
        specmod::ActionSpec::Tagged(specmod::TaggedAction::Reject { status, message }) => {
            Action::Reject {
                status: *status,
                message: message.clone(),
            }
        }
    };

    // Warn on unknown config_flag — validates Phase 3b.2.b's remaining
    // schema gap (adversarial review M1). Phase 3b.2.c will maintain
    // a real flag registry.
    if let Some(flag) = &predicates.config_flag {
        if flag != "maintenance_mode" {
            tracing::warn!(
                target: "deltaglider_proxy::admission",
                block = %spec.name,
                config_flag = %flag,
                "[admission] block `{}` references unknown config_flag `{}` — the predicate \
                 will always evaluate false. Currently known flags: [maintenance_mode].",
                spec.name,
                flag
            );
        }
    }

    Ok(AdmissionBlock {
        name: spec.name.clone(),
        match_: Match::Predicates(predicates),
        action,
    })
}

/// Hot-swappable shared handle. Readers clone the inner `Arc` lock-free
/// via [`ArcSwap::load_full`](arc_swap::ArcSwap::load_full); writers replace
/// the whole chain via `store()` on config change.
pub type SharedAdmissionChain = std::sync::Arc<arc_swap::ArcSwap<AdmissionChain>>;

/// Build a [`SharedAdmissionChain`] from a bucket-config map. Convenience
/// wrapper used at startup and on every hot-reload site.
///
/// Deprecated in favor of [`build_shared_chain_from_parts`], which
/// accepts operator-authored blocks too. Kept for any callers that
/// predate Phase 3b.2.a.
pub fn build_shared_chain(
    buckets: &std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
) -> SharedAdmissionChain {
    build_shared_chain_from_parts(buckets, &[])
}

/// Build a [`SharedAdmissionChain`] from bucket policies AND
/// operator-authored admission blocks. See [`AdmissionChain::from_config_parts`]
/// for the Phase 3b.2.a behavior (warn-and-ignore until 3b.2.b lands).
pub fn build_shared_chain_from_parts(
    buckets: &std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
    operator_blocks: &[crate::admission::AdmissionBlockSpec],
) -> SharedAdmissionChain {
    std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(
        AdmissionChain::from_config_parts(buckets, operator_blocks),
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
            Match::Predicates(_) => panic!("expected PublicPrefixGrant, got Predicates"),
        }
    }
}
