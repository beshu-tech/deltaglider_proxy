// SPDX-License-Identifier: BUSL-1.1

//! Canonical pagination state machine for the job subsystems
//! (replication, lifecycle, maintenance).
//!
//! Every worker loop used to hand-roll the same four decisions — and had
//! drifted on them (maintenance lacked the poison-token guard and the
//! `is_truncated` check):
//!
//! 1. **Token threading** — which continuation token the next
//!    `list_objects` call gets, and when the loop is exhausted.
//! 2. **Resume detection** — did this loop start from a PERSISTED cursor?
//! 3. **The poison-token guard** — when the FIRST page of a RESUMED loop
//!    fails to list, the persisted token is the prime suspect (backends
//!    invalidate tokens); the caller must clear its persisted copy
//!    instead of wedging every retry on the same bad cursor.
//! 4. **The page cap** — a hard bound against token loops.
//!
//! [`Pager`] owns exactly those four decisions and NOTHING else. Side
//! effects (what to persist, when to heartbeat, how to record failures,
//! when an error is fatal) stay in the callers — they differ between
//! subsystems for real reasons (replication fuses its token persist with
//! the event flush under one DB lock; lifecycle persists only in execute
//! mode; maintenance persists per phase).
//!
//! ## Normalization note (deliberate behavior unification)
//!
//! [`Pager::advance`] treats `(is_truncated=false, next_token=Some)` as
//! COMPLETE and normalizes the visible token to `None`. The engine never
//! emits that pairing, and the old loops disagreed on it (replication
//! persisted the token then broke; maintenance would have kept looping).
//! The unified rule is the anti-loop one.

/// One bound for every job pagination loop (was four private copies).
pub const MAX_JOB_PAGES: u32 = 10_000;

/// Attempts of one page listing before the loop gives up on a transient
/// error (the parity scan's budget).
pub const LIST_ATTEMPTS: u32 = 5;

/// What a failed page listing means for the loop that sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListFailure {
    /// The backend was slow, broke off, answered a 5xx or throttled: list
    /// the SAME token again after a pause.
    Transient,
    /// The request itself was refused as invalid (InvalidArgument, or a
    /// not-found class): on the first page of a resumed loop the persisted
    /// token is the suspect, and only this restarts the loop.
    Token,
    /// Anything else (the bucket is gone, access is refused): the loop
    /// stops and keeps its cursor.
    Fatal,
}

impl ListFailure {
    /// Classify an engine listing error. Engine continuation tokens are
    /// keys (a start-after on S3), so a backend cannot invalidate one: a
    /// timeout on the first resumed page says nothing about the token.
    pub fn of(e: &crate::deltaglider::EngineError) -> Self {
        use crate::deltaglider::EngineError as E;
        use crate::storage::StorageError as S;
        if crate::transfer::CopyClass::of_engine(e).retryable() {
            return ListFailure::Transient;
        }
        match e {
            E::InvalidArgument(_)
            | E::NotFound(_)
            | E::Storage(S::NotFound(_))
            | E::Storage(S::InvalidKey(_)) => ListFailure::Token,
            _ => ListFailure::Fatal,
        }
    }
}

/// The pause before listing attempt `attempt + 1` of the same page, or
/// `None` once [`LIST_ATTEMPTS`] are used up or `failure` is not
/// transient.
pub fn list_retry_delay(failure: ListFailure, attempt: u32) -> Option<std::time::Duration> {
    (failure == ListFailure::Transient && attempt < LIST_ATTEMPTS)
        .then(|| std::time::Duration::from_millis(250 * u64::from(attempt)))
}

#[derive(Debug)]
pub struct Pager {
    token: Option<String>,
    resumed: bool,
    pages_started: u32,
    max_pages: u32,
    /// True while the last `advance` said another page follows. Lets
    /// callers distinguish "loop ended because the listing completed"
    /// from "loop ended because the page budget ran out mid-listing".
    more_pending: bool,
}

