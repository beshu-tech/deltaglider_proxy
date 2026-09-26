// SPDX-License-Identifier: BUSL-1.1

//! What an `allow-anonymous` decision lets the anonymous request do.
//!
//! An operator-authored `allow-anonymous` block used to only mark the
//! request: the `$anonymous` principal got permissions from the bucket's
//! `public_prefixes`, so a block on a bucket without public prefixes
//! admitted a request that authorization then refused (403) — while the
//! trace said `allow-anonymous`. [`anonymous_grant`] is the ONE decision:
//! the live path turns its result into the principal's permissions, and
//! the trace reports it verbatim.
//!
//! The grant covers exactly the request that matched, read-class only:
//! GET/HEAD of that object, or a LIST of that bucket with that prefix.
//! A write never gets a grant. Synthesised `public-prefix:` blocks keep
//! their grant: the bucket's public prefixes.

use serde::Serialize;

use super::{Decision, RequestInfo};
use crate::iam::Permission;

/// The single request an `allow-anonymous` decision authorizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "action")]
pub enum AnonymousGrant {
    /// A synthesised `public-prefix:<bucket>` block: the grant is the
    /// bucket's `public_prefixes` (read + prefix-scoped list), never the
    /// whole request — a root LIST overlaps a public prefix, and granting
    /// it outright would list the private keys too.
    PublicPrefixes { bucket: String },
    /// GET/HEAD of exactly this object.
    Read { bucket: String, key: String },
    /// A bucket-level read (LIST) with exactly this `prefix` ("" = none).
    List { bucket: String, prefix: String },
}

/// Pure: the grant for `req` under `decision`. `None` unless the decision
/// is `AllowAnonymous` and the request is read-class (GET/HEAD) on a
/// bucket.
pub fn anonymous_grant(decision: &Decision, req: &RequestInfo<'_>) -> Option<AnonymousGrant> {
    if !matches!(decision, Decision::AllowAnonymous { .. }) {
        return None;
    }
    if !matches!(req.method, "GET" | "HEAD") || req.bucket.is_empty() {
        return None;
    }
    let bucket = req.bucket.to_string();
    if let Decision::AllowAnonymous { matched } = decision {
        if matched.starts_with(super::PUBLIC_PREFIX_BLOCK_PREFIX) {
            return Some(AnonymousGrant::PublicPrefixes { bucket });
        }
    }
    Some(match req.key.filter(|k| !k.is_empty()) {
        Some(key) => AnonymousGrant::Read {
            bucket,
            key: key.to_string(),
        },
        None => AnonymousGrant::List {
            bucket,
            prefix: req.list_prefix.unwrap_or("").to_string(),
        },
    })
}

impl AnonymousGrant {
    /// The permission that allows exactly this request (`None` for
    /// `PublicPrefixes`: those permissions come from the bucket policy).
    /// The `$anonymous` principal is minted per request, so the grant
    /// never outlives it.
    pub fn permission(&self) -> Option<Permission> {
        Some(match self {
            Self::PublicPrefixes { .. } => return None,
            Self::Read { bucket, key } => Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into()],
                resources: vec![format!("{bucket}/{key}")],
                conditions: None,
            },
            Self::List { bucket, prefix } => Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["list".into()],
                resources: vec![format!("{bucket}/*")],
                // `prefix*` admits this request (its `s3:prefix`) and every
                // key it returns (all start with the prefix) for the
                // filtered listing. No `?prefix=` means no `s3:prefix`
                // context key, so a condition could never match: the bare
                // list is unconditional (the block matched it as such).
                conditions: (!prefix.is_empty()).then(
                    || serde_json::json!({ "StringLike": { "s3:prefix": [format!("{prefix}*")] } }),
                ),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(method: &'a str, key: Option<&'a str>, prefix: Option<&'a str>) -> RequestInfo<'a> {
        RequestInfo {
            method,
            bucket: "releases",
            key,
            list_prefix: prefix,
            authenticated: false,
            source_ip: None,
        }
    }

    fn allow() -> Decision {
        Decision::AllowAnonymous {
            matched: "allow-public-zips".into(),
        }
    }

    #[test]
    fn grant_truth_table() {
        assert_eq!(
            anonymous_grant(&allow(), &req("GET", Some("builds/app.zip"), None)),
            Some(AnonymousGrant::Read {
                bucket: "releases".into(),
                key: "builds/app.zip".into()
            })
        );
        assert!(matches!(
            anonymous_grant(&allow(), &req("HEAD", Some("a.zip"), None)),
            Some(AnonymousGrant::Read { .. })
        ));
        assert_eq!(
            anonymous_grant(&allow(), &req("GET", None, Some("builds/"))),
            Some(AnonymousGrant::List {
                bucket: "releases".into(),
                prefix: "builds/".into()
            })
        );
        // Writes are never granted.
        for m in ["PUT", "POST", "DELETE", "PATCH"] {
            assert_eq!(
                anonymous_grant(&allow(), &req(m, Some("a.zip"), None)),
                None,
                "{m}"
            );
        }
        // Only an allow-anonymous decision grants.
        let cont = Decision::Continue { matched: None };
        assert_eq!(
            anonymous_grant(&cont, &req("GET", Some("a.zip"), None)),
            None
        );
        let deny = Decision::Deny {
            matched: "x".into(),
        };
        assert_eq!(
            anonymous_grant(&deny, &req("GET", Some("a.zip"), None)),
            None
        );
        // A synthesised public-prefix block grants the bucket's public
        // prefixes, not the request (a root LIST would list private keys).
        let pp = Decision::AllowAnonymous {
            matched: "public-prefix:releases".into(),
        };
        assert_eq!(
            anonymous_grant(&pp, &req("GET", None, None)),
            Some(AnonymousGrant::PublicPrefixes {
                bucket: "releases".into()
            })
        );
        assert_eq!(anonymous_grant(&pp, &req("PUT", Some("a"), None)), None);
        // ListBuckets (no bucket) is never granted.
        let mut root = req("GET", None, None);
        root.bucket = "";
        assert_eq!(anonymous_grant(&allow(), &root), None);
    }

    #[test]
    fn permission_scopes_exactly_the_request() {
        let p = AnonymousGrant::Read {
            bucket: "releases".into(),
            key: "builds/app.zip".into(),
        }
        .permission()
        .unwrap();
        assert_eq!(p.actions, vec!["read"]);
        assert_eq!(p.resources, vec!["releases/builds/app.zip"]);
        let l = AnonymousGrant::List {
            bucket: "releases".into(),
            prefix: "builds/".into(),
        }
        .permission()
        .unwrap();
        assert_eq!(l.actions, vec!["list"]);
        assert!(l.conditions.unwrap().to_string().contains("builds/*"));
        let bare = AnonymousGrant::List {
            bucket: "releases".into(),
            prefix: String::new(),
        }
        .permission()
        .unwrap();
        assert!(bare.conditions.is_none());
        let pp = AnonymousGrant::PublicPrefixes {
            bucket: "releases".into(),
        };
        assert!(pp.permission().is_none());
    }
}
