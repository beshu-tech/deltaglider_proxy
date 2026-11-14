// SPDX-License-Identifier: BUSL-1.1

//! The listing algebra of a prefix-scoped caller (review S12).
//!
//! A `ListScope::Filtered` caller may see only some keys of a bucket. Its
//! LIST reads only the prefixes its policy can see (`list_targets`), merges
//! them in key order, filters every entry per key, and never anchors a
//! continuation token on a hidden key. The engine is reached through the
//! one-method [`Lister`], so the algebra is tested without storage.

use crate::deltaglider::{EngineError, ListObjectsPage};
use crate::iam::{
    deny_may_hide_keys_under, user_can_see_common_prefix, user_can_see_listed_key, ListScope,
};
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

    /// [`Self::list`] for a page that a client waits for: a delta without
    /// listing facts is HEADed on the page's path (see
    /// `DeltaGliderEngine::finish_listed_page_for_client`). The default is
    /// [`Self::list`]: only the engine HEADs.
    #[allow(clippy::too_many_arguments)]
    fn list_for_client(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        cursor: Option<&str>,
        metadata: bool,
    ) -> impl Future<Output = Result<ListObjectsPage, EngineError>> + Send {
        self.list(bucket, prefix, delimiter, max_keys, cursor, metadata)
    }

    /// [`Self::finish`] for a page that a client waits for; the default is
    /// [`Self::finish`].
    fn finish_for_client(
        &self,
        bucket: &str,
        page: ListObjectsPage,
        metadata: bool,
    ) -> impl Future<Output = Result<ListObjectsPage, EngineError>> + Send {
        self.finish(bucket, page, metadata)
    }

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

    async fn list_for_client(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        cursor: Option<&str>,
        metadata: bool,
    ) -> Result<ListObjectsPage, EngineError> {
        self.list_objects_for_client(bucket, prefix, delimiter, max_keys, cursor, metadata)
            .await
    }

    async fn finish_for_client(
        &self,
        bucket: &str,
        mut page: ListObjectsPage,
        metadata: bool,
    ) -> Result<ListObjectsPage, EngineError> {
        self.finish_listed_page_for_client(bucket, &mut page, metadata)
            .await?;
        Ok(page)
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
    /// The proof of a folded name that this page left unfinished (it is
    /// `None` for a plain scan position). It travels with the token, sealed,
    /// and the next page resumes the proof there.
    pub proof: Option<ProofResume>,
}

/// Where the proof of one folded name stands (see [`visible_key_under`]):
/// every key at or before `inner` under the name was read, and none of them
/// is visible. `inner` is `None` when the proof has not read a key yet. The
/// value names hidden keys, so it leaves the process only sealed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProofResume {
    /// The folded name, or for a roll-up the visible prefix being proved.
    pub name: String,
    pub inner: Option<String>,
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
    // A delimiter of several characters can straddle the end of a visible
    // prefix (`se` + `cret/` at delimiter `ec`): listing the prefix would
    // then fold at other points than listing the request. Only a one-
    // character delimiter folds the same way, so read the request itself.
    if delimiter.is_some_and(|d| d.chars().count() > 1) {
        return vec![ListTarget::Scan(prefix.to_string())];
    }
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

/// What the proof of one folded name found.
#[derive(Debug, PartialEq, Eq)]
enum Proof {
    /// A key that the user may see lies under the name.
    Visible,
    /// Every key under the name is hidden: the name is not shown.
    NotVisible,
    /// The budget ran out before the end of the keys under the name. The
    /// name is not decided: the caller ends its page and the next page goes
    /// on from `inner` (the last key read, `None` when none was read).
    Undecided { inner: Option<String> },
}

