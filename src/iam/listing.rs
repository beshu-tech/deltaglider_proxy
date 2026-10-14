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

/// The engine calls the listing algebra makes: one LIST page.
/// The methods are declared `-> impl Future + Send` (not `async fn`) because
/// the s3s handler futures that await them must be `Send`; impls still write
/// `async fn`.
pub(crate) trait Lister: Sync {
    /// One complete page: logical sizes, and with `metadata` the storage
    /// facts (a HEAD only for an entry whose listing facts are missing).
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

    /// One page as the storage lists it, before [`Self::finish`]: the scan
    /// of a filtered LIST reads hidden keys that need no logical size and no
    /// HEAD. Also returns the requests the page sent to the backend, which
    /// the scan budget counts. The default is a complete page without
    /// metadata, counted as one request.
    fn list_lite(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        cursor: Option<&str>,
    ) -> impl Future<Output = Result<(ListObjectsPage, u32), EngineError>> + Send {
        async move {
            let page = self
                .list(bucket, prefix, delimiter, max_keys, cursor, false)
                .await?;
            Ok((page, 1))
        }
    }

    /// Complete the page a filtered LIST returns, built from
    /// [`Self::list_lite`] pages: logical sizes, and the facts-miss HEADs with
    /// `metadata`. The default returns the page as it is.
    fn finish(
        &self,
        _bucket: &str,
        page: ListObjectsPage,
        _metadata: bool,
    ) -> impl Future<Output = Result<ListObjectsPage, EngineError>> + Send {
        async move { Ok(page) }
    }
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

    async fn list_lite(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        cursor: Option<&str>,
    ) -> Result<(ListObjectsPage, u32), EngineError> {
        self.list_objects_lite_counted(bucket, prefix, delimiter, max_keys, cursor)
            .await
    }

    async fn finish(
        &self,
        bucket: &str,
        mut page: ListObjectsPage,
        metadata: bool,
    ) -> Result<ListObjectsPage, EngineError> {
        self.finish_listed_page(bucket, &mut page, metadata).await?;
        Ok(page)
    }
}

/// Why a folder walk gave no key list.
#[derive(Debug)]
pub(crate) enum ListingError {
    /// The walk spent its scan budget before the end of the folder, and a
    /// partial folder must never be acted on: the caller must select a
    /// narrower folder.
    ScanBudgetSpent,
    Engine(EngineError),
}

impl From<EngineError> for ListingError {
    fn from(e: EngineError) -> Self {
        ListingError::Engine(e)
    }
}

/// Keys per engine page of a filtered scan, whatever `max-keys` is. The
/// engine windows of the scan then never depend on the client page size,
/// and the scan budget (`advanced.filtered_list_max_engine_pages`) reads
/// about `budget × 1000` keys: 50,000 by default.
pub(crate) const FILTERED_SCAN_PAGE_KEYS: u32 = 1000;

/// One LIST page for a caller, and what its continuation token is.
#[derive(Debug)]
pub(crate) struct CallerPage {
    pub page: ListObjectsPage,
    /// `true` when `page.next_continuation_token` is a scan position where
    /// the scan budget ran out. It can be a key the caller may not see, so
    /// it leaves the process only sealed. When `false`, the token is the
    /// last entry of the page, which the caller sees anyway.
    pub hidden_cursor: bool,
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
/// For a `ListScope::Filtered` caller, the listing reads only the prefixes
/// the user's policy can see (`list_targets`), so hidden keys outside them
/// cost nothing and every visible key stays reachable. Only a policy that
/// cannot be narrowed to prefixes (a bucket-wide Allow with Deny
/// carve-outs) scans hidden keys, under a budget of `budget` backend
/// requests (`advanced.filtered_list_max_engine_pages`).
///
/// The client picks where the scan starts (`start-after`, a token, a V1
/// `marker`), so the answer must not depend on the hidden keys after that
/// point (review 2d: a bisection on `start-after` recovered hidden names
/// from the answer). So:
///
/// - The scan reads engine pages of [`FILTERED_SCAN_PAGE_KEYS`] keys,
///   whatever `max_keys` is.
/// - The page is EXACT: it holds `max_keys` visible entries whenever that
///   many exist, and it is truncated only when a visible entry follows it.
///   Its token is then its last entry.
/// - When the budget runs out first (about `budget × 1000` hidden keys in
///   a row), the page holds the visible entries found so far (maybe none),
///   it is truncated, and its token is the scan position
///   ([`CallerPage::hidden_cursor`]), which the S3 adapter seals. A restart
///   from the last visible entry would read the same hidden keys again and
///   never end.
///
/// The scan reads lite pages ([`Lister::list_lite`]): an engine page of the
/// scan costs its LIST requests only, never a listing-facts request or a
/// HEAD for a key the filter then drops. The logical sizes, and with
/// `metadata` the HEADs, are read once, for the page the caller gets
/// ([`Lister::finish`]).
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
) -> Result<CallerPage, EngineError> {
    let Some(Visibility { user, context }) = Visibility::of(scope) else {
        return Ok(CallerPage {
            page: lister
                .list(bucket, prefix, delimiter, max_keys, cursor, metadata)
                .await?,
            hidden_cursor: false,
        });
    };
    let mut budget = budget;
    // One entry past the page tells whether the listing goes on.
    let want = max_keys as usize + 1;
    let scan = scan_visible(
        lister,
        bucket,
        prefix,
        delimiter,
        cursor,
        user,
        context,
        want,
        &mut budget,
    )
    .await?;
    let found = scan.objects.len() + scan.prefixes.len();
    let budget_spent = scan.resume.is_some() && found < want;
    let mut page = crate::deltaglider::interleave_and_paginate(
        scan.objects,
        scan.prefixes.into_iter().collect(),
        max_keys,
        None,
    );
    if budget_spent {
        // Fewer than `max_keys + 1` visible entries, and the scan stopped
        // before the end: the next page starts at the scan position.
        page.is_truncated = true;
        page.next_continuation_token = scan.resume;
    }
    // Only the entries this page shows: a hidden key is never counted.
    let facts_missing_keys = page
        .objects
        .iter()
        .filter(|(k, _)| scan.facts_missing.contains(k))
        .map(|(k, _)| k.clone())
        .collect();
    let page = ListObjectsPage {
        objects: page.objects,
        common_prefixes: page.common_prefixes,
        is_truncated: page.is_truncated,
        next_continuation_token: page.next_continuation_token,
        facts_missing_keys,
    };
    // Sizes and metadata for the visible page only.
    Ok(CallerPage {
        page: lister.finish(bucket, page, metadata).await?,
        hidden_cursor: budget_spent,
    })
}

