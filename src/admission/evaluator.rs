//! Admission evaluator — take a normalised request description, walk the
//! chain, return a decision.
//!
//! The evaluator is a pure function over [`RequestInfo`] and
//! [`AdmissionChain`]. It takes no locks and does no I/O. This is
//! deliberate: the same routine backs the live request-path middleware
//! and the admin `/config/trace` endpoint, and pureness is what lets us
//! test both surfaces with the same code.

use super::{Action, AdmissionChain, Decision, Match, Predicates};

/// Everything the evaluator needs to know about a request. Extracted by
/// the middleware from `axum::http::Request` (for the live path) or from
/// an admin-API payload (for `/config/trace`).
///
/// Field conventions:
/// - `bucket` is always lowercase (S3 bucket names are case-insensitive).
/// - `key` is the percent-decoded object key for GET/HEAD on an object,
///   or `None` for a bucket-level LIST request.
/// - `list_prefix` is the effective `prefix` query parameter on a LIST
///   request (`None` when no `?prefix=` was provided). Ignored when
///   `key.is_some()`.
/// - `authenticated` is `true` when the request carried SigV4 credentials
///   (header or presigned). The evaluator uses this to short-circuit:
///   authenticated requests skip the public-prefix path today because the
///   caller chose to sign — public-prefix grants are for unauthenticated
///   traffic.
/// - `source_ip` is the peer IP from axum `ConnectInfo`. `None` when the
///   middleware couldn't determine it (synthetic trace inputs, unit
///   tests) — operator-authored `source_ip` / `source_ip_list` predicates
///   evaluate false when the IP is unknown, by design: we'd rather
///   fail-closed on a deny rule than leak through on missing data.
#[derive(Debug, Clone)]
pub struct RequestInfo<'a> {
    pub method: &'a str,
    pub bucket: &'a str,
    pub key: Option<&'a str>,
    pub list_prefix: Option<&'a str>,
    pub authenticated: bool,
    pub source_ip: Option<std::net::IpAddr>,
}

impl<'a> RequestInfo<'a> {
    fn is_read_method(&self) -> bool {
        matches!(self.method, "GET" | "HEAD")
    }
}

/// Walk the chain, return the first matched decision. If no block fires,
/// the default terminal is `Continue { matched: None }`.
///
/// Ordering invariant: admission chain evaluation is RRR-style — first
/// match wins. Callers must not assume the order of blocks; construction
/// sites are responsible for the ordering they want to express.
pub fn evaluate(chain: &AdmissionChain, req: &RequestInfo<'_>) -> Decision {
    for block in chain.blocks() {
        if matches(chain, &block.match_, &block.action, req) {
            return match &block.action {
                Action::AllowAnonymous => Decision::AllowAnonymous {
                    matched: block.name.clone(),
                },
                Action::Continue => Decision::Continue {
                    matched: Some(block.name.clone()),
                },
                Action::Deny => Decision::Deny {
                    matched: block.name.clone(),
                },
                Action::Reject { status, message } => Decision::Reject {
                    matched: block.name.clone(),
                    status: *status,
                    message: message.clone(),
                },
            };
        }
    }
    Decision::Continue { matched: None }
}

/// Predicate dispatch. New `Match` variants must add a branch here; the
/// wildcard is omitted intentionally so the compiler forces an update
/// when variants grow. The block's `action` is threaded through so
/// `match_predicates` can apply action-specific defaults (e.g. default
/// `authenticated=false` for `allow-anonymous` blocks that didn't
/// explicitly say so — see [`match_predicates`]).
fn matches(chain: &AdmissionChain, m: &Match, action: &Action, req: &RequestInfo<'_>) -> bool {
    match m {
        Match::PublicPrefixGrant { bucket } => match_public_prefix_grant(chain, bucket, req),
        Match::Predicates(p) => match_predicates(p, action, req),
    }
}

