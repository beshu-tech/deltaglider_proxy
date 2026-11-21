// SPDX-License-Identifier: BUSL-1.1

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
///   or `None` for a bucket-level request (LIST, batch delete). A block's
///   `path_glob` decides keys only: a request with no key meets a `deny`,
///   `reject` or `continue` block only when the glob covers every key below
///   the LIST prefix, and an `allow-anonymous` block when the glob can match
///   some key below it.
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
    walk(chain, req, MethodView::Line)
}

/// Which methods a block's `method` list is compared with.
#[derive(Debug, Clone, Copy)]
pub(super) enum MethodView<'a> {
    /// The request line: `req.method` is the method of the request.
    Line,
    /// A resource that the request READS beyond its request line (a copy
    /// source, a listed key, a ZIP entry): only the effect counts, so a list
    /// that names `GET` or `HEAD` matches.
    Read,
    /// A key that the request WRITES or DELETES (a form POST key, a
    /// DeleteObjects key): the effect (`req.method`, `PUT` or `DELETE`) or the
    /// method of the request line, `line`.
    Mutation { line: &'a str },
}

/// [`evaluate`] for a resource that the request reaches beyond its request
/// line, over `blocks` (the blocks of the chain that can concern the
/// resource's bucket, in chain order); `view` says which methods count
/// (see `Effect`).
pub(super) fn evaluate_blocks<'b>(
    chain: &AdmissionChain,
    blocks: impl Iterator<Item = &'b super::AdmissionBlock>,
    req: &RequestInfo<'_>,
    view: MethodView<'_>,
) -> Decision {
    walk_blocks(chain, blocks, req, view)
}

fn walk(chain: &AdmissionChain, req: &RequestInfo<'_>, view: MethodView<'_>) -> Decision {
    walk_blocks(chain, chain.blocks().iter(), req, view)
}