/// Every visible key under `prefix` (no delimiter), at most `max_keys`, and
/// whether a visible key past them exists (review D7: the admin folder
/// listing walked a whole prefix of hidden keys with no budget). For a
/// filtered caller the walk spends at most `budget` backend requests IN
/// ALL, in engine pages of [`FILTERED_SCAN_PAGE_KEYS`] keys (see
/// [`list_page_for_caller`]). It goes on from its scan position, so it
/// reads each engine page once. A walk that spends the budget before the
/// end is `ScanBudgetSpent`, because a partial folder must never be acted
/// on. An unrestricted caller is bounded by `max_keys`.
pub(crate) async fn list_keys_for_caller<L: Lister>(
    lister: &L,
    bucket: &str,
    prefix: &str,
    scope: Option<&ListScope>,
    max_keys: usize,
    budget: usize,
) -> Result<(Vec<String>, bool), ListingError> {
    let mut budget = budget;
    let mut keys = Vec::new();
    let mut cursor: Option<String> = None;
    let visibility = Visibility::of(scope);
    loop {
        let (objects, next) = match &visibility {
            None => {
                let page = lister
                    .list(bucket, prefix, None, 1000, cursor.as_deref(), false)
                    .await?;
                let next = page.next_continuation_token.filter(|_| page.is_truncated);
                (page.objects, next)
            }
            Some(Visibility { user, context }) => {
                let want = max_keys.saturating_sub(keys.len()) + 1;
                let scan = scan_visible(
                    lister,
                    bucket,
                    prefix,
                    None,
                    cursor.as_deref(),
                    user,
                    context,
                    want,
                    &mut budget,
                )
                .await?;
                if scan.resume.is_some() && scan.objects.len() < want {
                    return Err(ListingError::ScanBudgetSpent);
                }
                (scan.objects, scan.resume)
            }
        };
        for (key, _) in objects {
            if push_listed_key(&mut keys, key, max_keys) {
                return Ok((keys, true));
            }
        }
        match next {
            Some(position) => cursor = Some(position),
            None => return Ok((keys, false)),
        }
    }
}

/// Add one visible key to a walk's result; `true` when the walk holds more
/// keys than `max` (B096: exactly `max` is not truncated). The extra key is
/// not kept.
pub(crate) fn push_listed_key(keys: &mut Vec<String>, key: String, max: usize) -> bool {
    if keys.len() == max {
        return true;
    }
    keys.push(key);
    false
}

/// Pure: what one engine page of the scan spends of its budget: the
/// requests it sent to the backend, at least one. A backend that sends no
/// request (the filesystem) still spends one per page, so the scan ends.
/// The page that spends the last of the budget can go past it by its own
/// extra requests (a second backend page, the probes), never by a page.
fn scan_cost(requests: u32) -> usize {
    (requests as usize).max(1)
}

