// SPDX-License-Identifier: BUSL-1.1

//! Maintenance job kind **`backfill-metadata`**: stamp the canonical DG
//! metadata onto passthrough objects that lack it — foreign or pre-proxy
//! writes the proxy serves via `FileMetadata::fallback()` (empty
//! `dg-file-sha256`).
//!
//! The bytes are read ONCE, only to compute the content hashes (the one
//! part of the canonical set that cannot be derived without them), then the
//! metadata is written IN PLACE: the S3 backend does a server-side
//! self-copy with `MetadataDirective: REPLACE`, the filesystem backend
//! rewrites the xattr. No object bytes are uploaded.
//!
//! Timestamps: an S3 self-copy resets the storage `LastModified`. By
//! default the job pins `dg-created-at` to the object's pre-job created
//! time, so the LastModified the PROXY serves is unchanged (and
//! replication's NewerWins does not re-copy the object — see
//! docs/plan/rca-replication-recopy-2026-06-30.md). With
//! `refresh_last_modified: true` the job stamps the rewrite time instead,
//! so the object deliberately reads as modified now.
//!
//! ETag stability: a self-copy collapses a multipart ETag (`…-N`) into a
//! simple one. When the object's served ETag looks multipart and no
//! `dg-multipart-etag` override exists yet, the old ETag is preserved in
//! `dg-multipart-etag`, which the read path serves in preference to the
//! raw ETag — clients keep the ETag they know.

use chrono::{DateTime, Utc};
use md5::Md5;
use sha2::{Digest, Sha256};

use crate::types::{FileMetadata, StorageInfo, DELTAGLIDER_TOOL};

/// Job-kind string as stored in `maintenance_jobs.kind`.
pub const KIND: &str = "backfill-metadata";

/// Operator-supplied job parameters, stored as JSON in the job row.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BackfillParams {
    /// `false` (default): pin `dg-created-at` to the object's pre-job
    /// created time — the LastModified the proxy serves does not change.
    /// `true`: stamp the rewrite time — the object reads as modified now.
    #[serde(default)]
    pub refresh_last_modified: bool,
}

/// Parse the job row's params JSON (absent params = defaults).
pub fn parse_params(json: &str) -> Result<BackfillParams, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid backfill params: {e}"))
}

/// Pure: does this object need the backfill?
///
/// Only PASSTHROUGH objects qualify: delta and reference artifacts with
/// intact metadata already carry the canonical set, and ones with stripped
/// metadata are PATHOLOGICAL (the read path warns; a metadata stamp cannot
/// repair them — the delta linkage is gone). `fallback()` leaves
/// `file_sha256` empty, which is exactly the missing-canonical-set signal.
pub fn needs_metadata_backfill(meta: &FileMetadata) -> bool {
    matches!(meta.storage_info, StorageInfo::Passthrough) && meta.file_sha256.is_empty()
}

/// Pure: the metadata to stamp, from the object's current (fallback)
/// metadata plus the freshly computed content hashes.
///
/// `now` is the rewrite time, injected so the whole truth table is
/// unit-testable (the repo convention: no clock reads in decision logic).
pub fn backfilled_metadata(
    old: &FileMetadata,
    sha256: String,
    md5: String,
    size: u64,
    refresh_last_modified: bool,
    now: DateTime<Utc>,
) -> FileMetadata {
    let mut meta = old.clone();
    meta.tool = DELTAGLIDER_TOOL.to_string();
    meta.file_sha256 = sha256;
    meta.file_size = size;
    // Fallback metadata carries the raw ETag in the md5 slot. If it looks
    // multipart (`…-N`) and no override exists yet, preserve it as the
    // served ETag before the real MD5 replaces it — the self-copy would
    // otherwise hand clients a brand-new ETag for unchanged content.
    if meta.multipart_etag.is_none() && meta.md5.contains('-') {
        meta.multipart_etag = Some(meta.md5.clone());
    }
    meta.md5 = md5;
    if refresh_last_modified {
        meta.created_at = now;
    }
    // else: keep old.created_at — resolved from the pre-copy LastModified,
    // so the served time survives the storage-level timestamp reset.
    meta
}

/// Split a user key into the (prefix, filename) pair the deltaspace-scoped
/// storage calls take. Mirrors the classification split in `storage::s3`.
pub fn split_key(key: &str) -> (&str, &str) {
    match key.rsplit_once('/') {
        Some((prefix, filename)) => (prefix, filename),
        None => ("", key),
    }
}

