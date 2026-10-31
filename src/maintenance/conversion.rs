// SPDX-License-Identifier: BUSL-1.1

//! The re-encrypt job's duties toward the conversion marker of its bucket
//! (`docs/plan/encryption-conversion-marker.md`, P4). The marker exists for
//! CLIENTS: it lets them read the bucket's plaintext history while the job
//! converts it. The job itself reads every unsealed object through its
//! `ConversionReadProof` (`worker.rs`), with or without a marker.
//!
//! - At each (re)start, a marker of this bucket gets a new deadline by
//!   identity; its cutoff and run id stay (also a marker whose deadline
//!   passed while no job ran: the resume reopens it).
//! - Each converted object that a client could not read (stored at or
//!   after the cutoff plus the grace, or with no stored time) is
//!   `adopted_after_cutoff`: counted, listed in the job's failure ring as
//!   information, audited.
//! - When the run converted everything (no `rewrite_failed`), the job
//!   closes the marker by identity. A marker that changed under it
//!   (another run, a body that does not open) is `close_blocked`: audited,
//!   and the run settles with an error, never as a silent success.
//!
//! Opening a conversion is the apply flow's (P5); [`open_window`] is the
//! one helper that the admin endpoint and the apply flow share.

use crate::deltaglider::DynEngine;
use crate::storage::encrypting::conversion::{
    self as marker, ByIdentity, ConversionOp, ConversionReply, MarkerState, RunId, Window,
};
use crate::storage::encrypting::seal;
use crate::storage::StorageBackend;
use crate::types::FileMetadata;
use std::sync::atomic::{AtomicI64, Ordering};

/// The prefix of a failure-ring row of an object that the job could not
/// convert (the window stays open; a later run retries it).
pub const REWRITE_FAILED: &str = "rewrite_failed";

/// The prefix of a failure-ring row of an object that the job converted
/// although clients could not read it. Information, not a failure.
pub const ADOPTED_AFTER_CUTOFF: &str = "adopted_after_cutoff";

/// The failure-ring key of the marker itself (a blocked close).
pub const MARKER_KEY: &str = ".dg/facts/!!conversion";

/// The marker that one run of the job works with.
#[derive(Debug)]
pub(crate) enum JobMarker {
    /// The bucket writes plaintext, or holds no marker.
    None,
    /// A marker that this proxy cannot open: never written, reported at
    /// the end.
    Foreign,
    /// This bucket's marker.
    Ours {
        run: RunId,
        /// The window that classifies adoptions (by the cutoff only; the
        /// deadline does not matter to the job).
        window: Window,
    },
}

/// What the job ends with, for the run summary.
#[derive(Debug, Default)]
pub(crate) struct Adoptions {
    count: AtomicI64,
}

impl Adoptions {
    pub(crate) fn add(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
        marker::ADOPTED_AFTER_CUTOFF.inc();
    }
    pub(crate) fn count(&self) -> i64 {
        self.count.load(Ordering::Relaxed)
    }
}

fn audit(action: &str, target: &str, bucket: &str) {
    crate::audit::audit_log(
        action,
        "system",
        target,
        &axum::http::HeaderMap::new(),
        bucket,
        "",
    );
}