/// Pure: `true` when no key under `prefix` sorts after `cursor`, so a scan
/// of `prefix` cannot add to a page that starts after `cursor`. A key under
/// `prefix` starts with `prefix`; a cursor that sorts after `prefix` and
/// does not start with it differs from it at an earlier byte, where it is
/// greater, so it sorts after every such key.
fn range_is_past(prefix: &str, cursor: Option<&str>) -> bool {
    cursor.is_some_and(|c| c > prefix && !c.starts_with(prefix))
}

/// The policy a filtered listing filters with.
struct Visibility<'a> {
    user: &'a crate::iam::AuthenticatedUser,
    context: &'a iam_rs::Context,
}

impl<'a> Visibility<'a> {
    fn of(scope: Option<&'a ListScope>) -> Option<Self> {
        match scope {
            Some(ListScope::Filtered { user, context }) => Some(Self { user, context }),
            _ => None,
        }
    }
}

/// What a filtered scan read.
struct Scanned {
    /// The visible objects and common prefixes, in key order. There can be
    /// more than `want`: the scan keeps every visible entry of the engine
    /// pages it read.
    objects: Vec<(String, crate::types::FileMetadata)>,
    prefixes: std::collections::BTreeSet<String>,
    facts_missing: std::collections::HashSet<String>,
    /// `None`: the listing ended. Otherwise the scan position: every key at
    /// or before it was read, and keys after it may remain. The scan stops
    /// with a position when it holds `want` visible entries, or when the
    /// budget runs out first.
    resume: Option<String>,
}

