// SPDX-License-Identifier: BUSL-1.1

//! The listing algebra of a prefix-scoped caller (review S12).
//!
//! A `ListScope::Filtered` caller may see only some keys of a bucket. Its
//! LIST reads only the prefixes its policy can see (`list_targets`), merges
//! them in key order, filters every entry per key, and never anchors a
//! continuation token on a hidden key. The engine is reached through the
//! one-method [`Lister`], so the algebra is tested without storage.

use crate::deltaglider::{EngineError, ListObjectsPage};
use crate::iam::{user_can_see_common_prefix, user_can_see_listed_key, ListScope};
use std::future::Future;

/// The one engine call the listing algebra makes: one LIST page.
/// `list` is declared `-> impl Future + Send` (not `async fn`) because the
/// s3s handler futures that await it must be `Send`; impls still write
/// `async fn`.
pub(crate) trait Lister: Sync {
    #[allow(clippy::too_many_arguments)]
    fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        cursor: Option<&str>,
        metadata: bool,
    ) -> impl Future<Output = Result<ListObjectsPage, EngineError>> + Send;
}

// Generic over the backend, not just `DynEngine`: the `Send` check of an
// s3s handler future erases lifetimes, so it needs this impl for
// `DeltaGliderEngine<Box<DynStorageBackend<'any>>>`, not only `<'static>`.
impl<S: crate::storage::StorageBackend> Lister for crate::deltaglider::DeltaGliderEngine<S> {
    async fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        cursor: Option<&str>,
        metadata: bool,
    ) -> Result<ListObjectsPage, EngineError> {
        self.list_objects(bucket, prefix, delimiter, max_keys, cursor, metadata)
            .await
    }
}

/// Why a filtered listing gave no page.
#[derive(Debug)]
pub(crate) enum ListingError {
    /// The scan budget ran out before one visible entry, and a hidden key
    /// must never be a token: the caller must list a narrower prefix.
    NoVisibleKeyInBudget,
    Engine(EngineError),
}

impl From<EngineError> for ListingError {
    fn from(e: EngineError) -> Self {
        ListingError::Engine(e)
    }
}

/// One part of a filtered LIST (review S12). A prefix-scoped user's listing
/// is the merge of these, in key order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListTarget {
    /// List this engine prefix with the request's delimiter and filter the
    /// entries per key.
    Scan(String),
    /// Every visible key under `probes` rolls up into this one common
    /// prefix. It is listed when a probe prefix holds any key.
    Rollup {
        common_prefix: String,
        probes: Vec<String>,
    },
}

impl ListTarget {
    fn start(&self) -> &str {
        match self {
            ListTarget::Scan(p) => p,
            ListTarget::Rollup { common_prefix, .. } => common_prefix,
        }
    }
}

/// Pure: what a filtered LIST of `prefix` must read, given the user's
/// visible key prefixes (`visible_key_prefixes`: sorted and minimal). The
/// targets cover disjoint key ranges and come back in key order, so their
/// listings concatenate into one sorted listing.
fn list_targets(prefix: &str, delimiter: Option<&str>, visible: &[String]) -> Vec<ListTarget> {
    // A visible prefix that covers the request: one plain (filtered) scan.
    if visible.iter().any(|v| prefix.starts_with(v.as_str())) {
        return vec![ListTarget::Scan(prefix.to_string())];
    }
    let delimiter = delimiter.filter(|d| !d.is_empty());
    let mut targets: Vec<ListTarget> = Vec::new();
    for v in visible {
        let Some(rest) = v.strip_prefix(prefix) else {
            continue; // disjoint from the request
        };
        match delimiter.and_then(|d| rest.find(d).map(|i| i + d.len())) {
            // The engine would fold every key under `v` into this prefix.
            Some(end) => {
                let common_prefix = format!("{prefix}{}", &rest[..end]);
                match targets.iter_mut().find(|t| t.start() == common_prefix) {
                    Some(ListTarget::Rollup { probes, .. }) => probes.push(v.clone()),
                    _ => targets.push(ListTarget::Rollup {
                        common_prefix,
                        probes: vec![v.clone()],
                    }),
                }
            }
            // No delimiter between the request prefix and `v`: listing `v`
            // rolls up at the same points as listing `prefix` would.
            None => targets.push(ListTarget::Scan(v.clone())),
        }
    }
    targets.sort_by(|a, b| a.start().cmp(b.start()));
    targets
}