/// The marker of `bucket` at the start of a run. A marker of this bucket
/// gets the deadline `server now + window length` by identity (a resume
/// keeps the cutoff); the audit names the run and both times. `Err`: the
/// marker could not be read or written (the job stops and retries later,
/// so it never runs without knowing the marker).
pub(crate) async fn begin(engine: &DynEngine, bucket: &str) -> Result<JobMarker, String> {
    let storage = engine.storage();
    let state = match storage.conversion(bucket, ConversionOp::Status).await {
        Ok(ConversionReply::Status(s)) => s,
        Ok(_) => return Ok(JobMarker::None),
        Err(e) => return Err(format!("could not read the conversion marker: {e}")),
    };
    let (fields, server_now) = match state {
        MarkerState::Absent => return Ok(JobMarker::None),
        MarkerState::Foreign => {
            tracing::warn!(
                "maintenance: bucket '{bucket}' holds a conversion marker that this proxy cannot \
                 open; the job converts the bucket but cannot close it"
            );
            audit(
                "encryption_marker_ignored",
                "the re-encrypt job found a conversion marker that does not open here",
                bucket,
            );
            return Ok(JobMarker::Foreign);
        }
        MarkerState::Ours { fields, server_now } => (fields, server_now),
    };
    let deadline = extended_deadline(fields.deadline, server_now, marker::default_window_secs());
    let changed = storage
        .conversion(
            bucket,
            ConversionOp::Extend {
                run: fields.run,
                deadline,
            },
        )
        .await
        .map_err(|e| format!("could not extend the conversion marker: {e}"))?;
    let run = marker::run_label(&fields.run);
    match changed {
        ConversionReply::Changed(ByIdentity::Done(Some(f), _)) => {
            audit(
                "encryption_conversion_extended",
                &format!(
                    "run {run}: cutoff {} kept, deadline {} -> {}",
                    f.cutoff, fields.deadline, f.deadline
                ),
                bucket,
            );
            Ok(JobMarker::Ours {
                run: f.run,
                window: window_of(&f),
            })
        }
        ConversionReply::Changed(ByIdentity::Absent) => Ok(JobMarker::None),
        other => {
            // Another run's marker, or one that changed under every try: the
            // job never writes it, and reports it at the end.
            tracing::warn!(
                "maintenance: the conversion marker of bucket '{bucket}' (run {run}) was not \
                 extended: {other:?}"
            );
            Ok(JobMarker::Foreign)
        }
    }
}

/// THE deadline of a resume (pure): `server_now + window_secs`, never
/// earlier than the marker's deadline now.
pub(crate) fn extended_deadline(current: i64, server_now: i64, window_secs: i64) -> i64 {
    current.max(server_now.saturating_add(window_secs))
}

fn window_of(f: &marker::Fields) -> Window {
    Window::Open {
        cutoff: f.cutoff,
        deadline: f.deadline,
        run: f.run,
        version: String::new(),
    }
}

/// Was the object whose job read gave `meta` adopted after the cutoff?
/// Only an object without the encryption marker (plaintext) can be; the
/// verdict is THE read rule's, for the job reader with this run's window.
pub(crate) fn adopted(marker: &JobMarker, meta: &FileMetadata) -> bool {
    let JobMarker::Ours { window, .. } = marker else {
        return false;
    };
    if meta
        .user_metadata
        .contains_key(crate::storage::encrypting::ENCRYPTION_MARKER_KEY)
    {
        return false;
    }
    seal::read_verdict(
        meta,
        false,
        seal::PlaintextRule::Encrypts(window),
        seal::Reader::Job,
        marker::grace_secs(),
    ) == seal::Unsealed::Adopted
}

/// How the end of a run went for the marker.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Closed {
    /// Nothing to close, or closed now.
    Done,
    /// The run had failures: the window stays open for the next run.
    KeptOpen,
    /// The marker is not this run's (or did not open): not closed. The
    /// run must not report success.
    Blocked(String),
}

/// Close the marker at the end of a run with `failed` objects.
pub(crate) async fn finish(
    engine: &DynEngine,
    bucket: &str,
    marker: &JobMarker,
    failed: i64,
    adopted: i64,
) -> Closed {
    let run = match marker {
        JobMarker::None => return Closed::Done,
        JobMarker::Foreign => {
            let why = "the conversion marker does not open here or is another run's, so the \
                       job did not close it"
                .to_string();
            audit("encryption_conversion_close_blocked", &why, bucket);
            return Closed::Blocked(why);
        }
        JobMarker::Ours { run, .. } => *run,
    };
    let label = marker::run_label(&run);
    if failed > 0 {
        tracing::info!(
            "maintenance: the conversion of bucket '{bucket}' (run {label}) stays open: {failed} \
             object(s) were not converted"
        );
        return Closed::KeptOpen;
    }
    match engine
        .storage()
        .conversion(bucket, ConversionOp::Close { run })
        .await
    {
        Ok(ConversionReply::Changed(ByIdentity::Done(None, _)))
        | Ok(ConversionReply::Changed(ByIdentity::Absent)) => {
            audit(
                "encryption_conversion_closed",
                &format!("run {label}: {adopted} object(s) adopted after the cutoff"),
                bucket,
            );
            Closed::Done
        }
        Ok(ConversionReply::NotEncrypted) => Closed::Done,
        Ok(other) => {
            let why = format!(
                "the conversion marker (run {label}) changed while the job ran ({other:?}), so \
                 the job did not close it"
            );
            audit("encryption_conversion_close_blocked", &why, bucket);
            Closed::Blocked(why)
        }
        Err(e) => {
            let why = format!("the conversion marker (run {label}) could not be closed: {e}");
            audit("encryption_conversion_close_blocked", &why, bucket);
            Closed::Blocked(why)
        }
    }
}