impl Pager {
    /// A loop that resumes from a persisted cursor (`None` = fresh start).
    pub fn resuming(resume_token: Option<String>) -> Self {
        Self {
            resumed: resume_token.is_some(),
            token: resume_token,
            pages_started: 0,
            max_pages: MAX_JOB_PAGES,
            more_pending: false,
        }
    }

    /// A loop that never resumes (delete-pass, preview, cleanup sweeps).
    pub fn fresh() -> Self {
        Self::resuming(None)
    }

    /// Page-budget override — unit tests and the `DGP_TEST_MAX_JOB_PAGES`
    /// integration seam (replication truncation regression).
    pub fn with_max_pages(mut self, max_pages: u32) -> Self {
        self.max_pages = max_pages;
        self
    }

    /// The env-seam variant: applies `DGP_TEST_MAX_JOB_PAGES` when set.
    pub fn with_test_max_pages_env(self) -> Self {
        match crate::config::test_seams::test_seams().max_job_pages {
            Some(n) => self.with_max_pages(n),
            None => self,
        }
    }

    /// Begin the next page. Returns the 0-based page index, or `None`
    /// when the page budget is exhausted.
    #[must_use]
    pub fn begin_page(&mut self) -> Option<u32> {
        if self.pages_started >= self.max_pages {
            return None;
        }
        let idx = self.pages_started;
        self.pages_started += 1;
        Some(idx)
    }

    /// The continuation token for the page begun by [`begin_page`].
    ///
    /// [`begin_page`]: Self::begin_page
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// True exactly when the page that just failed to list was the FIRST
    /// page of a RESUMED loop — the persisted token is the prime suspect.
    /// The caller must clear its persisted cursor (and may
    /// [`restart_fresh`] to retry from page 0).
    ///
    /// [`restart_fresh`]: Self::restart_fresh
    pub fn poisoned_resume_token(&self) -> bool {
        self.resumed && self.pages_started == 1
    }

    /// Restart the loop from page 0 for a listing that failed with
    /// `failure`? Only a refused token on the first page of a resumed
    /// loop: a transient error is retried with the same token, and any
    /// other error stops the loop with its cursor kept.
    pub fn restarts_on(&self, failure: ListFailure) -> bool {
        self.poisoned_resume_token() && failure == ListFailure::Token
    }

    /// Drop the (poisoned) token and rewind to a fresh first page. After
    /// this `resumed` is false, so the guard can never fire twice.
    pub fn restart_fresh(&mut self) {
        self.token = None;
        self.resumed = false;
        self.pages_started = 0;
        self.more_pending = false;
    }

    /// True when the loop stopped because the page budget ran out while
    /// the listing still had more pages. Phase-machine callers (migrate,
    /// reencrypt) MUST treat this as fatal: falling through to the next
    /// phase here means "verified"/"flipped"/"deleted" over a listing
    /// that was silently truncated. Cursor-driven callers (replication,
    /// lifecycle) MUST keep their persisted token when this is true —
    /// clearing it "on clean finish" would orphan the tail forever.
    pub fn truncated_by_page_budget(&self) -> bool {
        self.pages_started >= self.max_pages && self.more_pending
    }

