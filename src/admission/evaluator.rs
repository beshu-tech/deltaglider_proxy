//! Admission evaluator — take a normalised request description, walk the
//! chain, return a decision.
//!
//! The evaluator is a pure function over [`RequestInfo`] and
//! [`AdmissionChain`]. It takes no locks and does no I/O. This is
//! deliberate: the same routine backs the live request-path middleware
//! and the admin `/config/trace` endpoint, and pureness is what lets us
//! test both surfaces with the same code.

use super::{Action, AdmissionChain, Decision, Match};

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
#[derive(Debug, Clone)]
pub struct RequestInfo<'a> {
    pub method: &'a str,
    pub bucket: &'a str,
    pub key: Option<&'a str>,
    pub list_prefix: Option<&'a str>,
    pub authenticated: bool,
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
        if matches(chain, &block.match_, req) {
            return match block.action {
                Action::AllowAnonymous => Decision::AllowAnonymous {
                    matched: block.name.clone(),
                },
                Action::Continue => Decision::Continue {
                    matched: Some(block.name.clone()),
                },
            };
        }
    }
    Decision::Continue { matched: None }
}

/// Predicate dispatch. New `Match` variants must add a branch here; the
/// wildcard is omitted intentionally so the compiler forces an update
/// when variants grow.
fn matches(chain: &AdmissionChain, m: &Match, req: &RequestInfo<'_>) -> bool {
    match m {
        Match::PublicPrefixGrant { bucket } => match_public_prefix_grant(chain, bucket, req),
    }
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
        };
        assert_eq!(evaluate(&chain, &req), Decision::Continue { matched: None });
    }
}