/// One LIST page as the caller may see it. THE listing path for V1 and V2.
///
/// For a `ListScope::Filtered` caller, the engine's next-token is the last
/// key of the UNFILTERED page, so returning it leaks a hidden key (review
/// S12: `max-keys=1` walks the whole bucket through tokens). The token is
/// always the last VISIBLE entry.
///
/// The listing reads only the prefixes the user's policy can see
/// (`list_targets`), so hidden keys outside them cost nothing and every
/// visible key stays reachable. Only a policy that cannot be narrowed to
/// prefixes (a bucket-wide Allow with Deny carve-outs) scans hidden keys,
/// under `budget` engine pages (`advanced.filtered_list_max_engine_pages`):
/// a prefix-scoped user can ask for a prefix where every key is hidden, and
/// the scan must end.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn list_page_for_caller<L: Lister>(
    lister: &L,
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    max_keys: u32,
    cursor: Option<&str>,
    metadata: bool,
    scope: Option<&ListScope>,
    budget: usize,
) -> Result<ListObjectsPage, ListingError> {
    let (user, context) = match scope {
        Some(ListScope::Filtered { user, context }) => (user, context),
        _ => {
            return Ok(lister
                .list(bucket, prefix, delimiter, max_keys, cursor, metadata)
                .await?);
        }
    };
    let visible = crate::iam::permissions::visible_key_prefixes(user, bucket);
    let mut targets = list_targets(prefix, delimiter, &visible);
    let engine_takes = |t: &ListTarget| match t {
        ListTarget::Scan(p) => crate::types::ObjectKey::validate_prefix(p).is_ok(),
        ListTarget::Rollup { probes, .. } => probes
            .iter()
            .all(|p| crate::types::ObjectKey::validate_prefix(p).is_ok()),
    };
    if !targets.iter().all(engine_takes) {
        // A policy literal the engine refuses as a prefix (`a/..` of
        // `b/a/..*`): fall back to the budgeted scan of the request.
        targets = vec![ListTarget::Scan(prefix.to_string())];
    }
    // One entry past the page tells whether the listing goes on.
    let want = max_keys as usize + 1;
    let mut objects = Vec::new();
    let mut prefixes = std::collections::BTreeSet::new();
    let mut facts_missing = std::collections::HashSet::new();
    let mut more = false;
    // ONE engine-page budget for the whole request, shared by every target
    // (scans and roll-up probes): per target, N visible prefixes read N
    // budgets for one LIST.
    let mut budget = budget;
    for target in targets {
        if objects.len() + prefixes.len() >= want {
            break;
        }
        if budget == 0 {
            // Stopped before this target: the listing goes on after the
            // last visible entry.
            more = true;
            break;
        }
        match target {
            ListTarget::Rollup {
                common_prefix,
                probes,
            } => {
                if cursor.is_some_and(|c| common_prefix.as_str() <= c)
                    || !user_can_see_common_prefix(user, bucket, &common_prefix, context)
                {
                    continue;
                }
                for probe in probes {
                    if budget == 0 {
                        more = true;
                        break;
                    }
                    budget -= 1;
                    let page = lister
                        .list(bucket, &probe, delimiter, 1, None, false)
                        .await?;
                    if !page.objects.is_empty() || !page.common_prefixes.is_empty() {
                        prefixes.insert(common_prefix);
                        break;
                    }
                }
            }
            ListTarget::Scan(scan_prefix) => {
                let need = want - objects.len() - prefixes.len();
                let mut scan_cursor = cursor.map(str::to_string);
                while budget > 0 {
                    budget -= 1;
                    let page = lister
                        .list(
                            bucket,
                            &scan_prefix,
                            delimiter,
                            need as u32,
                            scan_cursor.as_deref(),
                            metadata,
                        )
                        .await?;
                    facts_missing.extend(page.facts_missing_keys);
                    objects.extend(
                        page.objects
                            .into_iter()
                            .filter(|(key, _)| user_can_see_listed_key(user, bucket, key, context)),
                    );
                    prefixes.extend(
                        page.common_prefixes
                            .into_iter()
                            .filter(|p| user_can_see_common_prefix(user, bucket, p, context)),
                    );
                    more = page.is_truncated && page.next_continuation_token.is_some();
                    scan_cursor = page.next_continuation_token;
                    if !more || objects.len() + prefixes.len() >= want {
                        break;
                    }
                }
                if more {
                    // Stopped inside this scan (page full or budget spent):
                    // later targets come after its remaining keys.
                    break;
                }
            }
        }
    }
    if more && objects.is_empty() && prefixes.is_empty() {
        // No visible entry to anchor a token on, and a hidden key must never
        // be one. Fail instead of pretending the listing ended.
        return Err(ListingError::NoVisibleKeyInBudget);
    }
    let mut page = crate::deltaglider::interleave_and_paginate(
        objects,
        prefixes.into_iter().collect(),
        max_keys,
        None,
    );
    if more && !page.is_truncated {
        // At most `max_keys` visible entries and the scan stopped early: the
        // token is the last visible entry.
        page.is_truncated = true;
        page.next_continuation_token = page
            .objects
            .last()
            .map(|(k, _)| k.clone())
            .into_iter()
            .chain(page.common_prefixes.last().cloned())
            .max();
    }
    // Only the entries this page shows: a hidden key is never counted.
    let facts_missing_keys = page
        .objects
        .iter()
        .filter(|(k, _)| facts_missing.contains(k))
        .map(|(k, _)| k.clone())
        .collect();
    Ok(ListObjectsPage {
        objects: page.objects,
        common_prefixes: page.common_prefixes,
        is_truncated: page.is_truncated,
        next_continuation_token: page.next_continuation_token,
        facts_missing_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iam::permissions::permission_to_iam_policy;
    use crate::iam::AuthenticatedUser;
    use crate::storage::DynStorageBackend;
    use std::sync::Arc;

    /// A lister over a fixed key set (no delimiter), counting its calls.
    struct CountingLister {
        keys: Vec<String>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Lister for CountingLister {
        async fn list(
            &self,
            _bucket: &str,
            prefix: &str,
            _delimiter: Option<&str>,
            max_keys: u32,
            cursor: Option<&str>,
            _metadata: bool,
        ) -> Result<ListObjectsPage, EngineError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut after: Vec<&String> = self
                .keys
                .iter()
                .filter(|k| k.starts_with(prefix) && cursor.is_none_or(|c| k.as_str() > c))
                .collect();
            let is_truncated = after.len() > max_keys as usize;
            after.truncate(max_keys as usize);
            let objects: Vec<(String, crate::types::FileMetadata)> = after
                .into_iter()
                .map(|k| {
                    let meta = crate::types::FileMetadata::new_passthrough(
                        k.clone(),
                        String::new(),
                        String::new(),
                        1,
                        None,
                    );
                    (k.clone(), meta)
                })
                .collect();
            Ok(ListObjectsPage {
                next_continuation_token: is_truncated
                    .then(|| objects.last().map(|(k, _)| k.clone()))
                    .flatten(),
                objects,
                common_prefixes: Vec::new(),
                is_truncated,
                facts_missing_keys: Vec::new(),
            })
        }
    }

    /// The budget is exactly `budget` engine requests: a scan of hidden keys
    /// stops after them with `NoVisibleKeyInBudget`, and one more page of
    /// budget reaches the visible key.
    #[tokio::test]
    async fn the_scan_makes_exactly_budget_engine_requests() {
        // `max_keys=1` reads two entries per engine request.
        let mut keys: Vec<String> = (0..20).map(|i| format!("h/{i:02}")).collect();
        keys.push("v".into());
        let lister = CountingLister {
            keys,
            calls: Default::default(),
        };
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![read("Allow", "b/*"), read("Deny", "b/h/*")]);
        let err = list_page_for_caller(&lister, "b", "", None, 1, None, false, Some(&scope), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, ListingError::NoVisibleKeyInBudget), "{err:?}");
        assert_eq!(lister.calls.load(std::sync::atomic::Ordering::SeqCst), 10);
        let page = list_page_for_caller(&lister, "b", "", None, 1, None, false, Some(&scope), 11)
            .await
            .unwrap();
        assert_eq!(page.objects[0].0, "v");
    }

    /// A lister whose entries all lack listing facts.
    struct NoFactsLister(CountingLister);

    impl Lister for NoFactsLister {
        async fn list(
            &self,
            bucket: &str,
            prefix: &str,
            delimiter: Option<&str>,
            max_keys: u32,
            cursor: Option<&str>,
            metadata: bool,
        ) -> Result<ListObjectsPage, EngineError> {
            let mut page = self
                .0
                .list(bucket, prefix, delimiter, max_keys, cursor, metadata)
                .await?;
            page.facts_missing_keys = page.objects.iter().map(|(k, _)| k.clone()).collect();
            Ok(page)
        }
    }

    /// A filtered page reports the facts misses of the entries it shows,
    /// never of a hidden key.
    #[tokio::test]
    async fn a_filtered_page_counts_only_visible_facts_misses() {
        let lister = NoFactsLister(CountingLister {
            keys: vec!["h/1".into(), "v/1".into(), "v/2".into()],
            calls: Default::default(),
        });
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![read("Allow", "b/*"), read("Deny", "b/h/*")]);
        let page = list_page_for_caller(&lister, "b", "", None, 10, None, false, Some(&scope), 10)
            .await
            .unwrap();
        assert_eq!(
            page.facts_missing_keys,
            vec!["v/1".to_string(), "v/2".to_string()]
        );
    }

    fn policy_context_for_ip(client_ip: Option<std::net::IpAddr>) -> iam_rs::Context {
        let mut context = iam_rs::Context::new();
        crate::iam::permissions::insert_source_ip(&mut context, client_ip);
        context
    }

    /// A small scan budget, so a test passes it with few objects.
    const TEST_BUDGET: usize = 16;

    /// Review-2 (S12): the refill scans at most TEST_BUDGET
    /// engine pages and the token is the last VISIBLE key. More hidden
    /// entries than the budget between two visible ones make every later
    /// visible key unreachable: each follow-up restarts at the same token
    /// and fails.
    #[tokio::test]
    async fn review2_filtered_list_reaches_visible_keys_past_the_scan_budget() {
        use crate::iam::permissions::permission_to_iam_policy;
        use crate::iam::Permission;
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<crate::storage::DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine: crate::deltaglider::DynEngine =
            crate::deltaglider::DeltaGliderEngine::new_with_backend(
                Arc::new(backend),
                &crate::config::Config::default(),
                None,
            );
        engine.create_bucket("b").await.unwrap();
        engine
            .store("b", "d/a/v.png", b"x", None, Default::default())
            .await
            .unwrap();
        for i in 0..=TEST_BUDGET {
            engine
                .store(
                    "b",
                    &format!("d/h{i:05}/x.png"),
                    b"x",
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
        }
        engine
            .store("b", "d/z/v.png", b"x", None, Default::default())
            .await
            .unwrap();
        let allow = Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into(), "list".into()],
            resources: vec!["b/d/a/*".into(), "b/d/z/*".into()],
            conditions: None,
        };
        let user = AuthenticatedUser {
            name: "u".into(),
            access_key_id: "AK".into(),
            permissions: vec![allow.clone()],
            iam_policies: vec![permission_to_iam_policy(&allow)],
        };
        let scope = ListScope::Filtered {
            user: Box::new(user),
            context: Box::new(policy_context_for_ip(None)),
        };
        let p1 = list_page_for_caller(
            &engine,
            "b",
            "d/",
            Some("/"),
            1,
            None,
            false,
            Some(&scope),
            TEST_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(p1.common_prefixes, vec!["d/a/".to_string()]);
        let p2 = list_page_for_caller(
            &engine,
            "b",
            "d/",
            Some("/"),
            1,
            p1.next_continuation_token.as_deref(),
            false,
            Some(&scope),
            TEST_BUDGET,
        )
        .await;
        assert_eq!(
            p2.expect("d/z/ must stay reachable").common_prefixes,
            vec!["d/z/".to_string()]
        );
    }

    #[test]
    fn list_targets_read_only_the_visible_prefixes() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let scan = |p: &str| ListTarget::Scan(p.into());
        let roll = |cp: &str, probes: &[&str]| ListTarget::Rollup {
            common_prefix: cp.into(),
            probes: v(probes),
        };
        // A visible prefix covers the request: one scan of the request.
        assert_eq!(
            list_targets("d/x/", Some("/"), &v(&["d/"])),
            vec![scan("d/x/")]
        );
        assert_eq!(list_targets("", None, &v(&[""])), vec![scan("")]);
        // Deeper prefixes roll up at the delimiter, merged per common prefix.
        assert_eq!(
            list_targets("d/", Some("/"), &v(&["d/a/x", "d/a/y", "d/b", "e/"])),
            vec![roll("d/a/", &["d/a/x", "d/a/y"]), scan("d/b")]
        );
        // No delimiter: every visible prefix is its own scan.
        assert_eq!(
            list_targets("d/", None, &v(&["d/a/", "d/z/"])),
            vec![scan("d/a/"), scan("d/z/")]
        );
        assert!(list_targets("d/", Some("/"), &v(&["e/"])).is_empty());
    }

    fn scoped(perms: Vec<crate::iam::Permission>) -> ListScope {
        use crate::iam::permissions::permission_to_iam_policy;
        let user = AuthenticatedUser {
            name: "u".into(),
            access_key_id: "AK".into(),
            iam_policies: perms.iter().map(permission_to_iam_policy).collect(),
            permissions: perms,
        };
        ListScope::Filtered {
            user: Box::new(user),
            context: Box::new(policy_context_for_ip(None)),
        }
    }

    fn rule(effect: &str, resources: &[&str]) -> crate::iam::Permission {
        crate::iam::Permission {
            id: 0,
            effect: effect.into(),
            actions: vec!["read".into(), "list".into()],
            resources: resources.iter().map(|s| s.to_string()).collect(),
            conditions: None,
        }
    }

    async fn fs_engine(keys: &[&str]) -> (tempfile::TempDir, crate::deltaglider::DynEngine) {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<crate::storage::DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine: crate::deltaglider::DynEngine =
            crate::deltaglider::DeltaGliderEngine::new_with_backend(
                Arc::new(backend),
                &crate::config::Config::default(),
                None,
            );
        engine.create_bucket("b").await.unwrap();
        for k in keys {
            engine
                .store("b", k, b"x", None, Default::default())
                .await
                .unwrap();
        }
        (dir, engine)
    }

    /// Every entry of a filtered listing, walked page by page.
    async fn walk(
        engine: &crate::deltaglider::DynEngine,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        scope: &ListScope,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..100 {
            let page = list_page_for_caller(
                engine,
                "b",
                prefix,
                delimiter,
                max_keys,
                token.as_deref(),
                false,
                Some(scope),
                TEST_BUDGET,
            )
            .await
            .unwrap();
            let mut entries: Vec<String> = page.objects.into_iter().map(|(k, _)| k).collect();
            entries.extend(page.common_prefixes);
            entries.sort();
            assert!(entries.len() <= max_keys as usize);
            out.extend(entries);
            if !page.is_truncated {
                return out;
            }
            token = page.next_continuation_token;
        }
        panic!("listing did not end");
    }

    /// A prefix-scoped listing returns exactly what the per-key filter
    /// admits, in order, for every page size, with and without a delimiter.
    #[tokio::test]
    async fn filtered_listing_matches_the_per_key_filter_for_every_page_size() {
        let keys = [
            "d/a/1.png",
            "d/a/sub/2.png",
            "d/ab.png",
            "d/h1/x.png",
            "d/h2/x.png",
            "d/m/deep/3.png",
            "d/m/other.png",
            "d/z/4.png",
            "top.png",
        ];
        let (_dir, engine) = fs_engine(&keys).await;
        let scope = scoped(vec![
            rule("Allow", &["b/d/a*", "b/d/m/deep/*", "b/d/z/*"]),
            rule("Deny", &["b/d/a/sub/*"]),
        ]);
        let cases: [(&str, Option<&str>, &[&str]); 3] = [
            ("d/", Some("/"), &["d/a/", "d/ab.png", "d/m/", "d/z/"]),
            ("", Some("/"), &["d/"]),
            (
                "d/",
                None,
                &["d/a/1.png", "d/ab.png", "d/m/deep/3.png", "d/z/4.png"],
            ),
        ];
        for (prefix, delimiter, expected) in cases {
            for max_keys in 1..=5 {
                assert_eq!(
                    walk(&engine, prefix, delimiter, max_keys, &scope).await,
                    expected.to_vec(),
                    "prefix={prefix:?} delimiter={delimiter:?} max_keys={max_keys}"
                );
            }
        }
    }

    /// A bucket-wide Allow cannot be narrowed to prefixes: the budgeted scan
    /// stays the fallback, and a run of hidden keys past the budget fails
    /// the request instead of leaking a hidden key as the token.
    #[tokio::test]
    async fn bucket_wide_allow_with_a_large_deny_keeps_the_budget() {
        // `max_keys=1` reads two entries per engine page.
        let mut keys: Vec<String> = (0..=2 * TEST_BUDGET)
            .map(|i| format!("h/{i:03}.png"))
            .collect();
        keys.push("v.png".into());
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let (_dir, engine) = fs_engine(&refs).await;
        // Read only: a bucket-level `list` would admit every key anyway.
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![read("Allow", "b/*"), read("Deny", "b/h/*")]);
        let err = list_page_for_caller(
            &engine,
            "b",
            "",
            None,
            1,
            None,
            false,
            Some(&scope),
            TEST_BUDGET,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ListingError::NoVisibleKeyInBudget), "{err:?}");
    }

    /// The page budget is one per REQUEST, not one per visible prefix: a
    /// scan of hidden keys that ends inside the budget left the next prefix
    /// a whole new budget, so a policy with N prefixes read N budgets of
    /// engine pages for one LIST. Here `a/` spends the whole budget on
    /// hidden keys, so `b/` must not be read.
    #[tokio::test]
    async fn review3_one_page_budget_per_request() {
        // `max_keys=1` reads two entries per engine page.
        let mut keys: Vec<String> = (0..2 * TEST_BUDGET)
            .map(|i| format!("a/{i:03}.png"))
            .collect();
        keys.push("b/v.png".into());
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let (_dir, engine) = fs_engine(&refs).await;
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![
            read("Allow", "b/a/*"),
            read("Allow", "b/b/*"),
            read("Deny", "b/a/*"),
        ]);
        let got = list_page_for_caller(
            &engine,
            "b",
            "",
            None,
            1,
            None,
            false,
            Some(&scope),
            TEST_BUDGET,
        )
        .await;
        assert!(
            got.is_err(),
            "the request read past its page budget: {:?}",
            got.map(|p| p.objects.into_iter().map(|(k, _)| k).collect::<Vec<_>>())
        );
    }

    fn perm(actions: &[&str], resources: &[&str]) -> crate::iam::Permission {
        crate::iam::Permission {
            id: 0,
            effect: "Allow".into(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
            resources: resources.iter().map(|s| s.to_string()).collect(),
            conditions: None,
        }
    }

    /// `visible_key_prefixes` ignores actions: a WRITE-only grant on a big
    /// upload prefix becomes a scan target. That scan finds no visible key,
    /// spends the page budget (one engine page of `max_keys + 1` keys per
    /// step), and the request fails before the readable prefix is reached.
    #[tokio::test]
    async fn review3_a_write_only_grant_does_not_hide_the_readable_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<crate::storage::DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine: crate::deltaglider::DynEngine =
            crate::deltaglider::DeltaGliderEngine::new_with_backend(
                Arc::new(backend),
                &crate::config::Config::default(),
                None,
            );
        engine.create_bucket("b").await.unwrap();
        for i in 0..41 {
            engine
                .store(
                    "b",
                    &format!("incoming/{i:03}.png"),
                    b"x",
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
        }
        engine
            .store("b", "releases/a.png", b"x", None, Default::default())
            .await
            .unwrap();
        let perms = vec![
            perm(&["write"], &["b/incoming/*"]),
            perm(&["read", "list"], &["b/releases/*"]),
        ];
        let user = AuthenticatedUser {
            name: "u".into(),
            access_key_id: "AK".into(),
            iam_policies: perms.iter().map(permission_to_iam_policy).collect(),
            permissions: perms,
        };
        let scope = ListScope::Filtered {
            user: Box::new(user),
            context: Box::new(policy_context_for_ip(None)),
        };
        let page = list_page_for_caller(
            &engine,
            "b",
            "",
            None,
            1,
            None,
            false,
            Some(&scope),
            TEST_BUDGET,
        )
        .await
        .expect("the listing must not fail on the write-only prefix");
        assert_eq!(
            page.objects.first().map(|(k, _)| k.as_str()),
            Some("releases/a.png")
        );
    }
}