    /// Thread one page result. Returns true when another page follows
    /// (`is_truncated && next_token.is_some()`). When it returns false
    /// the visible [`token`] is normalized to `None`, so callers that
    /// persist `token()` at page end clear their cursor on a complete
    /// pass for free.
    ///
    /// [`token`]: Self::token
    #[must_use]
    pub fn advance(&mut self, is_truncated: bool, next_token: Option<String>) -> bool {
        if is_truncated && next_token.is_some() {
            self.token = next_token;
            self.more_pending = true;
            true
        } else {
            self.token = None;
            self.more_pending = false;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_threading_truth_table() {
        // (is_truncated, next_token) → (continue?, visible token)
        for (truncated, next, more, visible) in [
            (true, Some("t2".to_string()), true, Some("t2")),
            (true, None, false, None),
            // The pathological pairing: normalized to complete (anti-loop).
            (false, Some("t2".to_string()), false, None),
            (false, None, false, None),
        ] {
            let mut p = Pager::fresh();
            assert_eq!(p.begin_page(), Some(0));
            assert_eq!(
                p.advance(truncated, next.clone()),
                more,
                "truncated={truncated} next={next:?}"
            );
            assert_eq!(p.token(), visible, "truncated={truncated} next={next:?}");
        }
    }

    #[test]
    fn poison_guard_fires_only_on_first_resumed_page() {
        // Fresh loop: never fires.
        let mut fresh = Pager::fresh();
        let _ = fresh.begin_page();
        assert!(!fresh.poisoned_resume_token());

        // Resumed loop: fires on page 0 only.
        let mut p = Pager::resuming(Some("persisted".into()));
        let _ = p.begin_page();
        assert!(p.poisoned_resume_token(), "first page of a resumed loop");
        assert!(p.advance(true, Some("t2".into())));
        let _ = p.begin_page();
        assert!(
            !p.poisoned_resume_token(),
            "page 1+ failures are not the token's fault"
        );
    }

    #[test]
    fn poison_guard_is_one_shot_after_restart() {
        let mut p = Pager::resuming(Some("bad".into()));
        let _ = p.begin_page();
        assert!(p.poisoned_resume_token());
        p.restart_fresh();
        assert_eq!(p.token(), None);
        assert_eq!(p.begin_page(), Some(0), "rewound to a fresh first page");
        assert!(!p.poisoned_resume_token(), "guard can never fire twice");
    }

    #[test]
    fn max_pages_cap() {
        let mut p = Pager::fresh().with_max_pages(3);
        for i in 0..3u32 {
            assert_eq!(p.begin_page(), Some(i));
            assert!(p.advance(true, Some(format!("t{i}"))));
        }
        assert_eq!(p.begin_page(), None, "budget exhausted");
        assert_eq!(p.begin_page(), None, "stays exhausted");
        assert!(
            p.truncated_by_page_budget(),
            "budget ran out mid-listing — phase machines must fail, not fall through"
        );
    }

    #[test]
    fn complete_pass_is_not_budget_truncation() {
        // Ending exactly ON the budget with a complete listing is fine.
        let mut p = Pager::fresh().with_max_pages(2);
        let _ = p.begin_page();
        assert!(p.advance(true, Some("t1".into())));
        let _ = p.begin_page();
        assert!(!p.advance(false, None), "listing complete on the last page");
        assert!(!p.truncated_by_page_budget());

        // And a restart clears a previous truncation verdict.
        let mut q = Pager::fresh().with_max_pages(1);
        let _ = q.begin_page();
        assert!(q.advance(true, Some("t1".into())));
        assert!(q.truncated_by_page_budget());
        q.restart_fresh();
        assert!(!q.truncated_by_page_budget());
    }

    #[test]
    fn fresh_pager_starts_with_no_token_and_no_resume() {
        // The preview-safety invariant: a fresh loop can never trigger the
        // guard (and therefore never causes a persisted-cursor write).
        let mut p = Pager::fresh();
        assert_eq!(p.token(), None);
        let _ = p.begin_page();
        assert!(!p.poisoned_resume_token());
    }

    #[test]
    fn begin_token_advance_sequence() {
        // Three-page walkthrough: the token visible on each page equals the
        // previous page's next_continuation_token.
        let mut p = Pager::resuming(Some("p0".into()));
        assert_eq!(p.begin_page(), Some(0));
        assert_eq!(p.token(), Some("p0"));
        assert!(p.advance(true, Some("p1".into())));
        assert_eq!(p.begin_page(), Some(1));
        assert_eq!(p.token(), Some("p1"));
        assert!(p.advance(true, Some("p2".into())));
        assert_eq!(p.begin_page(), Some(2));
        assert_eq!(p.token(), Some("p2"));
        assert!(!p.advance(true, None));
        assert_eq!(p.token(), None, "complete pass clears the cursor");
    }
}