fn walk_blocks<'b>(
    chain: &AdmissionChain,
    blocks: impl Iterator<Item = &'b super::AdmissionBlock>,
    req: &RequestInfo<'_>,
    view: MethodView<'_>,
) -> Decision {
    for block in blocks {
        if matches(chain, &block.match_, &block.action, req, view) {
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
fn matches(
    chain: &AdmissionChain,
    m: &Match,
    action: &Action,
    req: &RequestInfo<'_>,
    view: MethodView<'_>,
) -> bool {
    match m {
        Match::PublicPrefixGrant { bucket } => match_public_prefix_grant(chain, bucket, req),
        Match::Predicates(p) => match_predicates(p, action, req, view),
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
fn match_predicates(
    p: &Predicates,
    action: &Action,
    req: &RequestInfo<'_>,
    view: MethodView<'_>,
) -> bool {
    if !request_facts_match(p, action, req, view) || !bucket_matches(p, req) {
        return false;
    }
    if let Some(glob) = &p.path_glob {
        // A `path_glob` decides KEYS, never request text. A request that
        // names no key (a LIST, a batch delete, a bucket-level verb) meets a
        // `deny`, `reject` or `continue` block when the glob covers EVERY key
        // below the request's prefix (the empty prefix for a verb with none):
        // no key is left for the per-key walk to decide. Otherwise that walk
        // decides, key by key. An `allow-anonymous` block admits such a
        // request when SOME key below the prefix can match (the listing then
        // shows just the matching keys).
        match req.key {
            Some(key) => {
                if !glob.matches_key(key) {
                    return false;
                }
            }
            None => {
                let prefix = req.list_prefix.unwrap_or("");
                let covered = match action {
                    Action::AllowAnonymous => glob.can_match_below(prefix),
                    Action::Deny | Action::Reject { .. } | Action::Continue => {
                        glob.matches_all_below(prefix)
                    }
                };
                if !covered {
                    return false;
                }
            }
        }
    }
    true
}

/// The `bucket` predicate.
pub(super) fn bucket_matches(p: &Predicates, req: &RequestInfo<'_>) -> bool {
    p.bucket.as_ref().is_none_or(|b| b == req.bucket)
}

/// The `method`, `source_ip`, `authenticated` and `config_flag` predicates.
/// The last three describe the request as a whole. `method` names the method
/// of the request line, or, for a resource that the request reaches beyond it,
/// the effect on that resource (`req.method` is then the effect's method), see
/// [`MethodView`]. `GET` and `HEAD` are ONE read: for a block with a `bucket`
/// or a `path_glob` a list that names either matches both, so a `GET` rule
/// cannot be read around with a `HEAD` (size, ETag, metadata).
pub(super) fn request_facts_match(
    p: &Predicates,
    action: &Action,
    req: &RequestInfo<'_>,
    view: MethodView<'_>,
) -> bool {
    if let Some(methods) = &p.methods {
        let named = |name: &str| methods.iter().any(|m| m.eq_ignore_ascii_case(name));
        let is_read =
            |name: &str| name.eq_ignore_ascii_case("GET") || name.eq_ignore_ascii_case("HEAD");
        let one_read = p.bucket.is_some() || p.path_glob.is_some();
        let names = |name: &str| {
            named(name) || (one_read && is_read(name) && (named("GET") || named("HEAD")))
        };
        let ok = match view {
            MethodView::Line => names(req.method),
            MethodView::Read => named("GET") || named("HEAD"),
            MethodView::Mutation { line } => names(req.method) || names(line),
        };
        if !ok {
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
    // No flag registry exists yet — every config_flag predicate fails
    // closed (the chain builder warns per block).
    p.config_flag.is_none()
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

#[cfg(test)]
mod glob_proptests {
    use super::*;
    use crate::admission::spec::{
        ActionSpec, AdmissionBlockSpec, AdmissionSpec, MatchSpec, SimpleAction,
    };
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn deny_block(glob: &str) -> AdmissionBlockSpec {
        AdmissionBlockSpec {
            name: "g".into(),
            match_: MatchSpec {
                path_glob: Some(glob.into()),
                bucket: Some("b".into()),
                ..Default::default()
            },
            action: ActionSpec::Simple(SimpleAction::Deny),
        }
    }

    fn denied(glob: &str, key: Option<&str>, list_prefix: Option<&str>) -> bool {
        let chain = AdmissionChain::from_config_parts(&BTreeMap::new(), &[deny_block(glob)]);
        let req = RequestInfo {
            method: "GET",
            bucket: "b",
            key,
            list_prefix,
            authenticated: true,
            source_ip: None,
        };
        matches!(evaluate(&chain, &req), Decision::Deny { .. })
    }

    proptest! {
        /// Arbitrary operator globs: validation and chain build never panic,
        /// and they agree (a glob the loader accepts is one the chain keeps).
        #[test]
        fn any_glob_validates_and_compiles_consistently(glob in ".{0,24}", key in ".{0,24}") {
            let spec = AdmissionSpec { blocks: vec![deny_block(&glob)] };
            let valid = spec.validate().is_ok();
            let compiles = globset::Glob::new(&glob).is_ok();
            prop_assert_eq!(valid, compiles, "validate and compile disagree on {:?}", glob);
            let _ = denied(&glob, Some(&key), None);
        }

        /// `lit*` fires exactly on keys that start with
        /// `lit`, `*` crossing `/`: a Deny on `secret/*` covers
        /// `secret/a/b`. A literal glob fires on that one key only.
        #[test]
        fn prefix_glob_matches_exactly_the_keys_under_it(
            lit in "[a-z0-9/._-]{0,8}",
            key in "[a-z0-9/._-]{0,12}",
        ) {
            let glob = format!("{lit}*");
            prop_assert_eq!(denied(&glob, Some(&key), None), key.starts_with(&lit), "{} vs {}", glob, key);
            // A LIST meets the block when the glob covers every key below its prefix.
            prop_assert_eq!(denied(&glob, None, Some(&key)), key.starts_with(&lit));
            if !lit.is_empty() {
                prop_assert_eq!(denied(&lit, Some(&key), None), key == lit);
            }
        }
    }
}

/// A request that names no key against a block with a `path_glob`: the old
/// rule matched the glob against the TEXT of the prefix; the new rule asks
/// whether the glob covers every key below the prefix. The table lists both
/// answers for every kind of keyless request and every action.
#[cfg(test)]
mod keyless_tests {
    use super::*;
    use crate::admission::spec::{
        ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction, TaggedAction,
    };
    use std::collections::BTreeMap;

    /// The rule before keyless requests had their own: the glob against the prefix text.
    fn old_matches(glob: &str, prefix: &str) -> bool {
        globset::Glob::new(glob)
            .unwrap()
            .compile_matcher()
            .is_match(prefix)
    }

    fn block(glob: &str, action: ActionSpec) -> AdmissionBlockSpec {
        AdmissionBlockSpec {
            name: "g".into(),
            match_: MatchSpec {
                path_glob: Some(glob.into()),
                bucket: Some("b".into()),
                ..Default::default()
            },
            action,
        }
    }

    fn fires(glob: &str, action: ActionSpec, prefix: Option<&str>) -> bool {
        let chain = AdmissionChain::from_config_parts(&BTreeMap::new(), &[block(glob, action)]);
        let req = RequestInfo {
            method: "GET",
            bucket: "b",
            key: None,
            list_prefix: prefix,
            authenticated: false,
            source_ip: None,
        };
        !matches!(evaluate(&chain, &req), Decision::Continue { matched: None })
    }

    fn actions() -> Vec<(&'static str, ActionSpec)> {
        vec![
            ("deny", ActionSpec::Simple(SimpleAction::Deny)),
            (
                "reject",
                ActionSpec::Tagged(TaggedAction::Reject {
                    status: 503,
                    message: None,
                }),
            ),
            ("continue", ActionSpec::Simple(SimpleAction::Continue)),
        ]
    }

    /// (case, glob, list prefix, old text rule fires, new rule fires).
    /// A bucket-level verb (CreateBucket, HEAD bucket, DeleteObjects) has no prefix.
    const TABLE: [(&str, &str, Option<&str>, bool, bool); 17] = [
        ("LIST inside the glob", "db/**", Some("db/"), true, true),
        ("LIST deeper inside", "db/**", Some("db/sub/"), true, true),
        (
            "LIST a name prefix inside",
            "db/**",
            Some("db/a"),
            true,
            true,
        ),
        (
            "LIST outside the glob",
            "db/**",
            Some("docs/"),
            false,
            false,
        ),
        ("LIST partly inside (`d`)", "db/**", Some("d"), false, false),
        ("LIST of the bucket root", "db/**", None, false, false),
        ("DeleteObjects", "db/**", None, false, false),
        ("CreateBucket", "db/**", None, false, false),
        ("HEAD bucket", "db/**", None, false, false),
        ("LIST under `**`", "**", Some("db/"), true, true),
        ("DeleteObjects under `**`", "**", None, true, true),
        ("CreateBucket under `**`", "**", None, true, true),
        ("HEAD bucket under `**`", "**", None, true, true),
        // The old text rule fired here although `a.zip.bak` is no zip: wrong.
        (
            "LIST `a.zip` under `*.zip`",
            "*.zip",
            Some("a.zip"),
            true,
            false,
        ),
        ("LIST root under `*.zip`", "*.zip", None, false, false),
        (
            "LIST inside a folder glob with a class",
            "[ab]/**",
            Some("a/x"),
            true,
            true,
        ),
        (
            "LIST partly inside a class glob",
            "[ab]/**",
            Some(""),
            false,
            false,
        ),
    ];

    #[test]
    fn a_keyless_request_meets_a_block_that_covers_every_key_below_its_prefix() {
        for (name, action) in actions() {
            for (case, glob, prefix, old, new) in TABLE {
                assert_eq!(
                    old_matches(glob, prefix.unwrap_or("")),
                    old,
                    "old rule: {case} / {glob}"
                );
                assert_eq!(
                    fires(glob, action.clone(), prefix),
                    new,
                    "{name}: {case} / {glob}"
                );
            }
        }
    }

    /// `allow-anonymous` keeps its own rule: some key below the prefix can match.
    #[test]
    fn allow_anonymous_admits_a_keyless_request_when_some_key_can_match() {
        let anon = || ActionSpec::Simple(SimpleAction::AllowAnonymous);
        for (case, glob, prefix, expected) in [
            ("LIST inside", "db/**", Some("db/"), true),
            ("LIST partly inside", "db/**", Some("d"), true),
            ("LIST of the root", "db/**", None, true),
            ("LIST outside", "db/**", Some("docs/"), false),
            ("LIST root under `*.zip`", "*.zip", None, true),
        ] {
            assert_eq!(fires(glob, anon(), prefix), expected, "{case}");
        }
    }
}

#[cfg(test)]
mod one_read_tests {
    use super::*;
    use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction};
    use std::collections::BTreeMap;

    fn denied(match_: MatchSpec, method: &str, key: Option<&str>) -> bool {
        let block = AdmissionBlockSpec {
            name: "d".into(),
            match_,
            action: ActionSpec::Simple(SimpleAction::Deny),
        };
        let chain = AdmissionChain::from_config_parts(&BTreeMap::new(), &[block]);
        let req = RequestInfo {
            method,
            bucket: "b",
            key,
            list_prefix: None,
            authenticated: true,
            source_ip: None,
        };
        matches!(evaluate(&chain, &req), Decision::Deny { .. })
    }

    fn rule(glob: Option<&str>, bucket: Option<&str>, methods: &[&str]) -> MatchSpec {
        MatchSpec {
            path_glob: glob.map(Into::into),
            bucket: bucket.map(Into::into),
            method: Some(methods.iter().map(|m| m.to_string()).collect()),
            ..Default::default()
        }
    }

    /// GET and HEAD are ONE read: a rule with a `path_glob` or a `bucket` that
    /// names either covers both request lines (a HEAD shows size, ETag and metadata).
    #[test]
    fn get_and_head_are_one_read_for_a_rule_with_a_glob_or_a_bucket() {
        for (m, other) in [("GET", "HEAD"), ("HEAD", "GET")] {
            let r = || rule(Some("secret/**"), None, &[m]);
            assert!(denied(r(), m, Some("secret/x")));
            assert!(
                denied(r(), other, Some("secret/x")),
                "{m} rule, {other} line"
            );
            assert!(!denied(r(), other, Some("public/x")));
            assert!(denied(rule(None, Some("b"), &[m]), other, Some("any")));
        }
        // A rule that names other methods is no read rule.
        assert!(!denied(
            rule(Some("secret/**"), None, &["PUT"]),
            "HEAD",
            Some("secret/x")
        ));
        assert!(!denied(
            rule(Some("secret/**"), None, &["GET"]),
            "PUT",
            Some("secret/x")
        ));
        // Without a bucket or a glob the method match stays literal.
        let plain = || rule(None, None, &["GET"]);
        assert!(denied(plain(), "GET", Some("any")));
        assert!(!denied(plain(), "HEAD", Some("any")));
    }
}