/// The scan of a filtered listing: the visible entries after `cursor`,
/// until it holds `want` of them, the listing ends, or `budget` runs out.
///
/// ONE request budget for the whole request (or folder walk), shared by
/// every target (scans and roll-up probes): per target, N visible prefixes
/// read N budgets for one LIST.
///
/// The scan position always moves past `cursor` before the budget can stop
/// the scan, so a client that follows the tokens always gets to the end. A
/// roll-up target that the scan starts is finished: its probes are bounded
/// by the policy, not by the data, and a position inside it cannot be
/// written as a key.
#[allow(clippy::too_many_arguments)]
async fn scan_visible<L: Lister>(
    lister: &L,
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    cursor: Option<&str>,
    user: &crate::iam::AuthenticatedUser,
    context: &iam_rs::Context,
    want: usize,
    budget: &mut usize,
) -> Result<Scanned, EngineError> {
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
    let mut scan = Scanned {
        objects: Vec::new(),
        prefixes: std::collections::BTreeSet::new(),
        facts_missing: std::collections::HashSet::new(),
        resume: None,
    };
    // Where the scan is: every key at or before it was read.
    let mut position: Option<String> = cursor.map(str::to_string);
    let moved = |position: &Option<String>| position.as_deref() != cursor;
    let found = |scan: &Scanned| scan.objects.len() + scan.prefixes.len();
    for target in targets {
        // A target that cannot add to this page spends no request and no
        // budget: the cursor is past it (an earlier page listed it), or its
        // roll-up entry is one the user may not see.
        let adds_nothing = match &target {
            ListTarget::Scan(scan_prefix) => range_is_past(scan_prefix, position.as_deref()),
            ListTarget::Rollup { common_prefix, .. } => {
                position
                    .as_deref()
                    .is_some_and(|c| common_prefix.as_str() <= c)
                    || !user_can_see_common_prefix(user, bucket, common_prefix, context)
            }
        };
        if adds_nothing {
            continue;
        }
        if found(&scan) >= want || (*budget == 0 && moved(&position)) {
            // Stopped before this target: the listing goes on after the
            // scan position.
            scan.resume = position;
            return Ok(scan);
        }
        match target {
            ListTarget::Rollup {
                common_prefix,
                probes,
            } => {
                for probe in probes {
                    let (page, requests) =
                        lister.list_lite(bucket, &probe, delimiter, 1, None).await?;
                    *budget = budget.saturating_sub(scan_cost(requests));
                    if !page.objects.is_empty() || !page.common_prefixes.is_empty() {
                        scan.prefixes.insert(common_prefix.clone());
                        break;
                    }
                }
                position = Some(common_prefix);
            }
            ListTarget::Scan(scan_prefix) => loop {
                if found(&scan) >= want || (*budget == 0 && moved(&position)) {
                    // Stopped inside this scan: the listing goes on after
                    // the scan position.
                    scan.resume = position;
                    return Ok(scan);
                }
                // Lite: no listing facts and no HEAD for the hidden keys.
                let (page, requests) = lister
                    .list_lite(
                        bucket,
                        &scan_prefix,
                        delimiter,
                        FILTERED_SCAN_PAGE_KEYS,
                        position.as_deref(),
                    )
                    .await?;
                *budget = budget.saturating_sub(scan_cost(requests));
                let more = page.is_truncated && page.next_continuation_token.is_some();
                // The last entry of the page, in key order: the engine's
                // token on a truncated page.
                let last = page
                    .objects
                    .last()
                    .map(|(k, _)| k.clone())
                    .into_iter()
                    .chain(page.common_prefixes.last().cloned())
                    .max();
                scan.facts_missing.extend(page.facts_missing_keys);
                scan.objects.extend(
                    page.objects
                        .into_iter()
                        .filter(|(key, _)| user_can_see_listed_key(user, bucket, key, context)),
                );
                scan.prefixes.extend(
                    page.common_prefixes
                        .into_iter()
                        .filter(|p| user_can_see_common_prefix(user, bucket, p, context)),
                );
                if more {
                    position = page.next_continuation_token;
                } else {
                    if last.is_some() {
                        position = last;
                    }
                    break;
                }
            },
        }
    }
    // Every target read to its end.
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// The budget is exactly `budget` engine requests of 1000 keys,
    /// whatever `max_keys` is: a scan of hidden keys stops after them with
    /// an empty truncated page whose cursor is the scan position, and a
    /// budget that reaches the visible key returns it.
    #[tokio::test]
    async fn the_scan_makes_exactly_budget_engine_requests() {
        let mut keys: Vec<String> = (0..5000).map(|i| format!("h/{i:04}")).collect();
        keys.push("v".into());
        let lister = counting(keys);
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![read("Allow", "b/*"), read("Deny", "b/h/*")]);
        let got = list_page_for_caller(&lister, "b", "", None, 1, None, false, Some(&scope), 2)
            .await
            .unwrap();
        assert!(got.page.objects.is_empty() && got.page.is_truncated);
        assert!(got.hidden_cursor);
        assert_eq!(got.page.next_continuation_token.as_deref(), Some("h/1999"));
        assert_eq!(calls(&lister), 2);
        let got = list_page_for_caller(&lister, "b", "", None, 1, None, false, Some(&scope), 6)
            .await
            .unwrap();
        assert_eq!(got.page.objects[0].0, "v");
        assert!(!got.page.is_truncated && !got.hidden_cursor);
        assert_eq!(calls(&lister), 2 + 6);
    }

    /// What a client sees of one LIST answer: the key count and the
    /// truncation flag (an error counts as its own shape).
    async fn shape<L: Lister>(
        lister: &L,
        scope: &ListScope,
        max_keys: u32,
        start_after: &str,
        budget: usize,
    ) -> Option<(usize, bool)> {
        list_page_for_caller(
            lister,
            "b",
            "",
            None,
            max_keys,
            Some(start_after),
            false,
            Some(scope),
            budget,
        )
        .await
        .ok()
        .map(|p| p.page)
        .map(|p| (p.objects.len() + p.common_prefixes.len(), p.is_truncated))
    }

    /// Every `start-after` an attacker bisects with: `h/` followed by one
    /// to three characters of the key alphabet.
    fn bisection_points() -> Vec<String> {
        let alphabet: Vec<char> = "/0123456789".chars().collect();
        let mut out = vec!["h/".to_string()];
        for a in &alphabet {
            out.push(format!("h/{a}"));
            for b in &alphabet {
                out.push(format!("h/{a}{b}"));
                for c in &alphabet {
                    out.push(format!("h/{a}{b}{c}"));
                }
            }
        }
        out
    }

    /// Oracle 1 (budget bisection): the budget error came after
    /// `budget × (max_keys + 1)` hidden keys, so a bisection on
    /// `start-after` found the hidden key where it started to fire. Below
    /// the threshold (`budget × 1000` hidden keys) every `start-after`
    /// inside the hidden run must give the same answer.
    #[tokio::test]
    async fn bisecting_start_after_on_the_scan_budget_gives_no_signal() {
        let mut keys: Vec<String> = (0..3000).map(|i| format!("h/{i:04}")).collect();
        keys.push("v".into());
        let lister = counting(keys);
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![read("Allow", "b/*"), read("Deny", "b/h/*")]);
        let mut shapes = std::collections::BTreeMap::new();
        for point in bisection_points() {
            let s = shape(&lister, &scope, 1, &point, 10).await;
            shapes.entry(s).or_insert_with(Vec::new).push(point);
        }
        assert_eq!(
            shapes.keys().collect::<Vec<_>>(),
            vec![&Some((1, false))],
            "the answer depends on the hidden keys: {:?}",
            shapes
                .iter()
                .map(|(s, p)| (s, p.first(), p.len()))
                .collect::<Vec<_>>()
        );
    }

    /// Oracle 2 (page-size flip): a filtered page with one visible entry
    /// was served short, so the key count of a page told how many hidden
    /// keys sat in the engine window after `start-after`.
    #[tokio::test]
    async fn bisecting_start_after_on_the_page_size_gives_no_signal() {
        let mut keys: Vec<String> = (0..10).map(|i| format!("h/{i:02}")).collect();
        keys.extend(["v".to_string(), "w".to_string()]);
        let lister = counting(keys);
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![read("Allow", "b/*"), read("Deny", "b/h/*")]);
        let mut shapes = std::collections::BTreeMap::new();
        for point in bisection_points() {
            let s = shape(&lister, &scope, 2, &point, 1000).await;
            shapes.entry(s).or_insert_with(Vec::new).push(point);
        }
        assert_eq!(
            shapes.keys().collect::<Vec<_>>(),
            vec![&Some((2, false))],
            "the answer depends on the hidden keys: {:?}",
            shapes
                .iter()
                .map(|(s, p)| (s, p.first(), p.len()))
                .collect::<Vec<_>>()
        );
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
            .unwrap()
            .page;
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
        let user = AuthenticatedUser::from_parts(
            "u".into(),
            "AK".into(),
            vec![allow.clone()],
            vec![permission_to_iam_policy(&allow)],
        );
        let scope = ListScope::Filtered {
            user: std::sync::Arc::new(user),
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
        .unwrap()
        .page;
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
            p2.expect("d/z/ must stay reachable").page.common_prefixes,
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
        let user = crate::iam::AuthenticatedUser::new("u", "AK", perms);
        ListScope::Filtered {
            user: std::sync::Arc::new(user),
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
            .unwrap()
            .page;
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
    /// stays the fallback. On the real engine, a run of hidden keys past the
    /// budget gives an empty truncated page whose cursor is the scan
    /// position (sealed by the S3 adapter), and that cursor reaches the
    /// visible key behind the run.
    #[tokio::test]
    async fn bucket_wide_allow_with_a_large_deny_keeps_the_budget() {
        let mut keys: Vec<String> = (0..=FILTERED_SCAN_PAGE_KEYS)
            .map(|i| format!("h/{i:04}.png"))
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
        let first = list_page_for_caller(&engine, "b", "", None, 1, None, false, Some(&scope), 1)
            .await
            .unwrap();
        assert!(first.page.objects.is_empty() && first.page.is_truncated);
        assert!(first.hidden_cursor);
        let cursor = first.page.next_continuation_token.unwrap();
        let next = list_page_for_caller(
            &engine,
            "b",
            "",
            None,
            1,
            Some(&cursor),
            false,
            Some(&scope),
            1,
        )
        .await
        .unwrap();
        let keys: Vec<&str> = next.page.objects.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["v.png"]);
        assert!(!next.page.is_truncated);
    }

    /// The page budget is one per REQUEST, not one per visible prefix: a
    /// scan of hidden keys that ends inside the budget left the next prefix
    /// a whole new budget, so a policy with N prefixes read N budgets of
    /// engine pages for one LIST. Here `a/` spends the whole budget on
    /// hidden keys, so `b/` must not be read.
    #[tokio::test]
    async fn review3_one_page_budget_per_request() {
        let mut keys: Vec<String> = (0..3000).map(|i| format!("a/{i:04}.png")).collect();
        keys.push("b/v.png".into());
        let lister = counting(keys);
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![
            read("Allow", "b/a/*"),
            read("Allow", "b/b/*"),
            read("Deny", "b/a/*"),
        ]);
        let got = list_page_for_caller(&lister, "b", "", None, 1, None, false, Some(&scope), 2)
            .await
            .unwrap();
        assert!(
            got.page.objects.is_empty() && got.hidden_cursor,
            "the request read past its page budget: {:?}",
            got.page.objects.iter().map(|(k, _)| k).collect::<Vec<_>>()
        );
        assert_eq!(calls(&lister), 2);
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
        let user = crate::iam::AuthenticatedUser::new("u", "AK", perms);
        let scope = ListScope::Filtered {
            user: std::sync::Arc::new(user),
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
        .expect("the listing must not fail on the write-only prefix")
        .page;
        assert_eq!(
            page.objects.first().map(|(k, _)| k.as_str()),
            Some("releases/a.png")
        );
    }

    fn counting(keys: Vec<String>) -> CountingLister {
        CountingLister {
            keys,
            calls: Default::default(),
        }
    }

    fn calls(lister: &CountingLister) -> usize {
        lister.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Review D7: a folder walk spends ONE budget across its pages, and a
    /// walk that spends it before the end fails: the visible keys of the
    /// first pages are never returned as the whole folder.
    #[tokio::test]
    async fn a_folder_walk_shares_one_budget_and_never_returns_a_partial_folder() {
        let mut keys: Vec<String> = (0..1000).map(|i| format!("a/{i:04}")).collect();
        keys.extend((0..3000).map(|i| format!("h/{i:04}")));
        keys.push("z/0".into());
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![read("Allow", "b/*"), read("Deny", "b/h/*")]);
        let lister = counting(keys);
        let err = list_keys_for_caller(&lister, "b", "", Some(&scope), 5000, 2)
            .await
            .unwrap_err();
        assert!(matches!(err, ListingError::ScanBudgetSpent), "{err:?}");
        assert_eq!(calls(&lister), 2, "the budget is for the whole walk");
        let (keys, truncated) = list_keys_for_caller(&lister, "b", "", Some(&scope), 5000, 10)
            .await
            .unwrap();
        assert_eq!(keys.len(), 1001);
        assert_eq!(keys.last().map(String::as_str), Some("z/0"));
        assert!(!truncated);
    }

    /// A folder the user cannot reach reads no engine page.
    #[tokio::test]
    async fn a_folder_walk_outside_the_grant_reads_nothing() {
        let lister = counting((0..10).map(|i| format!("y/{i}")).collect());
        let scope = scoped(vec![rule("Allow", &["b/x/*"])]);
        let (keys, truncated) = list_keys_for_caller(&lister, "b", "y/", Some(&scope), 5000, 10)
            .await
            .unwrap();
        assert!(keys.is_empty() && !truncated);
        assert_eq!(calls(&lister), 0);
    }

    /// `max_keys` keys exactly is not truncated; one more is (B096).
    #[tokio::test]
    async fn a_folder_walk_stops_one_key_past_max() {
        let lister = counting((0..5).map(|i| format!("k/{i}")).collect());
        let (keys, truncated) = list_keys_for_caller(&lister, "b", "k/", None, 5, 1)
            .await
            .unwrap();
        assert_eq!((keys.len(), truncated), (5, false));
        let (keys, truncated) = list_keys_for_caller(&lister, "b", "k/", None, 4, 1)
            .await
            .unwrap();
        assert_eq!((keys.len(), truncated), (4, true));
    }

    /// A visible prefix whose whole range is at or before the cursor holds
    /// nothing for the page: it costs no request. Before, every earlier
    /// prefix sent one LIST, so a user with more visible prefixes than the
    /// budget could not page past about `budget` of them.
    #[tokio::test]
    async fn prefixes_before_the_cursor_cost_no_request() {
        let lister = counting((0..60).map(|i| format!("t{i:02}/k")).collect());
        let res: Vec<String> = (0..60).map(|i| format!("b/t{i:02}/*")).collect();
        let res: Vec<&str> = res.iter().map(String::as_str).collect();
        let scope = scoped(vec![rule("Allow", &res)]);
        let mut cursor: Option<String> = None;
        let mut seen = Vec::new();
        loop {
            let page = list_page_for_caller(
                &lister,
                "b",
                "",
                None,
                1,
                cursor.as_deref(),
                false,
                Some(&scope),
                50,
            )
            .await
            .unwrap_or_else(|e| panic!("after {} keys at {cursor:?}: {e:?}", seen.len()))
            .page;
            seen.extend(page.objects.into_iter().map(|(k, _)| k));
            if !page.is_truncated {
                break;
            }
            cursor = page.next_continuation_token;
        }
        assert_eq!(seen.len(), 60);
        // The folder walk shares one budget over its pages: page 2 must not
        // read again the prefixes that page 1 finished.
        let mut keys: Vec<String> = (0..4)
            .flat_map(|p| (0..250).map(move |i| format!("t{p:02}/{i:03}")))
            .collect();
        keys.extend((4..10).map(|p| format!("t{p:02}/k")));
        let lister = counting(keys);
        let res: Vec<String> = (0..10).map(|i| format!("b/t{i:02}/*")).collect();
        let res: Vec<&str> = res.iter().map(String::as_str).collect();
        let scope = scoped(vec![rule("Allow", &res)]);
        let (keys, truncated) = list_keys_for_caller(&lister, "b", "", Some(&scope), 5000, 12)
            .await
            .unwrap();
        assert_eq!((keys.len(), truncated), (1006, false));
        // One engine page per prefix: the walk goes on from its scan
        // position, so it reads no engine page twice.
        assert_eq!(calls(&lister), 10);
    }

    #[test]
    fn a_scan_range_is_past_only_when_every_key_sorts_at_or_before_the_cursor() {
        assert!(!range_is_past("a/", None));
        assert!(!range_is_past("a/", Some("a/")));
        assert!(!range_is_past("a/", Some("a/zz")));
        assert!(!range_is_past("a/", Some("a")));
        assert!(!range_is_past("", Some("zzz")));
        assert!(range_is_past("a/", Some("a0")));
        assert!(range_is_past("a/", Some("b")));
    }

    proptest::proptest! {
        /// No key of a skipped range sorts after the cursor.
        #[test]
        fn a_skipped_range_holds_no_key_after_the_cursor(
            prefix in "[ab/]{0,3}",
            suffix in "[ab/]{0,3}",
            cursor in "[ab/]{0,5}",
        ) {
            let key = format!("{prefix}{suffix}");
            if range_is_past(&prefix, Some(&cursor)) {
                proptest::prop_assert!(key.as_str() <= cursor.as_str());
            }
        }
    }

    /// The scan budget counts the backend requests of the scan, not its
    /// engine pages: a page that costs a LIST and a probe (for a `.delta`
    /// form that sorts after the page) spends two. A budget of 4 engine
    /// pages sent 5 requests.
    #[tokio::test]
    async fn the_scan_budget_counts_backend_requests() {
        use crate::usage_scanner::test_support::{fake_s3_engine, put_raw_many, scope_lists};
        let (engine, fake, endpoint) = fake_s3_engine().await;
        // Hidden keys whose first page probes for `h/a.delta`.
        put_raw_many(&endpoint, (0..5000).map(|i| format!("h/a-{i:04}"))).await;
        put_raw_many(&endpoint, ["v/x".to_string()]).await;
        let scope = scoped(vec![rule("Allow", &["b/*"]), rule("Deny", &["b/h/*"])]);
        fake.clear();
        let budget = 4;
        let got =
            list_page_for_caller(&engine, "b", "", None, 1, None, false, Some(&scope), budget)
                .await
                .unwrap();
        assert!(got.page.objects.is_empty() && got.hidden_cursor);
        let lists = scope_lists(&fake);
        eprintln!(
            "a budget of {budget}: {} backend LISTs: {lists:?}",
            lists.len()
        );
        assert!(
            lists.len() <= budget,
            "a budget of {budget} sent {} backend LISTs: {lists:?}",
            lists.len()
        );
    }

    /// Walk a listing to its end with `max_keys=1000`; the backend LISTs
    /// each page sent, and the entries.
    async fn walk_counting_lists(
        engine: &crate::deltaglider::DynEngine,
        fake: &crate::storage::FakeS3,
        prefix: &str,
        scope: Option<&ListScope>,
    ) -> (Vec<usize>, usize) {
        let mut token: Option<String> = None;
        let (mut per_page, mut entries) = (Vec::new(), 0);
        for _ in 0..100 {
            fake.clear();
            let page = list_page_for_caller(
                engine,
                "b",
                prefix,
                None,
                1000,
                token.as_deref(),
                false,
                scope,
                50,
            )
            .await
            .unwrap()
            .page;
            per_page.push(crate::usage_scanner::test_support::scope_lists(fake).len());
            entries += page.objects.len();
            if !page.is_truncated {
                return (per_page, entries);
            }
            token = page.next_continuation_token;
        }
        panic!("listing did not end");
    }

    /// The cost of a filtered page (review 2d). A filtered page is exact:
    /// it holds `max_keys` visible entries whenever that many exist, so its
    /// size never tells how many hidden keys sat in an engine window. To
    /// know that the page is truncated, the scan must see one visible
    /// entry past it, so a full page of 1000 reads one more engine page of
    /// 1000 keys, and the next page reads that engine page again. Before,
    /// a filtered page was served short to save that read, and the short
    /// page was the oracle. 2,500 visible keys cost 6 backend LISTs
    /// filtered and 4 unfiltered, in the same 3 client pages: at most one
    /// more engine page (a LIST, and maybe a probe) per truncated page.
    #[tokio::test]
    async fn a_filtered_scan_costs_one_engine_page_more_per_truncated_page() {
        use crate::usage_scanner::test_support::{fake_s3_engine, put_raw_many};
        let (engine, fake, endpoint) = fake_s3_engine().await;
        put_raw_many(&endpoint, (0..2500).map(|i| format!("rel/img-{i:05}.jpg"))).await;
        // A bucket-wide Allow with a carve-out: the budgeted scan.
        let scope = scoped(vec![rule("Allow", &["b/*"]), rule("Deny", &["b/h/*"])]);
        let (plain, n_plain) = walk_counting_lists(&engine, &fake, "rel/", None).await;
        let (filtered, n_filtered) =
            walk_counting_lists(&engine, &fake, "rel/", Some(&scope)).await;
        assert_eq!((n_plain, n_filtered), (2500, 2500));
        assert_eq!(filtered.len(), plain.len(), "the same client pages");
        let truncated_pages = filtered.len() - 1;
        assert!(
            filtered.iter().sum::<usize>() <= plain.iter().sum::<usize>() + 2 * truncated_pages,
            "backend LISTs per page: filtered {filtered:?}, unfiltered {plain:?}"
        );
    }

    /// Hidden keys interleaved with visible ones, around every page
    /// boundary: each truncated page is FULL (review 2d: a short page told
    /// how many hidden keys followed `start-after`), its token is the last
    /// visible key it shows, and the walk returns exactly the visible keys.
    #[tokio::test]
    async fn filtered_pages_are_exact_and_never_anchor_on_a_hidden_key() {
        // Hidden `a/1/` between visible `a/0/` and `a/2/`, and so on: the
        // lister serves keys in this (sorted) order.
        let keys: Vec<String> = (0..40)
            .map(|i| match i % 3 {
                0 => format!("a/{i:03}/h"),
                _ => format!("a/{i:03}/v"),
            })
            .collect();
        let visible: Vec<String> = keys.iter().filter(|k| k.ends_with("/v")).cloned().collect();
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![read("Allow", "b/*"), read("Deny", "b/a/*/h")]);
        for max_keys in 1..=6 {
            let lister = counting(keys.clone());
            let mut got = Vec::new();
            let mut token: Option<String> = None;
            for _ in 0..200 {
                let listed = list_page_for_caller(
                    &lister,
                    "b",
                    "",
                    None,
                    max_keys,
                    token.as_deref(),
                    false,
                    Some(&scope),
                    TEST_BUDGET,
                )
                .await
                .unwrap();
                assert!(!listed.hidden_cursor);
                let page = listed.page;
                let shown: Vec<String> = page.objects.into_iter().map(|(k, _)| k).collect();
                if page.is_truncated {
                    assert_eq!(shown.len(), max_keys as usize, "a short truncated page");
                    assert_eq!(page.next_continuation_token.as_ref(), shown.last());
                } else {
                    assert!(shown.len() <= max_keys as usize);
                }
                got.extend(shown);
                if !page.is_truncated {
                    break;
                }
                token = page.next_continuation_token;
            }
            assert_eq!(got, visible, "max_keys={max_keys}");
        }
    }

    /// Clients loop on `IsTruncated` and the token: past runs of hidden
    /// keys longer than the budget, every token moves forward and the
    /// listing ends with exactly the visible keys, whatever `max_keys` is.
    #[tokio::test]
    async fn a_filtered_listing_past_the_budget_always_moves_forward_and_ends() {
        let mut keys: Vec<String> = vec!["a".into(), "m".into(), "z".into()];
        keys.extend((0..2500).map(|i| format!("h/{i:04}")));
        keys.extend((0..1500).map(|i| format!("n/{i:04}")));
        keys.sort();
        let lister = counting(keys);
        let read = |effect: &str, res: &str| crate::iam::Permission {
            actions: vec!["read".into()],
            ..rule(effect, &[res])
        };
        let scope = scoped(vec![
            read("Allow", "b/*"),
            read("Deny", "b/h/*"),
            read("Deny", "b/n/*"),
        ]);
        for max_keys in [1, 2, 1000] {
            for budget in [0, 1, 2] {
                let mut got = Vec::new();
                let mut token: Option<String> = None;
                let mut sealed_pages = 0;
                for round in 0.. {
                    assert!(round < 100, "the listing did not end");
                    let listed = list_page_for_caller(
                        &lister,
                        "b",
                        "",
                        None,
                        max_keys,
                        token.as_deref(),
                        false,
                        Some(&scope),
                        budget,
                    )
                    .await
                    .unwrap();
                    sealed_pages += usize::from(listed.hidden_cursor);
                    let page = listed.page;
                    got.extend(page.objects.into_iter().map(|(k, _)| k));
                    if !page.is_truncated {
                        break;
                    }
                    let next = page
                        .next_continuation_token
                        .expect("a truncated page has a token");
                    assert!(
                        token.as_deref().is_none_or(|t| next.as_str() > t),
                        "the token did not move: {token:?} -> {next:?}"
                    );
                    token = Some(next);
                }
                assert_eq!(got, ["a", "m", "z"], "max_keys={max_keys} budget={budget}");
                assert!(sealed_pages > 0, "max_keys={max_keys} budget={budget}");
            }
        }
    }

    /// Cockroach scan (engine.md "filtered-LIST budget"): the scan loop
    /// listed with `metadata=true`, so it HEADed (and read the listing facts
    /// of) every hidden key it passed, before the filter dropped it.
    #[tokio::test]
    async fn a_filtered_metadata_listing_heads_only_the_page_it_returns() {
        use crate::usage_scanner::test_support::{
            facts_lists, fake_s3_engine, heads, put_raw_many,
        };
        let (engine, fake, endpoint) = fake_s3_engine().await;
        put_raw_many(&endpoint, (0..40).map(|i| format!("a/h-{i:03}.zip.delta"))).await;
        put_raw_many(&endpoint, (0..3).map(|i| format!("v/{i}.zip"))).await;
        let scope = scoped(vec![rule("Allow", &["b/*"]), rule("Deny", &["b/a/*"])]);
        fake.clear();
        let page = list_page_for_caller(&engine, "b", "", None, 2, None, true, Some(&scope), 64)
            .await
            .unwrap()
            .page;
        let keys: Vec<&str> = page.objects.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["v/0.zip", "v/1.zip"]);
        assert!(
            heads(&fake) <= 2,
            "{} HEADs for a page of 2 visible keys behind 40 hidden ones",
            heads(&fake)
        );
        assert_eq!(facts_lists(&fake), 0, "listing facts read for hidden keys");
    }
}