/// Stream an object's content through SHA-256 + MD5 without buffering it,
/// via the engine (so encryption stays transparent and the hash is of the
/// LOGICAL bytes — what a client GET returns).
pub async fn hash_object_content<S: crate::storage::StorageBackend>(
    engine: &crate::deltaglider::DeltaGliderEngine<S>,
    bucket: &str,
    key: &str,
) -> Result<(String, String, u64), String> {
    use crate::deltaglider::RetrieveResponse;
    use futures::StreamExt;

    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let mut size: u64 = 0;
    match engine
        .retrieve_stream(bucket, key)
        .await
        .map_err(|e| format!("read for hashing failed: {e}"))?
    {
        RetrieveResponse::Streamed { mut stream, .. } => {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| format!("read for hashing failed: {e}"))?;
                sha.update(&chunk);
                md5.update(&chunk);
                size += chunk.len() as u64;
            }
        }
        RetrieveResponse::Buffered { data, .. } => {
            sha.update(&data);
            md5.update(&data);
            size = data.len() as u64;
        }
    }
    Ok((
        hex::encode(sha.finalize()),
        hex::encode(md5.finalize()),
        size,
    ))
}

/// The backfill phase machine: `counting` → `objects`. Mirrors the
/// reencrypt executor (page-granular resume, per-object failures recorded
/// and skipped over, cancellation checked per page). No `references`
/// phase — references either carry intact metadata or are healed by the
/// write path's `heal_reference_if_corrupt`.
pub(crate) async fn execute_backfill_phases(
    db: &std::sync::Arc<tokio::sync::Mutex<crate::config_db::ConfigDb>>,
    state: &std::sync::Arc<crate::api::handlers::AppState>,
    instance_id: &str,
    job: &super::store::MaintenanceJob,
) -> Result<(), String> {
    use super::worker::{
        check_cancel, counting_phase, drain_inflight_writes, heartbeat, persist, record_failure,
        PAGE_SIZE,
    };
    use crate::job_loop::Pager;

    let bucket = &job.bucket;
    let params = job
        .params
        .as_deref()
        .map(parse_params)
        .transpose()?
        .unwrap_or_default();

    // ── Drain in-flight writes admitted before the gate armed. ──
    drain_inflight_writes(state, bucket).await?;

    let mut phase = job.phase.clone();
    let resume_token = job.continuation_token.clone();
    let mut total = job.objects_total;
    let mut done = job.objects_done;
    let mut skipped = job.objects_skipped;
    let mut failed = job.objects_failed;
    let mut bytes = job.bytes_done;

    // ── Phase: counting ──
    if phase == "counting" {
        let count = counting_phase(db, state, instance_id, job, resume_token.clone()).await?;
        total = Some(count);
        phase = "objects".to_string();
        (done, skipped, failed, bytes) = (0, 0, 0, 0);
        persist(db, job, &phase, total, 0, 0, 0, 0, None).await;
    }

    // ── Phase: objects ──
    if phase == "objects" {
        // Resume only when the job was persisted IN this phase (a fresh
        // transition from counting starts at page 0).
        let mut pager = Pager::resuming(if job.phase == "objects" {
            resume_token.clone()
        } else {
            None
        });
        while pager.begin_page().is_some() {
            check_cancel(db, job.id).await?;
            let engine = state.engine.load().clone();
            let page = match engine
                .list_objects(bucket, "", None, PAGE_SIZE, pager.token(), false)
                .await
            {
                Ok(p) => p,
                Err(e) if pager.poisoned_resume_token() => {
                    // Restart the phase from page 0: needs_metadata_backfill
                    // makes the re-scan idempotent (already-stamped objects
                    // skip). Counters are NOT reset — display drift only.
                    tracing::warn!(
                        "maintenance: job #{} objects resume token rejected ({e}); restarting phase fresh",
                        job.id
                    );
                    pager.restart_fresh();
                    persist(
                        db, job, "objects", total, done, skipped, failed, bytes, None,
                    )
                    .await;
                    continue;
                }
                Err(e) => return Err(format!("object list failed: {e}")),
            };

            for (key, _) in page.objects.iter().filter(|(k, _)| !k.ends_with('/')) {
                let meta = match engine.head(bucket, key).await {
                    Ok(m) => m,
                    Err(e) => {
                        failed += 1;
                        record_failure(
                            db,
                            job.id,
                            key,
                            &format!("could not read object metadata: {e}"),
                        )
                        .await;
                        continue;
                    }
                };
                if !needs_metadata_backfill(&meta) {
                    skipped += 1;
                    continue;
                }
                // The one bytes-read of the job: hash the logical content.
                let (sha256, md5, size) =
                    match hash_object_content(engine.as_ref(), bucket, key).await {
                        Ok(h) => h,
                        Err(e) => {
                            failed += 1;
                            record_failure(db, job.id, key, &e).await;
                            continue;
                        }
                    };
                let new_meta = backfilled_metadata(
                    &meta,
                    sha256,
                    md5,
                    size,
                    params.refresh_last_modified,
                    chrono::Utc::now(),
                );
                let (prefix, filename) = split_key(key);
                if let Err(e) = engine
                    .storage()
                    .put_passthrough_metadata(bucket, prefix, filename, &new_meta)
                    .await
                {
                    failed += 1;
                    record_failure(db, job.id, key, &format!("metadata write failed: {e}")).await;
                    continue;
                }
                // The 10-minute metadata cache would otherwise keep serving
                // the pre-backfill fallback shape on LIST.
                engine.invalidate_metadata_cache(bucket, key);
                done += 1;
                bytes += size as i64;
            }

            let more = pager.advance(page.is_truncated, page.next_continuation_token);
            persist(
                db,
                job,
                "objects",
                total,
                done,
                skipped,
                failed,
                bytes,
                pager.token(),
            )
            .await;
            heartbeat(db, job.id, instance_id).await?;
            if !more {
                break;
            }
        }
        if pager.truncated_by_page_budget() {
            // Falling through would report `completed` with the tail still
            // unstamped — silent truncation.
            return Err("backfill stopped at the page budget with more pages \
                 pending — bucket too large for one pass; job left resumable \
                 in phase 'objects' (cursor persisted)"
                .to_string());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fallback_meta(md5: &str) -> FileMetadata {
        FileMetadata::fallback(
            "app.zip".to_string(),
            42,
            md5.to_string(),
            chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            Some("application/zip".to_string()),
            StorageInfo::Passthrough,
        )
    }

    #[test]
    fn needs_backfill_only_for_passthrough_with_empty_sha() {
        // The fallback shape: passthrough, empty sha → backfill.
        assert!(needs_metadata_backfill(&fallback_meta("abc")));
        // Canonical metadata present → skip.
        let mut full = fallback_meta("abc");
        full.file_sha256 = "deadbeef".to_string();
        assert!(!needs_metadata_backfill(&full));
        // Delta artifacts never qualify, even with an empty sha — a
        // metadata stamp cannot repair a stripped delta.
        let mut delta = fallback_meta("abc");
        delta.storage_info = StorageInfo::delta_stub(7);
        assert!(!needs_metadata_backfill(&delta));
    }

    #[test]
    fn backfill_preserves_created_at_by_default_and_refreshes_on_request() {
        let old = fallback_meta("abc");
        let now = Utc::now();
        let kept = backfilled_metadata(&old, "s".into(), "m".into(), 42, false, now);
        assert_eq!(kept.created_at, old.created_at, "served time must not move");
        let moved = backfilled_metadata(&old, "s".into(), "m".into(), 42, true, now);
        assert_eq!(moved.created_at, now, "refresh must stamp the rewrite time");
    }

    #[test]
    fn backfill_preserves_a_multipart_etag_before_replacing_md5() {
        // Fallback md5 slot holds the raw ETag; a multipart one ("…-N")
        // would be collapsed by the self-copy — it must survive as the
        // served-ETag override.
        let old = fallback_meta("d41d8cd98f00b204e9800998ecf8427e-3");
        let new = backfilled_metadata(&old, "s".into(), "realmd5".into(), 42, false, Utc::now());
        assert_eq!(
            new.multipart_etag.as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e-3")
        );
        assert_eq!(new.md5, "realmd5");
        // A simple ETag needs no override.
        let simple = backfilled_metadata(
            &fallback_meta("d41d8cd98f00b204e9800998ecf8427e"),
            "s".into(),
            "realmd5".into(),
            42,
            false,
            Utc::now(),
        );
        assert_eq!(simple.multipart_etag, None);
        // An existing override is never clobbered.
        let mut pre = fallback_meta("x-2");
        pre.multipart_etag = Some("keep-1".into());
        let kept = backfilled_metadata(&pre, "s".into(), "m".into(), 42, false, Utc::now());
        assert_eq!(kept.multipart_etag.as_deref(), Some("keep-1"));
    }

    #[test]
    fn backfill_keeps_foreign_user_metadata() {
        let mut old = fallback_meta("abc");
        old.user_metadata = HashMap::from([("x-custom".to_string(), "v".to_string())]);
        let new = backfilled_metadata(&old, "s".into(), "m".into(), 1, false, Utc::now());
        assert_eq!(
            new.user_metadata.get("x-custom").map(String::as_str),
            Some("v")
        );
    }

    #[test]
    fn split_key_handles_root_and_nested() {
        assert_eq!(split_key("app.zip"), ("", "app.zip"));
        assert_eq!(split_key("a/b/app.zip"), ("a/b", "app.zip"));
    }

    #[test]
    fn params_default_preserves_timestamps() {
        assert!(!parse_params("{}").unwrap().refresh_last_modified);
        assert!(
            parse_params("{\"refresh_last_modified\":true}")
                .unwrap()
                .refresh_last_modified
        );
        assert!(parse_params("not json").is_err());
    }
}