/// AND of every populated predicate. An empty `Predicates` matches every
/// request (operator-authored terminal fallback). Unset fields are
/// treated as "don't care" — the symmetry with serde's Option makes the
/// YAML and the runtime semantics agree without a translation layer.
///
/// Action-specific default: `AllowAnonymous` blocks without an explicit
/// `authenticated` predicate default to `authenticated == false`. An
/// operator authoring `allow-anonymous` is giving a carve-out for
/// unsigned requests; letting it match signed requests would silently
/// discard the caller's credentials and downgrade them to the narrower
/// `$anonymous` principal (which often has no permissions, resulting
/// in an unexpected 403). Signed callers should always flow through
/// SigV4 verification — they *chose* to sign.
fn match_predicates(p: &Predicates, action: &Action, req: &RequestInfo<'_>) -> bool {
    if let Some(methods) = &p.methods {
        let m_upper = req.method.to_ascii_uppercase();
        if !methods.iter().any(|m| m == &m_upper) {
            return false;
        }
    }
    if let Some(nets) = &p.source_networks {
        // Source-IP predicate present but no IP on the request: fail
        // closed. A `deny` rule must NOT leak through on missing data;
        // an `allow-anonymous` rule simply won't match, forcing the
        // request down the normal auth path.
        let Some(ip) = req.source_ip else {
            return false;
        };
        if !nets.iter().any(|n| n.contains(&ip)) {
            return false;
        }
    }
    if let Some(bucket) = &p.bucket {
        if bucket != req.bucket {
            return false;
        }
    }
    if let Some(glob) = &p.path_glob {
        // Match against the key for object ops, or the list prefix for
        // bucket LIST. Missing both = match against empty string so
        // path_glob: "*" still fires on a bare bucket LIST.
        let target = req.key.or(req.list_prefix).unwrap_or("");
        if !glob.is_match(target) {
            return false;
        }
    }
    // `authenticated` predicate. If the operator set it explicitly,
    // compare as-is. Otherwise, for `allow-anonymous` blocks, default
    // to "unauthenticated only" — signed callers should flow through
    // SigV4, not get their principal silently replaced.
    match p.authenticated {
        Some(required) => {
            if required != req.authenticated {
                return false;
            }
        }
        None => {
            if matches!(action, Action::AllowAnonymous) && req.authenticated {
                return false;
            }
        }
    }
    if p.config_flag.is_some() {
        // No flag registry exists yet — every config_flag predicate
        // fails closed. The chain builder emits a `tracing::warn!`
        // per block so operators see the gap. Live dispatch lands
        // with the Phase 3b.2.c rate-limit work.
        return false;
    }
    true
}