/// Does a key that `user` may see lie under `folded` (a name a delimiter
/// folded keys into)? Reads the keys under it, lite, within the request's
/// scan budget. THE rule for a folded name that a Deny may touch
/// ([`deny_may_hide_keys_under`]): the name of a protected key is shown only
/// when a visible key lies under it too (S3 shows a folded name only for
/// keys the caller may list). A proof that the budget cannot finish is
/// [`Proof::Undecided`]: the scan then stops before the name and the next
/// page resumes the proof at `inner` (`from`). A name is never decided by
/// the budget, only by the keys.
///
/// A resumed proof (`resumed`) reads at least one engine page whatever the
/// budget is, so its position strictly advances in every request and the
/// proof ends.
///
/// The engine refuses a prefix with a `..` segment, and a folded name can
/// end in one (`a/..` of `a/..x/k` at delimiter `.`). The proof then reads
/// the parent folder and keeps the keys that start with the name.
async fn visible_key_under<L: Lister>(
    lister: &L,
    bucket: &str,
    folded: &str,
    user: &crate::iam::AuthenticatedUser,
    context: &iam_rs::Context,
    budget: &mut usize,
    resumed: Option<Option<String>>,
) -> Result<Proof, EngineError> {
    let listed = if crate::types::ObjectKey::validate_prefix(folded).is_ok() {
        folded
    } else {
        &folded[..folded.rfind('/').map_or(0, |i| i + 1)]
    };
    if crate::types::ObjectKey::validate_prefix(listed).is_err() {
        return Ok(Proof::NotVisible);
    }
    let mut at_least_one = resumed.is_some();
    let mut cursor: Option<String> = resumed.flatten();
    while *budget > 0 || at_least_one {
        at_least_one = false;
        let (page, requests) = lister
            .list_lite(
                bucket,
                listed,
                None,
                FILTERED_SCAN_PAGE_KEYS,
                cursor.as_deref(),
            )
            .await?;
        *budget = budget.saturating_sub(scan_cost(requests));
        for (key, _) in &page.objects {
            if key.starts_with(folded) {
                if user_can_see_listed_key(user, bucket, key, context) {
                    return Ok(Proof::Visible);
                }
            } else if key.as_str() > folded {
                // Keys come in order: no key under the name follows.
                return Ok(Proof::NotVisible);
            }
        }
        match page.next_continuation_token {
            Some(next) if page.is_truncated => cursor = Some(next),
            _ => return Ok(Proof::NotVisible),
        }
    }
    Ok(Proof::Undecided { inner: cursor })
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
    proof: Option<ProofResume>,
) -> Result<CallerPage, EngineError> {
    let Some(Visibility { user, context }) = Visibility::of(scope) else {
        return Ok(CallerPage {
            page: lister
                .list_for_client(bucket, prefix, delimiter, max_keys, cursor, metadata)
                .await?,
            hidden_cursor: false,
            proof: None,
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
        proof,
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
        page: lister.finish_for_client(bucket, page, metadata).await?,
        hidden_cursor: budget_spent,
        proof: if budget_spent { scan.proof } else { None },
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
    start_after: Option<&str>,
    max_keys: usize,
    budget: usize,
) -> Result<(Vec<String>, bool), ListingError> {
    let mut budget = budget;
    let mut keys = Vec::new();
    // The engine's cursor is a key: the listing starts after it.
    let mut cursor: Option<String> = start_after.filter(|k| !k.is_empty()).map(String::from);
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
                    None,
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

/// The progress of the resumed proof when it is the proof of `name`: its
/// inner cursor (`None` for a proof that has read nothing). `None` for any
/// other name.
fn take_resume(pending: &mut Option<ProofResume>, name: &str) -> Option<Option<String>> {
    if pending.as_ref().is_some_and(|p| p.name == name) {
        pending.take().map(|p| p.inner)
    } else {
        None
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
    /// With `resume`: the proof of a folded name that the scan left
    /// unfinished, which the next page goes on with.
    proof: Option<ProofResume>,
}

/// The scan of a filtered listing: the visible entries after `cursor`,
/// until it holds `want` of them, the listing ends, or `budget` runs out.
///
/// ONE request budget for the whole request (or folder walk), shared by
/// every target (scans and roll-up probes): per target, N visible prefixes
/// read N budgets for one LIST.
///
/// The scan position always moves past `cursor` before the budget can stop
/// the scan, so a client that follows the tokens always gets to the end.
///
/// The proof of a folded name (a Deny may hide keys under it) can need more
/// requests than the budget holds. The scan then stops before the name and
/// returns the proof's progress (`Scanned::proof`); the next request passes
/// it back as `resume`, and its proof reads at least one more engine page.
/// A name is never dropped for the budget, and what is shown never depends
/// on where a client starts. The progress is a hidden position and a hidden
/// name, so it leaves the process only sealed.
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
    resume: Option<ProofResume>,
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
        proof: None,
    };
    // The proof to go on with, until the scan reaches its name.
    let mut pending = resume;
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
            scan.proof = pending;
            return Ok(scan);
        }
        match target {
            ListTarget::Rollup {
                common_prefix,
                probes,
            } => {
                if deny_may_hide_keys_under(user, bucket, &common_prefix) {
                    // A Deny may protect keys under the name: one probe key
                    // could be a hidden one. Look for a visible key under
                    // each visible prefix: every visible key lies under one.
                    // A resumed proof skips the probes before its own: they
                    // held no visible key.
                    let mut skipping = pending.as_ref().is_some_and(|p| probes.contains(&p.name));
                    let mut proof = Proof::NotVisible;
                    let mut proved = String::new();
                    for probe in &probes {
                        let resumed = take_resume(&mut pending, probe);
                        if skipping && resumed.is_none() {
                            continue;
                        }
                        skipping = false;
                        proof = visible_key_under(
                            lister, bucket, probe, user, context, budget, resumed,
                        )
                        .await?;
                        if proof != Proof::NotVisible {
                            proved = probe.clone();
                            break;
                        }
                    }
                    match proof {
                        Proof::Visible => {
                            scan.prefixes.insert(common_prefix.clone());
                        }
                        // Stop before the name: the next page goes on with
                        // the proof.
                        Proof::Undecided { inner } => {
                            scan.resume = Some(position.unwrap_or_default());
                            scan.proof = Some(ProofResume {
                                name: proved,
                                inner,
                            });
                            return Ok(scan);
                        }
                        Proof::NotVisible => {}
                    }
                } else {
                    for probe in probes {
                        let (page, requests) =
                            lister.list_lite(bucket, &probe, delimiter, 1, None).await?;
                        *budget = budget.saturating_sub(scan_cost(requests));
                        if !page.objects.is_empty() || !page.common_prefixes.is_empty() {
                            scan.prefixes.insert(common_prefix.clone());
                            break;
                        }
                    }
                }
                position = Some(common_prefix);
            }
            ListTarget::Scan(scan_prefix) => loop {
                if found(&scan) >= want || (*budget == 0 && moved(&position)) {
                    // Stopped inside this scan: the listing goes on after
                    // the scan position.
                    scan.resume = position;
                    scan.proof = pending;
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
                // The names of the page, in key order, each decided in turn.
                let mut folded_names = page.common_prefixes;
                folded_names.sort();
                let mut cut: Option<(String, String, Option<String>)> = None;
                for folded in &folded_names {
                    if !user_can_see_common_prefix(user, bucket, folded, context) {
                        continue;
                    }
                    if !deny_may_hide_keys_under(user, bucket, folded) {
                        scan.prefixes.insert(folded.clone());
                        continue;
                    }
                    let resumed = take_resume(&mut pending, folded);
                    match visible_key_under(lister, bucket, folded, user, context, budget, resumed)
                        .await?
                    {
                        Proof::Visible => {
                            scan.prefixes.insert(folded.clone());
                        }
                        Proof::NotVisible => {}
                        Proof::Undecided { inner } => {
                            // The last entry before the name: where the next
                            // page starts. It lists the name first, and its
                            // proof goes on at `inner`.
                            let before = page
                                .objects
                                .iter()
                                .map(|(k, _)| k)
                                .chain(folded_names.iter())
                                .filter(|k| *k < folded)
                                .max()
                                .cloned()
                                .or_else(|| position.clone());
                            cut = Some((folded.clone(), before.unwrap_or_default(), inner));
                            break;
                        }
                    }
                }
                let below = cut.as_ref().map(|(name, _, _)| name.as_str());
                scan.objects.extend(
                    page.objects
                        .into_iter()
                        .filter(|(key, _)| below.is_none_or(|name| key.as_str() < name))
                        .filter(|(key, _)| user_can_see_listed_key(user, bucket, key, context)),
                );
                if let Some((name, before, inner)) = cut {
                    scan.resume = Some(before);
                    scan.proof = Some(ProofResume { name, inner });
                    return Ok(scan);
                }
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
        let got = list_page_for_caller(
            &lister,
            "b",
            "",
            None,
            1,
            None,
            false,
            Some(&scope),
            2,
            None,
        )
        .await
        .unwrap();
        assert!(got.page.objects.is_empty() && got.page.is_truncated);
        assert!(got.hidden_cursor);
        assert_eq!(got.page.next_continuation_token.as_deref(), Some("h/1999"));
        assert_eq!(calls(&lister), 2);
        let got = list_page_for_caller(
            &lister,
            "b",
            "",
            None,
            1,
            None,
            false,
            Some(&scope),
            6,
            None,
        )
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
            None,
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
        let page = list_page_for_caller(
            &lister,
            "b",
            "",
            None,
            10,
            None,
            false,
            Some(&scope),
            10,
            None,
        )
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
            None,
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
            None,
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
        walk_with_budget(engine, prefix, delimiter, max_keys, scope, TEST_BUDGET).await
    }

    async fn walk_with_budget(
        engine: &crate::deltaglider::DynEngine,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        scope: &ListScope,
        budget: usize,
    ) -> Vec<String> {
        pages_from(engine, prefix, delimiter, max_keys, scope, budget, None)
            .await
            .into_iter()
            .flat_map(|(entries, _)| entries)
            .collect()
    }

    /// The pages of a filtered listing, each as (sorted entries, truncated),
    /// from `start_after` to the end.
    async fn pages_from(
        engine: &crate::deltaglider::DynEngine,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        scope: &ListScope,
        budget: usize,
        start_after: Option<&str>,
    ) -> Vec<(Vec<String>, bool)> {
        let mut out = Vec::new();
        let mut token: Option<String> = start_after.map(str::to_string);
        let mut proof: Option<ProofResume> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let listed = list_page_for_caller(
                engine,
                "b",
                prefix,
                delimiter,
                max_keys,
                token.as_deref(),
                false,
                Some(scope),
                budget,
                proof.take(),
            )
            .await
            .unwrap();
            proof = listed.proof;
            let page = listed.page;
            let mut entries: Vec<String> = page.objects.into_iter().map(|(k, _)| k).collect();
            entries.extend(page.common_prefixes);
            entries.sort();
            assert!(entries.len() <= max_keys as usize);
            out.push((entries, page.is_truncated));
            if !page.is_truncated {
                return out;
            }
            token = page.next_continuation_token;
            // Every page makes progress: no state of the paging repeats.
            assert!(
                seen.insert((token.clone(), proof.clone())),
                "the paging repeated a state"
            );
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
        let first = list_page_for_caller(
            &engine,
            "b",
            "",
            None,
            1,
            None,
            false,
            Some(&scope),
            1,
            None,
        )
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
            None,
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
        let got = list_page_for_caller(
            &lister,
            "b",
            "",
            None,
            1,
            None,
            false,
            Some(&scope),
            2,
            None,
        )
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
            None,
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
        let err = list_keys_for_caller(&lister, "b", "", Some(&scope), None, 5000, 2)
            .await
            .unwrap_err();
        assert!(matches!(err, ListingError::ScanBudgetSpent), "{err:?}");
        assert_eq!(calls(&lister), 2, "the budget is for the whole walk");
        let (keys, truncated) =
            list_keys_for_caller(&lister, "b", "", Some(&scope), None, 5000, 10)
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
        let (keys, truncated) =
            list_keys_for_caller(&lister, "b", "y/", Some(&scope), None, 5000, 10)
                .await
                .unwrap();
        assert!(keys.is_empty() && !truncated);
        assert_eq!(calls(&lister), 0);
    }

    /// `max_keys` keys exactly is not truncated; one more is (B096).
    #[tokio::test]
    async fn a_folder_walk_stops_one_key_past_max() {
        let lister = counting((0..5).map(|i| format!("k/{i}")).collect());
        let (keys, truncated) = list_keys_for_caller(&lister, "b", "k/", None, None, 5, 1)
            .await
            .unwrap();
        assert_eq!((keys.len(), truncated), (5, false));
        let (keys, truncated) = list_keys_for_caller(&lister, "b", "k/", None, None, 4, 1)
            .await
            .unwrap();
        assert_eq!((keys.len(), truncated), (4, true));
    }

    /// The next page of a folder starts after the last key of the page
    /// before: a folder of any size is read page by page.
    #[tokio::test]
    async fn a_folder_walk_starts_after_the_given_key() {
        let lister = counting((0..5).map(|i| format!("k/{i}")).collect());
        let (keys, truncated) = list_keys_for_caller(&lister, "b", "k/", None, Some("k/1"), 2, 1)
            .await
            .unwrap();
        assert_eq!(
            (keys, truncated),
            (vec!["k/2".to_string(), "k/3".to_string()], true)
        );
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
                None,
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
        let (keys, truncated) =
            list_keys_for_caller(&lister, "b", "", Some(&scope), None, 5000, 12)
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
        let got = list_page_for_caller(
            &engine,
            "b",
            "",
            None,
            1,
            None,
            false,
            Some(&scope),
            budget,
            None,
        )
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
                None,
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
                    None,
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
                        None,
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
        let page = list_page_for_caller(
            &engine,
            "b",
            "",
            None,
            2,
            None,
            true,
            Some(&scope),
            64,
            None,
        )
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

    /// The oracle of a listing: the visible keys under `prefix`, folded at
    /// `delimiter` like S3 does. A name appears only when some visible key
    /// lies under it.
    fn oracle(
        keys: &[&str],
        scope: &ListScope,
        prefix: &str,
        delimiter: Option<&str>,
    ) -> Vec<String> {
        let ListScope::Filtered { user, context } = scope else {
            unreachable!()
        };
        let mut out = std::collections::BTreeSet::new();
        for key in keys {
            let Some(rest) = key.strip_prefix(prefix) else {
                continue;
            };
            if !user_can_see_listed_key(user, "b", key, context) {
                continue;
            }
            match delimiter
                .filter(|d| !d.is_empty())
                .and_then(|d| rest.find(d).map(|i| i + d.len()))
            {
                Some(end) => out.insert(format!("{prefix}{}", &rest[..end])),
                None => out.insert(key.to_string()),
            };
        }
        out.into_iter().collect()
    }

    /// F34: a custom delimiter folds `secret/k` into `sec`; the folded name
    /// is a prefix of a Deny-protected key and must not be shown when no
    /// visible key lies under it.
    #[tokio::test]
    async fn a_custom_delimiter_never_rolls_up_a_denied_name() {
        let keys = ["secret/k", "sector.txt", "pub/a", "sedan/x"];
        let (_dir, engine) = fs_engine(&keys).await;
        let scope = scoped(vec![rule("Allow", &["b/*"]), rule("Deny", &["b/secret/*"])]);
        // `sec` holds `sector.txt` (visible): shown, with `sedan/x`. `secr`, `secre` and
        // `secret` have no visible key under them: not shown.
        for (prefix, delimiter, shown) in [
            ("se", "c", vec!["sec", "sedan/x"]),
            ("se", "r", vec!["sector", "sedan/x"]),
            ("secr", "e", vec![]),
            ("secre", "t", vec![]),
        ] as [(&str, &str, Vec<&str>); 4]
        {
            let got = walk(&engine, prefix, Some(delimiter), 1000, &scope).await;
            assert_eq!(got, shown, "prefix={prefix:?} delimiter={delimiter:?}");
            assert_eq!(got, oracle(&keys, &scope, prefix, Some(delimiter)));
        }
    }

    /// A Deny with a leading wildcard may hide keys under every folded name,
    /// so each name costs a proof. A proof that the budget cannot pay must
    /// stop the page before the name, never drop the name: the next page
    /// decides it, and a client that follows the tokens lists every folder.
    #[tokio::test]
    async fn a_wildcard_deny_never_loses_a_folder_past_the_budget() {
        let keys: Vec<String> = (0..40).map(|i| format!("f{i:02}/x")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let (_dir, engine) = fs_engine(&refs).await;
        let scope = scoped(vec![rule("Allow", &["b/*"]), rule("Deny", &["b/*.pem"])]);
        let want: Vec<String> = (0..40).map(|i| format!("f{i:02}/")).collect();
        for max_keys in [1, 7, 1000] {
            assert_eq!(
                walk(&engine, "", Some("/"), max_keys, &scope).await,
                want,
                "max_keys={max_keys}"
            );
        }
    }

    /// The roll-up proof reads the user's visible prefixes, not the whole
    /// folded name: hidden keys outside the grant spend no budget.
    #[tokio::test]
    async fn a_rollup_proof_reads_only_the_visible_prefix() {
        let mut keys: Vec<String> = (0..2500).map(|i| format!("a/hid/{i:05}")).collect();
        keys.push("a/pub/y".into());
        let lister = counting(keys);
        let scope = scoped(vec![
            rule("Allow", &["b/a/pub/*"]),
            rule("Deny", &["b/a/pub/x/*"]),
        ]);
        let got = list_page_for_caller(
            &lister,
            "b",
            "",
            Some("/"),
            10,
            None,
            false,
            Some(&scope),
            2,
            None,
        )
        .await
        .unwrap()
        .page;
        assert_eq!(got.common_prefixes, vec!["a/".to_string()]);
        assert!(!got.is_truncated);
    }

    /// A folded name that ends in a `..` segment (`a/..` of `a/..x/k` at
    /// delimiter `.`) is not a prefix the engine accepts. It is a name like
    /// any other: shown when a visible key lies under it, never an error.
    #[tokio::test]
    async fn a_folded_name_with_a_dot_dot_segment_is_decided_like_any_name() {
        let scope = scoped(vec![rule("Allow", &["b/*"]), rule("Deny", &["b/*/k"])]);
        let (_dir, engine) = fs_engine(&["a/..x/k", "a/.y"]).await;
        assert_eq!(
            walk(&engine, "a/.", Some("."), 1000, &scope).await,
            ["a/.y"]
        );
        let (_dir, engine) = fs_engine(&["a/..x/k", "a/..x/j", "a/.y"]).await;
        assert_eq!(
            walk(&engine, "a/.", Some("."), 1000, &scope).await,
            ["a/..", "a/.y"]
        );
    }

    /// The keys of a folder whose hidden keys outnumber one engine page.
    fn big_hidden_folder(name: &str, hidden: usize, visible: &[&str]) -> Vec<String> {
        let mut keys: Vec<String> = (0..hidden).map(|i| format!("{name}/{i:04}.pem")).collect();
        keys.extend(visible.iter().map(|v| format!("{name}/{v}")));
        keys
    }

    /// A proof that one request cannot finish goes on in the next page. With
    /// the smallest budget (one backend request) and a small page, a wildcard
    /// Deny over many folders and one folder with more hidden keys than the
    /// budget reads: every allowed folder comes exactly once, no hidden one
    /// comes, and the paging ends.
    #[tokio::test]
    async fn a_proof_past_the_budget_goes_on_in_the_next_page() {
        let mut keys: Vec<String> = (0..6).map(|i| format!("a{i}/x")).collect();
        keys.extend(big_hidden_folder("h", 1100, &["zz"]));
        keys.push("m/only.pem".into());
        keys.push("z/y".into());
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let (_dir, engine) = fs_engine(&refs).await;
        let scope = scoped(vec![rule("Allow", &["b/*"]), rule("Deny", &["b/*.pem"])]);
        let want: Vec<String> = ["a0/", "a1/", "a2/", "a3/", "a4/", "a5/", "h/", "z/"]
            .map(String::from)
            .to_vec();
        assert_eq!(oracle(&refs, &scope, "", Some("/")), want);
        for budget in [1, 2] {
            for max_keys in [1, 2, 3] {
                assert_eq!(
                    walk_with_budget(&engine, "", Some("/"), max_keys, &scope, budget).await,
                    want,
                    "budget={budget} max_keys={max_keys}"
                );
            }
        }
    }

    /// `start-after` is the client's. The answer for a start point must not
    /// change at a hidden key (review 2d, re-review): the first page is the
    /// same for every start between visible entries, whatever hidden keys lie
    /// there, and the whole listing is the oracle's names after the start.
    #[tokio::test]
    async fn start_after_probing_around_hidden_keys_shows_no_difference() {
        let mut keys: Vec<String> = vec!["a/x".into(), "e.secret".into(), "z/y".into()];
        keys.extend(big_hidden_folder("h", 1100, &[]));
        keys.sort();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let (_dir, engine) = fs_engine(&refs).await;
        let scope = scoped(vec![
            rule("Allow", &["b/*"]),
            rule("Deny", &["b/h/*.pem"]),
            rule("Deny", &["b/e.*"]),
        ]);
        let all = oracle(&refs, &scope, "", Some("/"));
        assert_eq!(all, ["a/", "z/"]);
        let mut first_shapes = Vec::new();
        for start in ["", "a/x", "d", "e.secres", "e.secret", "e.secreu", "g"] {
            let pages = pages_from(&engine, "", Some("/"), 10, &scope, 2, Some(start)).await;
            let got: Vec<String> = pages.iter().flat_map(|(e, _)| e.clone()).collect();
            let want: Vec<String> = all.iter().filter(|n| n.as_str() > start).cloned().collect();
            assert_eq!(got, want, "start={start:?}");
            if ["d", "e.secres", "e.secret", "e.secreu", "g"].contains(&start) {
                first_shapes.push((start, pages[0].clone()));
            }
        }
        let first = &first_shapes[0].1;
        for (start, shape) in &first_shapes {
            assert_eq!(shape, first, "the first page differs at start={start:?}");
        }
    }

    /// The roll-up arm resumes too: a visible prefix whose Deny hides more
    /// keys than the budget reads, followed by a second prefix that holds
    /// the visible key. One backend request per page, and `c/` still comes.
    #[tokio::test]
    async fn a_rollup_proof_past_the_budget_goes_on_in_the_next_page() {
        let mut keys = big_hidden_folder("c/a", 1100, &[]);
        keys.push("c/pub/k".into());
        keys.push("zz".into());
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let (_dir, engine) = fs_engine(&refs).await;
        let scope = scoped(vec![
            rule("Allow", &["b/c/a/*", "b/c/pub/*"]),
            rule("Deny", &["b/c/a/*.pem", "b/c/pub/x/*"]),
        ]);
        assert_eq!(oracle(&refs, &scope, "", Some("/")), ["c/"]);
        for budget in [1, 2] {
            assert_eq!(
                walk_with_budget(&engine, "", Some("/"), 1, &scope, budget).await,
                ["c/"],
                "budget={budget}"
            );
        }
        // No visible key under either prefix: the name never shows.
        let (_dir, engine) = fs_engine(
            &big_hidden_folder("c/a", 1100, &[])
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )
        .await;
        assert!(walk_with_budget(&engine, "", Some("/"), 1, &scope, 1)
            .await
            .is_empty());
    }

    mod differential {
        use super::*;
        use proptest::prelude::*;

        const KEYS: [&str; 14] = [
            "a/x",
            "a/secret/k",
            "a/sector",
            "secret/k",
            "secret/j/z",
            "sector.txt",
            "sedan/x",
            "se/c/d",
            "pub/a",
            "pub/b/c",
            "ccc",
            "c/c/c",
            "abc/abc",
            "zz",
        ];
        const ALLOWS: [&str; 6] = ["b/*", "b/a/*", "b/pub/*", "b/se*", "b/c*", "b/abc/*"];
        const DENIES: [&str; 7] = [
            "b/secret/*",
            "b/a/secret/*",
            "b/sec*",
            "b/se/*",
            "b/c/*",
            "b/*/k",
            "b/abc/abc",
        ];
        const DELIMS: [&str; 7] = ["/", "c", "e", "r", "a", "ec", "t/"];
        const PREFIXES: [&str; 9] = ["", "s", "se", "sec", "secr", "a/", "a/s", "c", "ab"];

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(300))]
            /// A filtered LIST equals the fold of the visible keys for any
            /// delimiter, request prefix and Allow/Deny mix: no folded name
            /// is shown unless a visible key lies under it.
            #[test]
            fn a_filtered_list_equals_the_fold_of_the_visible_keys(
                allows in proptest::collection::vec(proptest::sample::select(&ALLOWS[..]), 1..4),
                denies in proptest::collection::vec(proptest::sample::select(&DENIES[..]), 0..3),
                delimiter in proptest::sample::select(&DELIMS[..]),
                prefix in proptest::sample::select(&PREFIXES[..]),
                max_keys in prop_oneof![Just(2u32), Just(1000u32)],
                // A small budget runs out inside most listings of these 14
                // keys; 16 never.
                budget in prop_oneof![1usize..=8, Just(TEST_BUDGET)],
            ) {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(async {
                    let (_dir, engine) = fs_engine(&KEYS).await;
                    let mut perms: Vec<_> = allows.iter().map(|a| rule("Allow", &[a])).collect();
                    perms.extend(denies.iter().map(|d| rule("Deny", &[d])));
                    let scope = scoped(perms);
                    let got = walk_with_budget(&engine, prefix, Some(delimiter), max_keys, &scope, budget).await;
                    let want = oracle(&KEYS, &scope, prefix, Some(delimiter));
                    prop_assert_eq!(
                        got, want,
                        "allows={:?} denies={:?} delimiter={:?} prefix={:?} budget={}",
                        allows, denies, delimiter, prefix, budget
                    );
                    Ok(())
                })?;
            }
        }
    }
}
