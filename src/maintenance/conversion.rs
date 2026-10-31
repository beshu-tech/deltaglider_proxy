// SPDX-License-Identifier: BUSL-1.1

//! The re-encrypt job's duties toward the conversion marker of its bucket
//! (`docs/plan/encryption-conversion-marker.md`, P4). The marker exists for
//! CLIENTS: it lets them read the bucket's plaintext history while the job
//! converts it. The job itself reads every unsealed object through its
//! `ConversionReadProof` (`worker.rs`), with or without a marker.
//!
//! - A job OWNS a marker only by run: the apply, the boot and
//!   `open_window` store the marker's run id in the job's params. At each
//!   (re)start, the marker of that run gets a new deadline by identity;
//!   its cutoff and run id stay. A job without a run (a key rotation, a
//!   plain re-encrypt) never extends or closes a marker.
//! - In a bucket that encrypts, each converted object (or reference) that
//!   a client could not read is `adopted_after_cutoff`: no marker of this
//!   job's run, or stored at or after the cutoff plus the grace, or with
//!   no stored time. It is counted in the job row, sampled in the failure
//!   ring (never evicting a rewrite failure), audited. Only a bucket that
//!   serves plaintext adopts silently.
//! - When the run converted everything (no `rewrite_failed`), the job
//!   closes the marker of its run by identity. A marker that changed under
//!   it (another run, a body that does not open) is `close_blocked`:
//!   audited, and the run settles with an error, never as a silent success.
//!
//! [`open_window`] and [`queue_reencrypt`] are the helpers that the admin
//! endpoint, the apply flow and the boot share.

use crate::deltaglider::DynEngine;
use crate::storage::encrypting::conversion::{
    self as marker, ByIdentity, ConversionOp, ConversionReply, MarkerState, OpenMode, OpenOutcome,
    RunId, Window,
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

/// The job's optional last phase before the close: delete the noncurrent
/// plaintext versions of a versioned backend bucket
/// ([`StorageBackend::purge_plaintext_versions`]).
pub const PURGE_PHASE: &str = "purge-versions";

/// The re-encrypt job's parameters, stored as JSON in its row (absent
/// params: the defaults).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReencryptParams {
    /// After a run that converted every object: delete the noncurrent
    /// plaintext versions of the converted keys. The deleted versions
    /// cannot be restored.
    #[serde(default)]
    pub purge_plaintext_versions: bool,
    /// The run of the conversion marker that the job's creator opened (an
    /// apply, a boot, or `open_window`). Only a job with the marker's run
    /// extends its deadline and closes it; a job without one (a key
    /// rotation, a plain re-encrypt) never writes a marker.
    #[serde(default, with = "run_hex", skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
}