/// The single admission predicate currently implemented. Conditions for a
/// `PublicPrefixGrant` to fire:
///
/// - Request method is GET or HEAD (public-prefix grants are read-only).
/// - Request is unauthenticated (a signed request knows what it's doing;
///   the admin API elsewhere enforces that IAM permissions be at least
///   as broad as public-prefix grants for specific operators).
/// - The bucket in the request matches the bucket named by the block,
///   case-insensitively.
/// - Either:
///   - `key.is_some()` and the key starts with one of the bucket's
///     configured public prefixes (object GET/HEAD), OR
///   - `key.is_none()` and the LIST prefix overlaps a public prefix in
///     either direction (the existing `list_overlaps_public` semantics).
fn match_public_prefix_grant(
    chain: &AdmissionChain,
    block_bucket: &str,
    req: &RequestInfo<'_>,
) -> bool {
    if !req.is_read_method() || req.authenticated {
        return false;
    }
    if req.bucket != block_bucket {
        return false;
    }
    let snapshot = chain.public_prefixes();
    match req.key {
        Some(key) => snapshot.is_public_read(block_bucket, key),
        None => {
            let prefix = req.list_prefix.unwrap_or("");
            snapshot.list_overlaps_public(block_bucket, prefix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket_policy::BucketPolicyConfig;
    use std::collections::BTreeMap;

    fn chain_with(bucket: &str, prefixes: &[&str]) -> AdmissionChain {
        let mut cfg = BTreeMap::new();
        cfg.insert(
            bucket.to_string(),
            BucketPolicyConfig {
                public_prefixes: prefixes.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        );
        AdmissionChain::from_bucket_config(&cfg)
    }

    #[test]
    fn evaluator_allows_anonymous_get_on_public_prefix() {
        let chain = chain_with("my-bucket", &["releases/"]);
        let req = RequestInfo {
            method: "GET",
            bucket: "my-bucket",
            key: Some("releases/v1.zip"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        let decision = evaluate(&chain, &req);
        assert_eq!(
            decision,
            Decision::AllowAnonymous {
                matched: "public-prefix:my-bucket".into(),
            }
        );
    }

    #[test]
    fn evaluator_allows_anonymous_head_on_public_prefix() {
        let chain = chain_with("my-bucket", &["releases/"]);
        let req = RequestInfo {
            method: "HEAD",
            bucket: "my-bucket",
            key: Some("releases/v1.zip"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert!(matches!(
            evaluate(&chain, &req),
            Decision::AllowAnonymous { .. }
        ));
    }

    #[test]
    fn evaluator_continues_on_non_public_key() {
        let chain = chain_with("my-bucket", &["releases/"]);
        let req = RequestInfo {
            method: "GET",
            bucket: "my-bucket",
            key: Some("private/secret.txt"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert_eq!(evaluate(&chain, &req), Decision::Continue { matched: None });
    }

    #[test]
    fn evaluator_continues_on_put_even_inside_public_prefix() {
        let chain = chain_with("my-bucket", &["releases/"]);
        let req = RequestInfo {
            method: "PUT",
            bucket: "my-bucket",
            key: Some("releases/v1.zip"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        // Write methods never ride the public-prefix grant — they must sign.
        assert_eq!(evaluate(&chain, &req), Decision::Continue { matched: None });
    }

    #[test]
    fn evaluator_continues_for_authenticated_requests() {
        let chain = chain_with("my-bucket", &["releases/"]);
        let req = RequestInfo {
            method: "GET",
            bucket: "my-bucket",
            key: Some("releases/v1.zip"),
            list_prefix: None,
            authenticated: true,
            source_ip: None,
        };
        // Even though the key matches, authenticated traffic goes through
        // SigV4; admission doesn't short-circuit it.
        assert_eq!(evaluate(&chain, &req), Decision::Continue { matched: None });
    }

    #[test]
    fn evaluator_allows_list_when_prefix_overlaps_public_range() {
        let chain = chain_with("my-bucket", &["releases/"]);

        // Narrower-than-public prefix still overlaps.
        let req = RequestInfo {
            method: "GET",
            bucket: "my-bucket",
            key: None,
            list_prefix: Some("releases/v2/"),
            authenticated: false,
            source_ip: None,
        };
        assert!(matches!(
            evaluate(&chain, &req),
            Decision::AllowAnonymous { .. }
        ));

        // Wider-than-public (empty prefix) also overlaps — listing the whole
        // bucket anonymously must be scoped by IAM to the public slice, but
        // admission lets it through.
        let req = RequestInfo {
            method: "GET",
            bucket: "my-bucket",
            key: None,
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert!(matches!(
            evaluate(&chain, &req),
            Decision::AllowAnonymous { .. }
        ));
    }

    #[test]
    fn evaluator_rejects_list_with_disjoint_prefix() {
        let chain = chain_with("my-bucket", &["releases/"]);
        let req = RequestInfo {
            method: "GET",
            bucket: "my-bucket",
            key: None,
            list_prefix: Some("secrets/"),
            authenticated: false,
            source_ip: None,
        };
        assert_eq!(evaluate(&chain, &req), Decision::Continue { matched: None });
    }

    #[test]
    fn evaluator_is_insensitive_to_bucket_mismatch() {
        let chain = chain_with("my-bucket", &["releases/"]);
        let req = RequestInfo {
            method: "GET",
            bucket: "other-bucket",
            key: Some("releases/v1.zip"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert_eq!(evaluate(&chain, &req), Decision::Continue { matched: None });
    }

    // ── Phase 3b.2.b: operator-authored block dispatch ─────────────────

    fn chain_from_spec(blocks: Vec<crate::admission::AdmissionBlockSpec>) -> AdmissionChain {
        AdmissionChain::from_config_parts(&BTreeMap::new(), &blocks)
    }

    #[test]
    fn evaluator_denies_request_from_blocked_ip() {
        use crate::admission::spec::{
            ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction, SourceIpEntry,
        };
        let block = AdmissionBlockSpec {
            name: "deny-bad-ips".into(),
            match_: MatchSpec {
                source_ip_list: Some(vec![SourceIpEntry::from_net(
                    "203.0.113.0/24".parse().unwrap(),
                )]),
                ..Default::default()
            },
            action: ActionSpec::Simple(SimpleAction::Deny),
        };
        let chain = chain_from_spec(vec![block]);
        let req = RequestInfo {
            method: "GET",
            bucket: "my-bucket",
            key: Some("file"),
            list_prefix: None,
            authenticated: false,
            source_ip: Some("203.0.113.42".parse().unwrap()),
        };
        assert_eq!(
            evaluate(&chain, &req),
            Decision::Deny {
                matched: "deny-bad-ips".into()
            }
        );
    }

    #[test]
    fn evaluator_source_ip_predicate_fails_closed_when_ip_unknown() {
        // When no source IP is available (missing ConnectInfo, untrusted
        // proxy), deny rules must NOT leak through. Evaluator returns the
        // default terminal Continue.
        use crate::admission::spec::{
            ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction, SourceIpEntry,
        };
        let block = AdmissionBlockSpec {
            name: "deny-bad-ips".into(),
            match_: MatchSpec {
                source_ip_list: Some(vec![SourceIpEntry::from_net(
                    "203.0.113.0/24".parse().unwrap(),
                )]),
                ..Default::default()
            },
            action: ActionSpec::Simple(SimpleAction::Deny),
        };
        let chain = chain_from_spec(vec![block]);
        let req = RequestInfo {
            method: "GET",
            bucket: "my-bucket",
            key: Some("file"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        // Fails closed: predicate doesn't match, so block doesn't fire,
        // so decision is default-terminal Continue.
        assert_eq!(evaluate(&chain, &req), Decision::Continue { matched: None });
    }

    #[test]
    fn evaluator_rejects_with_custom_status() {
        use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, TaggedAction};
        let block = AdmissionBlockSpec {
            name: "maint".into(),
            match_: MatchSpec::default(),
            action: ActionSpec::Tagged(TaggedAction::Reject {
                status: 503,
                message: Some("back soon".into()),
            }),
        };
        let chain = chain_from_spec(vec![block]);
        let req = RequestInfo {
            method: "GET",
            bucket: "any",
            key: Some("any"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert_eq!(
            evaluate(&chain, &req),
            Decision::Reject {
                matched: "maint".into(),
                status: 503,
                message: Some("back soon".into()),
            }
        );
    }

    #[test]
    fn evaluator_path_glob_matches_and_passes_through() {
        use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction};
        let block = AdmissionBlockSpec {
            name: "allow-zips".into(),
            match_: MatchSpec {
                path_glob: Some("*.zip".into()),
                bucket: Some("releases".into()),
                method: Some(vec!["GET".into(), "HEAD".into()]),
                ..Default::default()
            },
            action: ActionSpec::Simple(SimpleAction::AllowAnonymous),
        };
        let chain = chain_from_spec(vec![block]);

        // Matches: .zip + releases + GET.
        let req = RequestInfo {
            method: "GET",
            bucket: "releases",
            key: Some("v1.zip"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert_eq!(
            evaluate(&chain, &req),
            Decision::AllowAnonymous {
                matched: "allow-zips".into(),
            }
        );

        // Doesn't match: wrong extension.
        let req2 = RequestInfo {
            method: "GET",
            bucket: "releases",
            key: Some("v1.tar.gz"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert_eq!(
            evaluate(&chain, &req2),
            Decision::Continue { matched: None }
        );
    }

    #[test]
    fn evaluator_operator_deny_wins_over_synthesised_public_prefix() {
        // Authored blocks run BEFORE synthesised public-prefix blocks.
        // An operator-authored deny for a specific IP range must
        // short-circuit even if the bucket is otherwise publicly
        // readable.
        use crate::admission::spec::{
            ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction, SourceIpEntry,
        };
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "public-bucket".to_string(),
            crate::bucket_policy::BucketPolicyConfig {
                public_prefixes: vec!["".into()], // entire bucket
                ..Default::default()
            },
        );
        let deny_block = AdmissionBlockSpec {
            name: "deny-tor".into(),
            match_: MatchSpec {
                source_ip_list: Some(vec![SourceIpEntry::from_net(
                    "203.0.113.0/24".parse().unwrap(),
                )]),
                ..Default::default()
            },
            action: ActionSpec::Simple(SimpleAction::Deny),
        };
        let chain = AdmissionChain::from_config_parts(&cfg, &[deny_block]);

        // Request from the blocked range — even though the bucket is
        // public, deny wins.
        let req = RequestInfo {
            method: "GET",
            bucket: "public-bucket",
            key: Some("anything"),
            list_prefix: None,
            authenticated: false,
            source_ip: Some("203.0.113.9".parse().unwrap()),
        };
        assert_eq!(
            evaluate(&chain, &req),
            Decision::Deny {
                matched: "deny-tor".into(),
            }
        );

        // Request from outside the blocked range — public-prefix grant
        // takes over.
        let req2 = RequestInfo {
            method: "GET",
            bucket: "public-bucket",
            key: Some("anything"),
            list_prefix: None,
            authenticated: false,
            source_ip: Some("198.51.100.5".parse().unwrap()),
        };
        assert!(matches!(
            evaluate(&chain, &req2),
            Decision::AllowAnonymous { .. }
        ));
    }

    #[test]
    fn evaluator_config_flag_predicate_fails_closed_without_flag_registry() {
        // No flag registry exists yet (lands with Phase 3b.2.c).
        // The predicate evaluates false, so the block never fires.
        use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, TaggedAction};
        let block = AdmissionBlockSpec {
            name: "maint".into(),
            match_: MatchSpec {
                config_flag: Some("maintenance_mode".into()),
                ..Default::default()
            },
            action: ActionSpec::Tagged(TaggedAction::Reject {
                status: 503,
                message: None,
            }),
        };
        let chain = chain_from_spec(vec![block]);
        let req = RequestInfo {
            method: "GET",
            bucket: "any",
            key: Some("any"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert_eq!(evaluate(&chain, &req), Decision::Continue { matched: None });
    }

    #[test]
    fn evaluator_allow_anonymous_does_not_downgrade_signed_request() {
        // Regression guard for the "allow-anonymous block silently
        // strips credentials" bug. An operator who writes
        //   { path_glob: "*.zip", bucket: "x", action: allow-anonymous }
        // expects the rule to carve out unsigned access; they do NOT
        // expect a signed admin's request to get pre-admitted as
        // `$anonymous` (which, on a bucket with no public_prefixes,
        // lands them in a scoped principal with zero permissions →
        // surprise 403). The fix: when `authenticated` is unset and
        // the action is `AllowAnonymous`, default to matching only
        // unauthenticated requests.
        use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction};
        let block = AdmissionBlockSpec {
            name: "allow-zips".into(),
            match_: MatchSpec {
                path_glob: Some("*.zip".into()),
                bucket: Some("releases".into()),
                method: Some(vec!["GET".into(), "HEAD".into()]),
                ..Default::default()
            },
            action: ActionSpec::Simple(SimpleAction::AllowAnonymous),
        };
        let chain = chain_from_spec(vec![block]);

        // Unauthenticated GET: the block fires as intended.
        let anon = RequestInfo {
            method: "GET",
            bucket: "releases",
            key: Some("v1.zip"),
            list_prefix: None,
            authenticated: false,
            source_ip: None,
        };
        assert_eq!(
            evaluate(&chain, &anon),
            Decision::AllowAnonymous {
                matched: "allow-zips".into(),
            }
        );

        // Same URL but now SIGNED: must NOT downgrade. The signed
        // caller flows through to SigV4 via Continue.
        let signed = RequestInfo {
            method: "GET",
            bucket: "releases",
            key: Some("v1.zip"),
            list_prefix: None,
            authenticated: true,
            source_ip: None,
        };
        assert_eq!(
            evaluate(&chain, &signed),
            Decision::Continue { matched: None },
            "signed request must not be pre-admitted as anonymous"
        );
    }

    #[test]
    fn evaluator_allow_anonymous_explicit_authenticated_true_still_matches_signed() {
        // Operator asserted `authenticated: true` explicitly. This is
        // an unusual shape but we honour it — the explicit predicate
        // overrides the action-specific default.
        use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction};
        let block = AdmissionBlockSpec {
            name: "signed-anon".into(),
            match_: MatchSpec {
                authenticated: Some(true),
                ..Default::default()
            },
            action: ActionSpec::Simple(SimpleAction::AllowAnonymous),
        };
        let chain = chain_from_spec(vec![block]);
        let signed = RequestInfo {
            method: "GET",
            bucket: "any",
            key: Some("any"),
            list_prefix: None,
            authenticated: true,
            source_ip: None,
        };
        assert!(matches!(
            evaluate(&chain, &signed),
            Decision::AllowAnonymous { .. }
        ));
    }
}