/// Open the conversion window of `bucket` (or reuse the valid one) for
/// [`marker::default_window_secs`]: the helper of the admin
/// `POST /jobs/reencrypt {open_window: true}` and of the apply flow (P5).
/// Audits `encryption_conversion_opened`.
pub async fn open_window(engine: &DynEngine, bucket: &str, by: &str) -> Result<RunId, String> {
    let window_secs = marker::default_window_secs();
    match engine
        .storage()
        .conversion(bucket, ConversionOp::Open { window_secs })
        .await
    {
        Ok(ConversionReply::Opened(run)) => {
            crate::audit::audit_log(
                "encryption_conversion_opened",
                by,
                &format!("run {}", marker::run_label(&run)),
                &axum::http::HeaderMap::new(),
                bucket,
                "",
            );
            Ok(run)
        }
        Ok(ConversionReply::NotEncrypted) => Err(format!(
            "bucket '{bucket}' does not encrypt: it has no conversion to open"
        )),
        Ok(other) => Err(format!("unexpected reply to an open: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Does `bucket` hold a conversion marker (ours or not)? A bucket that
/// writes plaintext has none that matters.
pub async fn marker_exists(engine: &DynEngine, bucket: &str) -> Result<bool, String> {
    match engine
        .storage()
        .conversion(bucket, ConversionOp::Status)
        .await
    {
        Ok(ConversionReply::Status(MarkerState::Absent)) | Ok(ConversionReply::NotEncrypted) => {
            Ok(false)
        }
        Ok(_) => Ok(true),
        Err(e) => Err(format!("could not read the conversion marker: {e}")),
    }
}

/// The refusal of a metadata backfill on a bucket in conversion.
pub fn backfill_refusal(bucket: &str) -> String {
    format!(
        "bucket '{bucket}' is being converted to encryption (a conversion marker exists): a \
         metadata backfill would move the stored time of its plaintext objects past the \
         conversion's cutoff, and clients could no longer read them. Let the re-encrypt job \
         finish first"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resume_never_shortens_the_deadline() {
        assert_eq!(extended_deadline(100, 50, 30), 100);
        assert_eq!(extended_deadline(100, 90, 30), 120);
        assert_eq!(extended_deadline(10, 1000, 30), 1030, "an expired marker");
        assert_eq!(extended_deadline(0, i64::MAX, 30), i64::MAX);
    }

    fn plain(lm: Option<i64>) -> FileMetadata {
        FileMetadata::new_passthrough("a.txt".into(), "x".into(), "e".into(), 3, None)
            .stored_at_for_tests(lm)
    }

    #[test]
    fn adoption_is_judged_by_the_cutoff_of_this_run() {
        let ours = JobMarker::Ours {
            run: [1; 16],
            window: Window::Open {
                cutoff: 1_000,
                deadline: 2_000,
                run: [1; 16],
                version: String::new(),
            },
        };
        let g = marker::grace_secs();
        assert!(!adopted(&ours, &plain(Some(999))));
        assert!(!adopted(&ours, &plain(Some(1_000 + g - 1))));
        assert!(adopted(&ours, &plain(Some(1_000 + g))));
        assert!(adopted(&ours, &plain(None)), "no stored time");
        // No marker, or one that is not ours: nothing to classify by.
        assert!(!adopted(&JobMarker::None, &plain(None)));
        assert!(!adopted(&JobMarker::Foreign, &plain(None)));
        // An encrypted object is never adopted.
        let mut sealed = plain(None);
        sealed.user_metadata.insert(
            crate::storage::encrypting::ENCRYPTION_MARKER_KEY.into(),
            "v2".into(),
        );
        assert!(!adopted(&ours, &sealed));
    }
}