/// A run id in the params JSON: hex text.
mod run_hex {
    use super::RunId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(run: &Option<RunId>, s: S) -> Result<S::Ok, S::Error> {
        match run {
            Some(r) => s.serialize_str(&hex::encode(r)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RunId>, D::Error> {
        let text = Option::<String>::deserialize(d)?;
        text.map(|t| {
            let mut run = [0u8; 16];
            hex::decode_to_slice(&t, &mut run)
                .map(|()| run)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
    }
}

impl ReencryptParams {
    /// The params JSON of a new job (`None`: the defaults).
    pub fn to_json(self) -> Option<String> {
        (self != Self::default()).then(|| serde_json::to_string(&self).expect("params serialize"))
    }

    /// The params of a job row. A row without params, or with params of
    /// another shape, gets the defaults: the purge never runs by mistake.
    pub(crate) fn of(params: Option<&str>) -> Self {
        params
            .and_then(|p| serde_json::from_str(p).ok())
            .unwrap_or_default()
    }
}

/// Where the purge phase is, saved as the job's cursor: the backend's
/// page cursor and the versions deleted so far (the run summary counts
/// them across a resume).
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PurgeCursor {
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub deleted: u64,
}

impl PurgeCursor {
    /// The cursor of a job persisted in the purge phase (a cursor that
    /// does not parse starts the phase again: deleting is idempotent).
    pub(crate) fn parse(token: Option<&str>) -> Self {
        token
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default()
    }
    pub(crate) fn token(&self) -> String {
        serde_json::to_string(self).expect("cursor serializes")
    }
}

/// The marker that one run of the job works with.
#[derive(Debug)]
pub(crate) enum JobMarker {
    /// The job has no run (a key rotation, a plain re-encrypt): it never
    /// reads, extends or closes a marker.
    Unowned,
    /// The bucket writes plaintext, or holds no marker: nothing to close.
    None,
    /// A marker that this proxy cannot open, or another run's: never
    /// written, reported at the end.
    Foreign,
    /// The marker of this job's run.
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

/// THE audit entry of the conversion (job, apply, boot, admin), by
/// `user`, about `bucket` and its object `key` (empty: the bucket), with
/// the request `headers` when a request asked for it.
pub(crate) fn audit_with(
    action: &str,
    user: &str,
    target: &str,
    bucket: &str,
    key: &str,
    headers: Option<&axum::http::HeaderMap>,
) {
    let none = axum::http::HeaderMap::new();
    crate::audit::audit_log(action, user, target, headers.unwrap_or(&none), bucket, key);
}

/// [`audit_with`] about the bucket, without a request.
pub(crate) fn audit(action: &str, user: &str, target: &str, bucket: &str) {
    audit_with(action, user, target, bucket, "", None);
}

/// The marker of `bucket` at the start of a run of a job with `run`. Only
/// the marker of that run gets the deadline `server now + window length`
/// by identity (a resume keeps the cutoff); the audit names the run and
/// both times. A job without a run reads no marker. `Err`: the marker
/// could not be read or written (the job stops and retries later, so it
/// never runs without knowing the marker).
pub(crate) async fn begin(
    engine: &DynEngine,
    bucket: &str,
    run: Option<RunId>,
) -> Result<JobMarker, String> {
    let Some(run) = run else {
        return Ok(JobMarker::Unowned);
    };
    let storage = engine.storage();
    let state = match storage.conversion(bucket, ConversionOp::Status).await {
        Ok(ConversionReply::Status(s)) => s,
        Ok(_) => return Ok(JobMarker::None),
        Err(e) => return Err(format!("could not read the conversion marker: {e}")),
    };
    let label = marker::run_label(&run);
    let (fields, server_now) = match state {
        MarkerState::Absent => return Ok(JobMarker::None),
        MarkerState::Ours { fields, server_now } if fields.run == run => (fields, server_now),
        MarkerState::Ours { fields, .. } => {
            tracing::warn!(
                "maintenance: bucket '{bucket}' holds the conversion marker of run {}, not of \
                 this job's run {label}; the job converts the bucket but does not close it",
                marker::run_label(&fields.run)
            );
            return Ok(JobMarker::Foreign);
        }
        MarkerState::Foreign => {
            tracing::warn!(
                "maintenance: bucket '{bucket}' holds a conversion marker that this proxy cannot \
                 open; the job converts the bucket but cannot close it"
            );
            audit(
                "encryption_marker_ignored",
                "system",
                "the re-encrypt job found a conversion marker that does not open here",
                bucket,
            );
            return Ok(JobMarker::Foreign);
        }
    };
    let deadline = extended_deadline(fields.deadline, server_now, marker::default_window_secs());
    let changed = storage
        .conversion(bucket, ConversionOp::Extend { run, deadline })
        .await
        .map_err(|e| format!("could not extend the conversion marker: {e}"))?;
    match changed {
        ConversionReply::Changed(ByIdentity::Done(Some(f), _)) => {
            audit(
                "encryption_conversion_extended",
                "system",
                &format!(
                    "run {label}: cutoff {} kept, deadline {} -> {}",
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
                "maintenance: the conversion marker of bucket '{bucket}' (run {label}) was not \
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

/// Was the unsealed object (or reference) whose job read gave `meta`
/// adopted: converted although a client could not read it? Never in a
/// bucket whose rule serves plaintext (`serves`). Otherwise THE read rule
/// decides, for the job reader, with the window of this job's run: no
/// marker of this run (none, a foreign one, another run's, a job without
/// a run) is a closed window, so every unsealed object that is not a
/// folder marker is adopted and reported.
pub(crate) fn adopted(marker: &JobMarker, serves: bool, key: &str, meta: &FileMetadata) -> bool {
    if serves
        || meta
            .user_metadata
            .contains_key(crate::storage::encrypting::ENCRYPTION_MARKER_KEY)
    {
        return false;
    }
    let closed = Window::Closed;
    let window = match marker {
        JobMarker::Ours { window, .. } => window,
        _ => &closed,
    };
    seal::read_verdict(
        meta,
        key.ends_with('/'),
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
        JobMarker::Unowned | JobMarker::None => return Closed::Done,
        JobMarker::Foreign => {
            let why = "the conversion marker does not open here or is another run's, so the \
                       job did not close it"
                .to_string();
            audit(
                "encryption_conversion_close_blocked",
                "system",
                &why,
                bucket,
            );
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
                "system",
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
            audit(
                "encryption_conversion_close_blocked",
                "system",
                &why,
                bucket,
            );
            Closed::Blocked(why)
        }
        Err(e) => {
            let why = format!("the conversion marker (run {label}) could not be closed: {e}");
            audit(
                "encryption_conversion_close_blocked",
                "system",
                &why,
                bucket,
            );
            Closed::Blocked(why)
        }
    }
}

/// A conversion window that [`open_window`] opened or found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenedWindow {
    pub run: RunId,
    /// This call wrote the marker: a caller whose next step fails removes
    /// it ([`close_opened`]).
    pub created: bool,
}

/// Open the conversion window of `bucket` for
/// [`marker::default_window_secs`] in `mode` ([`OpenMode::Resume`]: keep a
/// marker of this bucket as it is, the Resume of
/// `POST /jobs/reencrypt {open_window: true}`; [`OpenMode::Refresh`]: a
/// conversion that starts, at boot). Audits `encryption_conversion_opened`
/// for a marker that this call wrote or refreshed, by `by`.
pub async fn open_window(
    engine: &DynEngine,
    bucket: &str,
    mode: OpenMode,
    by: &str,
) -> Result<OpenedWindow, String> {
    let window_secs = marker::default_window_secs();
    let outcome = match engine
        .storage()
        .conversion(bucket, ConversionOp::Open { window_secs, mode })
        .await
    {
        Ok(ConversionReply::Opened(outcome)) => outcome,
        Ok(other) => return Err(format!("unexpected reply to an open: {other:?}")),
        Err(e) => return Err(e.to_string()),
    };
    let Some((fields, _)) = outcome.marker() else {
        return Err(format!(
            "bucket '{bucket}' holds a conversion marker ({MARKER_KEY}) that this proxy cannot \
             open (another key, another format, or an edited marker). The proxy never \
             overwrites it. Remove the marker on the backend if no other proxy converts the \
             bucket, then try again"
        ));
    };
    let created = matches!(outcome, OpenOutcome::Created(..));
    if created || matches!(outcome, OpenOutcome::Refreshed(..)) {
        audit(
            "encryption_conversion_opened",
            by,
            &format!("run {}", marker::run_label(&fields.run)),
            bucket,
        );
    }
    Ok(OpenedWindow {
        run: fields.run,
        created,
    })
}

/// Remove the marker of `run` that a caller opened, when the job it was
/// for could not be queued. A failure is logged: the marker then closes
/// at its deadline.
pub async fn close_opened(engine: &DynEngine, bucket: &str, run: RunId, why: &str) {
    let closed = engine
        .storage()
        .conversion(bucket, ConversionOp::CloseAsBuilt { run: Some(run) })
        .await;
    let label = marker::run_label(&run);
    match closed {
        Ok(ConversionReply::Changed(ByIdentity::Done(None, _)))
        | Ok(ConversionReply::Changed(ByIdentity::Absent)) => audit(
            "encryption_conversion_closed",
            "admin",
            &format!("run {label}: {why}"),
            bucket,
        ),
        other => tracing::error!(
            "the conversion marker of bucket '{bucket}' (run {label}) was not removed after \
             {why} ({other:?}); it closes at its deadline"
        ),
    }
}

/// Does `bucket` hold a conversion marker (ours or not)? Read on the
/// backend itself, whatever the published config says about the bucket:
/// an apply opens a marker before it publishes the engine that encrypts.
pub async fn marker_exists(engine: &DynEngine, bucket: &str) -> Result<bool, String> {
    engine
        .storage()
        .read_conversion_marker(bucket)
        .await
        .map(|read| read.marker.is_some())
        .map_err(|e| format!("could not read the conversion marker: {e}"))
}

/// Is the conversion window of `bucket` open now (a marker of this bucket
/// whose deadline is after the backend's clock)? A read failure counts
/// as closed.
pub async fn window_open(engine: &DynEngine, bucket: &str) -> bool {
    matches!(
        engine.storage().conversion(bucket, ConversionOp::Status).await,
        Ok(ConversionReply::Status(MarkerState::Ours { fields, server_now }))
            if server_now < fields.deadline
    )
}

/// What the boot did about a bucket that encrypts with a proxy key and
/// does not serve plaintext ([`start_at_boot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootConversion {
    /// Its conversion window is open: nothing to do.
    WindowOpen,
    /// The sample holds no object that a client cannot read.
    NoPlaintext,
    /// The boot opened the window and queued the job of its run.
    Started { run: RunId, job_id: i64 },
    /// `key` is refused to clients, and the boot did not start a
    /// conversion (`why`): the operator decides.
    Refused { key: String, why: String },
}

/// The boot's conversion of a bucket that a restart made encrypt
/// (operator, Helm, GitOps: no apply ran). When the sample of `sample`
/// objects holds one that a client cannot read and none that is encrypted,
/// the bucket has NO key claim (no proxy ever wrote encrypted data there,
/// so its plaintext is history, not a plant) and NO marker, and no job is
/// active on it: open
/// the window in [`OpenMode::Refresh`] (a marker that another node wrote
/// meanwhile and that opens here keeps its run, with a new cutoff) and
/// queue the re-encrypt job of its run, by `boot`. Idempotent across
/// restarts: the job's first write claims the bucket, and the marker
/// stays until the job ends.
pub async fn start_at_boot(
    app: &crate::api::handlers::AppState,
    bucket: &str,
    sample: u32,
) -> Result<BootConversion, String> {
    let engine = app.engine.load_full();
    if window_open(&engine, bucket).await {
        return Ok(BootConversion::WindowOpen);
    }
    let found = engine
        .plaintext_sample(bucket, sample)
        .await
        .map_err(|e| e.to_string())?;
    let Some(key) = found.first_refused else {
        return Ok(BootConversion::NoPlaintext);
    };
    let refused = |why: &str| {
        Ok(BootConversion::Refused {
            key: key.clone(),
            why: why.to_string(),
        })
    };
    if found.encrypted {
        // A key claim that a backend writer deleted must not turn plants
        // into history.
        return refused(
            "the bucket holds encrypted objects too, so its plaintext objects may not be \
             history",
        );
    }
    match engine.storage().read_key_claim(bucket).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return refused(
                "a proxy already wrote encrypted objects to the bucket (it holds a key claim), \
                 so its plaintext objects may not be history",
            )
        }
        Err(e) => return refused(&format!("the key claim could not be read: {e}")),
    }
    match marker_exists(&engine, bucket).await {
        Ok(false) => {}
        Ok(true) => {
            return refused(
                "the bucket holds a conversion marker that is closed here (past its deadline, \
                 or one that does not open)",
            )
        }
        Err(e) => return refused(&e),
    }
    let Some(db) = app.jobs_db.as_ref() else {
        return refused("this instance has no jobs database");
    };
    match db.lock().await.maintenance_active_job_for_bucket(bucket) {
        Ok(None) => {}
        Ok(Some(job)) => {
            return refused(&format!(
                "a {} job (maintenance:{}) is active on the bucket",
                job.kind, job.id
            ))
        }
        Err(e) => return refused(&format!("the active jobs could not be read: {e}")),
    }
    let opened = match open_window(&engine, bucket, OpenMode::Refresh, "boot").await {
        Ok(w) => w,
        Err(e) => return refused(&format!("the window could not be opened: {e}")),
    };
    let request = QueuedReencrypt {
        bucket: bucket.to_string(),
        params: ReencryptParams {
            purge_plaintext_versions: false,
            run: Some(opened.run),
        },
    };
    let none = axum::http::HeaderMap::new();
    let queued = queue_reencrypt(app, db, std::slice::from_ref(&request), "boot", &none).await;
    match queued.into_iter().next() {
        Some(Ok(job_id)) => Ok(BootConversion::Started {
            run: opened.run,
            job_id,
        }),
        other => {
            if opened.created {
                close_opened(
                    &engine,
                    bucket,
                    opened.run,
                    "the boot could not queue its job",
                )
                .await;
            }
            refused(&format!("the job could not be queued: {other:?}"))
        }
    }
}

/// One re-encrypt job to queue ([`queue_reencrypt`]).
#[derive(Debug, Clone)]
pub struct QueuedReencrypt {
    pub bucket: String,
    pub params: ReencryptParams,
}

/// Why a re-encrypt job was not queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotQueued {
    /// Another job (not a delete) is active on the bucket.
    Active,
    /// The jobs database refused the row.
    Failed(String),
}

/// THE creation of re-encrypt jobs (the apply, the boot, the admin
/// endpoint): one row per request in one lock of the jobs DB, the write
/// gate armed from creation (no create-to-claim window for writes), the
/// workers woken, and one `maintenance_reencrypt_requested` audit entry by
/// `by`. Returns one answer per request, in order.
pub async fn queue_reencrypt(
    app: &crate::api::handlers::AppState,
    db: &std::sync::Arc<tokio::sync::Mutex<crate::config_db::JobsDb>>,
    requests: &[QueuedReencrypt],
    by: &str,
    headers: &axum::http::HeaderMap,
) -> Vec<Result<i64, NotQueued>> {
    let now = super::store::current_unix_seconds();
    let results: Vec<Result<i64, NotQueued>> = {
        let db = db.lock().await;
        let results = requests
            .iter()
            .map(|r| {
                match db.maintenance_create_job(
                    "reencrypt",
                    &r.bucket,
                    "counting",
                    r.params.to_json().as_deref(),
                    by,
                    now,
                ) {
                    Ok(Some(id)) => Ok(id),
                    Ok(None) => Err(NotQueued::Active),
                    Err(e) => Err(NotQueued::Failed(e.to_string())),
                }
            })
            .collect();
        app.maintenance_gate.sync_from(&db);
        results
    };
    let queued: Vec<&QueuedReencrypt> = requests
        .iter()
        .zip(&results)
        .filter(|(_, r)| r.is_ok())
        .map(|(q, _)| q)
        .collect();
    if !queued.is_empty() {
        super::wake_workers(&app.maintenance_notify);
        let names: Vec<&str> = queued.iter().map(|q| q.bucket.as_str()).collect();
        tracing::info!("maintenance: re-encrypt requested for {names:?} by {by}");
        let mut what = names.join(",");
        if queued.iter().any(|q| q.params.purge_plaintext_versions) {
            what.push_str(" (purge plaintext versions)");
        }
        crate::audit::audit_log(
            "maintenance_reencrypt_requested",
            by,
            &what,
            headers,
            "",
            "",
        );
    }
    results
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
    fn the_purge_runs_only_when_a_job_asked_for_it() {
        assert_eq!(ReencryptParams::default().to_json(), None);
        let purge = ReencryptParams {
            purge_plaintext_versions: true,
            run: None,
        };
        let json = purge.to_json().unwrap();
        assert_eq!(ReencryptParams::of(Some(&json)), purge);
        assert_eq!(ReencryptParams::of(None), ReencryptParams::default());
        assert_eq!(
            ReencryptParams::of(Some("{\"refresh_last_modified\":true}")),
            ReencryptParams::default()
        );
        assert_eq!(
            ReencryptParams::of(Some("not json")),
            ReencryptParams::default()
        );
    }

    #[test]
    fn the_purge_cursor_keeps_its_count_across_a_resume() {
        let c = PurgeCursor {
            at: Some("{\"k\":\"a\",\"v\":\"1\"}".into()),
            deleted: 7,
        };
        assert_eq!(PurgeCursor::parse(Some(&c.token())), c);
        assert_eq!(PurgeCursor::parse(None), PurgeCursor::default());
        assert_eq!(PurgeCursor::parse(Some("x")), PurgeCursor::default());
    }

    /// Review C3: adoptions never evict a rewrite failure from the 200-row
    /// failure ring, and every adoption counts (beyond 200).
    #[test]
    fn adoptions_never_evict_a_rewrite_failure_from_the_ring() {
        let db = crate::config_db::JobsDb::in_memory("k").unwrap();
        let id = db
            .maintenance_create_job("reencrypt", "releases", "counting", None, "t", 1)
            .unwrap()
            .unwrap();
        db.maintenance_record_failure(id, "bad.bin", &format!("{REWRITE_FAILED}: x"), 200)
            .unwrap();
        for i in 0..250 {
            db.maintenance_record_adoption(
                id,
                &format!("late-{i}"),
                &format!("{ADOPTED_AFTER_CUTOFF}: y"),
            )
            .unwrap();
            // Failures recorded between the adoptions count only themselves.
            if i % 50 == 0 {
                db.maintenance_record_failure(
                    id,
                    "flaky.bin",
                    &format!("{REWRITE_FAILED}: z"),
                    200,
                )
                .unwrap();
            }
        }
        let failed = db
            .maintenance_count_failures_with_prefix(id, REWRITE_FAILED)
            .unwrap();
        let samples = db
            .maintenance_count_failures_with_prefix(id, ADOPTED_AFTER_CUTOFF)
            .unwrap();
        let row = db.maintenance_job_by_id(id).unwrap().unwrap();
        assert_eq!(
            (failed, row.objects_adopted, samples),
            (6, 250, crate::maintenance::store::ADOPTED_SAMPLES),
            "the failing objects stay visible, every adoption counts, the keys are a sample"
        );
        let failures = db.maintenance_list_failures(id, 1000).unwrap();
        assert!(failures.iter().any(|f| f.object_key == "bad.bin"));
    }

    #[test]
    fn a_resume_never_shortens_the_deadline() {
        assert_eq!(extended_deadline(100, 50, 30), 100);
        assert_eq!(extended_deadline(100, 90, 30), 120);
        assert_eq!(extended_deadline(10, 1000, 30), 1030, "an expired marker");
        assert_eq!(extended_deadline(0, i64::MAX, 30), i64::MAX);
    }

    #[test]
    fn the_run_of_a_job_round_trips_through_its_params() {
        let p = ReencryptParams {
            purge_plaintext_versions: false,
            run: Some([0xab; 16]),
        };
        let json = p.to_json().unwrap();
        assert!(json.contains(&"ab".repeat(16)), "{json}");
        assert_eq!(ReencryptParams::of(Some(&json)), p);
        // A run that does not parse: the defaults (no run, no purge).
        assert_eq!(
            ReencryptParams::of(Some("{\"run\":\"zz\",\"purge_plaintext_versions\":true}")),
            ReencryptParams::default()
        );
    }

    fn plain(lm: Option<i64>) -> FileMetadata {
        FileMetadata::new_passthrough("a.txt".into(), "x".into(), "e".into(), 3, None)
            .stored_at_for_tests(lm)
    }

    /// Review S1: in a bucket that encrypts, only the window of this job's
    /// run makes an unsealed object history; without it (no marker, a
    /// foreign or another run's marker, a job without a run) every
    /// unsealed object is adopted and reported. A bucket that serves
    /// plaintext adopts nothing.
    #[test]
    fn adoption_is_judged_by_the_window_of_this_run() {
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
        let a = |m: &JobMarker, meta: &FileMetadata| adopted(m, false, "a.txt", meta);
        assert!(!a(&ours, &plain(Some(999))));
        assert!(!a(&ours, &plain(Some(1_000 + g - 1))));
        assert!(a(&ours, &plain(Some(1_000 + g))));
        assert!(a(&ours, &plain(None)), "no stored time");
        for other in [JobMarker::None, JobMarker::Foreign, JobMarker::Unowned] {
            assert!(a(&other, &plain(Some(999))), "{other:?}");
            // A bucket whose rule serves plaintext: silent.
            assert!(!adopted(&other, true, "a.txt", &plain(None)), "{other:?}");
        }
        // A folder marker is never adopted.
        let folder = FileMetadata::new_passthrough("d/".into(), "x".into(), "e".into(), 0, None)
            .stored_at_for_tests(None);
        assert!(!adopted(&JobMarker::None, false, "d/", &folder));
        // An encrypted object is never adopted.
        let mut sealed = plain(None);
        sealed.user_metadata.insert(
            crate::storage::encrypting::ENCRYPTION_MARKER_KEY.into(),
            "v2".into(),
        );
        assert!(!a(&ours, &sealed));
        assert!(!a(&JobMarker::None, &sealed));
    }
}
