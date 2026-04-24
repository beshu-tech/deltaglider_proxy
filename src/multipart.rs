//! In-memory multipart upload state management
//!
//! Parts are buffered in memory until CompleteMultipartUpload assembles them
//! and passes the result through `engine.store()` for delta compression.
//! Uploads are ephemeral — lost on restart; clients handle this gracefully.

use crate::api::{PartInfo, S3Error, UploadInfo};
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Duration, Utc};
use md5::{Digest, Md5};
use parking_lot::RwLock;
use rand::Rng;
use std::collections::HashMap;

/// Data for a single uploaded part
struct PartData {
    data: Bytes,
    md5_hex: String,
    md5_raw: [u8; 16],
    size: u64,
    uploaded_at: DateTime<Utc>,
}

/// Lifecycle state of a multipart upload. Replaces the old
/// `completed: bool` flag to close a race between `complete()` and
/// `abort()` where the handler could return 204 "aborted" AFTER
/// complete had already validated parts and the subsequent
/// `engine.store*` was about to publish the object (C4 security fix).
///
/// The state machine:
///
/// ```text
///                   upload_part ↻       abort
///                      │                 │
///                      ▼                 ▼
///   [create] ─▶ Open ─▶─ begin_complete ─▶─ Completing
///                │                            │
///                │                            ├── finish_upload ──▶ (removed)
///                │                            └── rollback_upload ──▶ Open
///                │
///                └── abort ──▶ (removed)
/// ```
///
/// Invariants enforced by callers:
/// - `upload_part` rejects unless state is `Open`.
/// - `abort` rejects when state is `Completing` (409 Conflict).
/// - `begin_complete` only returns parts if state was `Open`; atomically
///   flips to `Completing` under the write lock.
/// - `finish_upload` / `rollback_upload` terminate `Completing` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultipartState {
    /// Accepting UploadPart calls. Abort is allowed.
    Open,
    /// `begin_complete` has validated and handed off parts; `engine.store*`
    /// is in flight. New UploadParts and aborts are refused.
    Completing,
}

/// State for an in-progress multipart upload
struct MultipartUpload {
    upload_id: String,
    bucket: String,
    key: String,
    created_at: DateTime<Utc>,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
    parts: HashMap<u32, PartData>,
    state: MultipartState,
}

/// Result of assembling a completed multipart upload
#[derive(Debug)]
pub struct CompletedUpload {
    pub data: Bytes,
    pub etag: String,
    pub content_type: Option<String>,
    pub user_metadata: HashMap<String, String>,
}

/// Result of completing a multipart upload without assembling into a contiguous buffer.
/// Used for non-delta-eligible files to avoid the ~2x memory spike from assembly.
pub struct CompletedParts {
    pub parts: Vec<Bytes>,
    pub etag: String,
    pub total_size: u64,
    pub content_type: Option<String>,
    pub user_metadata: HashMap<String, String>,
}

/// Internal: validated parts from the shared validation step.
struct ValidatedParts {
    part_data: Vec<Bytes>,
    etag: String,
    total_size: u64,
}

/// Default maximum number of concurrent multipart uploads.
/// Overridable via `DGP_MAX_MULTIPART_UPLOADS` env var.
fn default_max_uploads() -> usize {
    crate::config::env_parse_with_default("DGP_MAX_MULTIPART_UPLOADS", 1000)
}

/// Thread-safe in-memory store for multipart upload state
pub struct MultipartStore {
    uploads: RwLock<HashMap<String, MultipartUpload>>,
    max_object_size: u64,
    max_uploads: usize,
}

impl MultipartStore {
    pub fn new(max_object_size: u64) -> Self {
        Self {
            uploads: RwLock::new(HashMap::new()),
            max_object_size,
            max_uploads: default_max_uploads(),
        }
    }

    /// Create a new multipart upload, returns the upload ID.
    /// Returns `S3Error::SlowDown` if the maximum number of concurrent uploads is reached.
    pub fn create(
        &self,
        bucket: &str,
        key: &str,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
    ) -> Result<String, S3Error> {
        let now = Utc::now();

        // Cryptographically random upload ID (matches AWS S3 behavior).
        let mut random_bytes = [0u8; 16];
        rand::rngs::OsRng.fill(&mut random_bytes);
        let upload_id = hex::encode(random_bytes); // 32 hex chars

        let mut uploads = self.uploads.write();

        // Enforce maximum concurrent uploads to prevent resource exhaustion
        if uploads.len() >= self.max_uploads {
            return Err(S3Error::SlowDown(format!(
                "Too many concurrent multipart uploads (max {})",
                self.max_uploads
            )));
        }

        let upload = MultipartUpload {
            upload_id: upload_id.clone(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            created_at: now,
            content_type,
            user_metadata,
            parts: HashMap::new(),
            state: MultipartState::Open,
        };

        uploads.insert(upload_id.clone(), upload);
        Ok(upload_id)
    }

    /// Upload a part, returns the quoted ETag (MD5 hex).
    pub fn upload_part(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        part_number: u32,
        data: Bytes,
    ) -> Result<String, S3Error> {
        if !(1..=10000).contains(&part_number) {
            return Err(S3Error::InvalidArgument(
                "Part number must be between 1 and 10000".to_string(),
            ));
        }

        let md5_raw: [u8; 16] = Md5::digest(&data).into();
        let md5_hex = hex::encode(md5_raw);
        let etag = format!("\"{}\"", md5_hex);
        let size = data.len() as u64;

        let mut uploads = self.uploads.write();
        let upload = uploads
            .get_mut(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;

        // Validate bucket+key match
        if upload.bucket != bucket || upload.key != key {
            return Err(S3Error::NoSuchUpload(upload_id.to_string()));
        }

        // C4 security fix: parts can only be uploaded while the upload is
        // Open. Once CompleteMultipartUpload has started (state=Completing),
        // accepting new parts would race with the in-flight `engine.store*`.
        if upload.state != MultipartState::Open {
            return Err(S3Error::InvalidRequest(
                "Upload is in the process of being completed; no more parts can be added"
                    .to_string(),
            ));
        }

        // Overwrite semantics: re-uploading same part_number replaces previous data
        upload.parts.insert(
            part_number,
            PartData {
                data,
                md5_hex,
                md5_raw,
                size,
                uploaded_at: Utc::now(),
            },
        );

        Ok(etag)
    }

    /// Get the size of a specific uploaded part (for quota pre-check).
    pub fn get_part_size(&self, upload_id: &str, part_number: u32) -> Option<u64> {
        let uploads = self.uploads.read();
        uploads
            .get(upload_id)
            .and_then(|u| u.parts.get(&part_number))
            .map(|p| p.data.len() as u64)
    }

    /// Begin completion: validate parts, atomically transition to
    /// `Completing`, and return the assembled buffer. After this call
    /// the upload is reserved — new UploadParts AND abort are refused
    /// (409) until the caller invokes `finish_upload` or
    /// `rollback_upload`. This closes the C4 complete/abort race.
    ///
    /// On validation failure the state is NOT changed (upload stays
    /// `Open` so the client can retry with corrected part metadata).
    pub fn complete(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)], // (part_number, etag)
    ) -> Result<CompletedUpload, S3Error> {
        let mut uploads = self.uploads.write();

        // Refuse if the upload is already Completing — only one complete
        // may be in flight at a time. Double-complete returns 404 to
        // preserve the prior contract.
        if let Some(u) = uploads.get(upload_id) {
            if u.state == MultipartState::Completing {
                return Err(S3Error::InvalidRequest(
                    "Upload is already being completed".to_string(),
                ));
            }
        }

        let (validated, upload) =
            self.validate_parts(&uploads, upload_id, bucket, key, requested_parts)?;

        let mut assembled = BytesMut::new();
        for part in &validated.part_data {
            assembled.extend_from_slice(part);
        }

        let result = CompletedUpload {
            data: assembled.freeze(),
            etag: validated.etag,
            content_type: upload.content_type.clone(),
            user_metadata: upload.user_metadata.clone(),
        };

        // Flip to Completing under the same write lock that performed the
        // validation — atomic with respect to `abort` and `upload_part`.
        if let Some(u) = uploads.get_mut(upload_id) {
            u.state = MultipartState::Completing;
        }

        Ok(result)
    }

    /// Begin-complete variant that returns the individual part buffers
    /// instead of a single contiguous buffer — avoids the ~2x memory
    /// spike for non-delta-eligible files. Same state-machine semantics
    /// as `complete()`.
    pub fn complete_parts(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)],
    ) -> Result<CompletedParts, S3Error> {
        let mut uploads = self.uploads.write();

        if let Some(u) = uploads.get(upload_id) {
            if u.state == MultipartState::Completing {
                return Err(S3Error::InvalidRequest(
                    "Upload is already being completed".to_string(),
                ));
            }
        }

        let (validated, upload) =
            self.validate_parts(&uploads, upload_id, bucket, key, requested_parts)?;

        let result = CompletedParts {
            parts: validated.part_data,
            etag: validated.etag,
            total_size: validated.total_size,
            content_type: upload.content_type.clone(),
            user_metadata: upload.user_metadata.clone(),
        };

        if let Some(u) = uploads.get_mut(upload_id) {
            u.state = MultipartState::Completing;
        }

        Ok(result)
    }

    /// Roll the upload back to `Open` after a failed engine.store*.
    /// The client is expected to retry CompleteMultipartUpload with the
    /// same part set — this matches S3's behaviour when the backing
    /// store rejects a complete.
    ///
    /// Idempotent: if the upload was already removed (e.g. via a
    /// concurrent abort after rollback), does nothing.
    pub fn rollback_upload(&self, upload_id: &str) {
        if let Some(u) = self.uploads.write().get_mut(upload_id) {
            u.state = MultipartState::Open;
        }
    }

    /// Finalise a completed upload after `engine.store*` succeeds.
    /// Removes the upload from the map. This is the terminal state.
    /// Semantically equivalent to the previous `remove_upload`.
    pub fn finish_upload(&self, upload_id: &str) {
        self.uploads.write().remove(upload_id);
    }

    /// Shared validation for `complete()` and `complete_parts()`.
    ///
    /// Looks up the upload, validates part ordering and ETags, enforces size limits,
    /// and computes the S3-compatible multipart ETag. Returns validated part data
    /// and a reference to the upload (for content_type / user_metadata).
    fn validate_parts<'a>(
        &self,
        uploads: &'a HashMap<String, MultipartUpload>,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)],
    ) -> Result<(ValidatedParts, &'a MultipartUpload), S3Error> {
        let upload = uploads
            .get(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;
        if upload.bucket != bucket || upload.key != key {
            return Err(S3Error::NoSuchUpload(upload_id.to_string()));
        }

        if requested_parts.is_empty() {
            return Err(S3Error::InvalidPart(
                "You must specify at least one part".to_string(),
            ));
        }

        // Validate ascending order
        for window in requested_parts.windows(2) {
            if window[0].0 >= window[1].0 {
                return Err(S3Error::InvalidPartOrder);
            }
        }

        // Validate each part exists and ETags match; compute total size
        let mut total_size: u64 = 0;
        let mut md5_concat = Vec::new();
        let mut part_data = Vec::with_capacity(requested_parts.len());

        for (part_number, requested_etag) in requested_parts {
            let part = upload.parts.get(part_number).ok_or_else(|| {
                S3Error::InvalidPart(format!("Part {} has not been uploaded", part_number))
            })?;

            // Normalize ETags for comparison (strip quotes)
            let requested_clean = requested_etag.trim_matches('"');
            if requested_clean != part.md5_hex {
                return Err(S3Error::InvalidPart(format!(
                    "ETag mismatch for part {}: expected \"{}\", got \"{}\"",
                    part_number, part.md5_hex, requested_clean
                )));
            }

            total_size += part.size;
            if total_size > self.max_object_size {
                return Err(S3Error::InvalidArgument(format!(
                    "Assembled object size {} exceeds maximum {}",
                    total_size, self.max_object_size
                )));
            }

            md5_concat.extend_from_slice(&part.md5_raw);
            part_data.push(part.data.clone());
        }

        // S3-compatible multipart ETag: MD5(concat of part MD5 raw bytes)-N
        let final_md5 = Md5::digest(&md5_concat);
        let etag = format!("\"{}-{}\"", hex::encode(final_md5), requested_parts.len());

        Ok((
            ValidatedParts {
                part_data,
                etag,
                total_size,
            },
            upload,
        ))
    }

    /// Abort a multipart upload. Validates bucket+key match.
    ///
    /// C4 security fix: refuse when the upload is already in
    /// `Completing` state. Accepting the abort at that point would
    /// race with the in-flight `engine.store*` and return a 204
    /// "aborted" even though the object actually lands. Clients
    /// should wait for the CompleteMultipartUpload response instead.
    pub fn abort(&self, upload_id: &str, bucket: &str, key: &str) -> Result<(), S3Error> {
        let mut uploads = self.uploads.write();
        let upload = uploads
            .get(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;

        if upload.bucket != bucket || upload.key != key {
            return Err(S3Error::NoSuchUpload(upload_id.to_string()));
        }

        if upload.state == MultipartState::Completing {
            return Err(S3Error::InvalidRequest(
                "Cannot abort: upload is currently being completed".to_string(),
            ));
        }

        uploads.remove(upload_id);
        Ok(())
    }

    /// List parts for an upload. Validates bucket+key match.
    pub fn list_parts(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Vec<PartInfo>, S3Error> {
        let uploads = self.uploads.read();
        let upload = uploads
            .get(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;

        if upload.bucket != bucket || upload.key != key {
            return Err(S3Error::NoSuchUpload(upload_id.to_string()));
        }

        let mut parts: Vec<PartInfo> = upload
            .parts
            .iter()
            .map(|(&num, pd)| PartInfo {
                part_number: num,
                etag: format!("\"{}\"", pd.md5_hex),
                size: pd.size,
                last_modified: pd.uploaded_at,
            })
            .collect();
        parts.sort_by_key(|p| p.part_number);
        Ok(parts)
    }

    /// List uploads, optionally filtered by bucket and prefix.
    pub fn list_uploads(&self, bucket: Option<&str>, prefix: Option<&str>) -> Vec<UploadInfo> {
        let uploads = self.uploads.read();
        let mut result: Vec<UploadInfo> = uploads
            .values()
            .filter(|u| {
                if let Some(b) = bucket {
                    if u.bucket != b {
                        return false;
                    }
                }
                if let Some(p) = prefix {
                    if !u.key.starts_with(p) {
                        return false;
                    }
                }
                true
            })
            .map(|u| UploadInfo {
                key: u.key.clone(),
                upload_id: u.upload_id.clone(),
                initiated: u.created_at,
            })
            .collect();
        result.sort_by(|a, b| a.key.cmp(&b.key).then(a.upload_id.cmp(&b.upload_id)));
        result
    }

    /// Remove uploads older than max_age.
    pub fn cleanup_expired(&self, max_age: std::time::Duration) {
        let cutoff = Utc::now() - Duration::from_std(max_age).unwrap_or(Duration::hours(1));
        self.uploads.write().retain(|_, u| u.created_at > cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_upload_part() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let data = Bytes::from(vec![0u8; 1024]);
        let etag = store
            .upload_part(&upload_id, "bucket", "key.bin", 1, data)
            .unwrap();
        assert!(etag.starts_with('"'));
        assert!(etag.ends_with('"'));
    }

    #[test]
    fn test_complete_roundtrip() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let part1 = Bytes::from(vec![1u8; 100]);
        let part2 = Bytes::from(vec![2u8; 200]);
        let etag1 = store
            .upload_part(&upload_id, "bucket", "key.bin", 1, part1.clone())
            .unwrap();
        let etag2 = store
            .upload_part(&upload_id, "bucket", "key.bin", 2, part2.clone())
            .unwrap();

        let result = store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag1), (2, etag2)])
            .unwrap();

        assert_eq!(result.data.len(), 300);
        assert_eq!(&result.data[..100], &[1u8; 100]);
        assert_eq!(&result.data[100..], &[2u8; 200]);
        assert!(result.etag.ends_with("-2\""));
    }

    #[test]
    fn test_abort() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();
        store.abort(&upload_id, "bucket", "key.bin").unwrap();

        let result = store.upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            1,
            Bytes::from(vec![0u8; 10]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_bucket_key_mismatch() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket-a", "key.bin", None, HashMap::new())
            .unwrap();

        let result = store.upload_part(
            &upload_id,
            "bucket-b",
            "key.bin",
            1,
            Bytes::from(vec![0u8; 10]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_part_number() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let result = store.upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            0,
            Bytes::from(vec![0u8; 10]),
        );
        assert!(result.is_err());

        let result = store.upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            10001,
            Bytes::from(vec![0u8; 10]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_list_parts() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        for i in 1..=3 {
            store
                .upload_part(
                    &upload_id,
                    "bucket",
                    "key.bin",
                    i,
                    Bytes::from(vec![i as u8; 100]),
                )
                .unwrap();
        }

        let parts = store.list_parts(&upload_id, "bucket", "key.bin").unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].part_number, 1);
        assert_eq!(parts[1].part_number, 2);
        assert_eq!(parts[2].part_number, 3);
    }

    #[test]
    fn test_overwrite_part() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let etag1 = store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                1,
                Bytes::from(vec![1u8; 100]),
            )
            .unwrap();
        let etag2 = store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                1,
                Bytes::from(vec![2u8; 100]),
            )
            .unwrap();

        assert_ne!(etag1, etag2);

        let parts = store.list_parts(&upload_id, "bucket", "key.bin").unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].etag, etag2);
    }

    #[test]
    fn test_complete_with_zero_parts() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();
        store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                1,
                Bytes::from(vec![1u8; 100]),
            )
            .unwrap();

        // Complete with empty parts list should fail
        let result = store.complete(&upload_id, "bucket", "key.bin", &[]);
        assert!(result.is_err(), "complete with zero parts should fail");
    }

    #[test]
    fn test_complete_with_wrong_etag() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();
        store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                1,
                Bytes::from(vec![1u8; 100]),
            )
            .unwrap();

        // Complete with wrong etag should fail
        let result = store.complete(
            &upload_id,
            "bucket",
            "key.bin",
            &[(1, "\"wrong_etag\"".to_string())],
        );
        assert!(result.is_err(), "complete with wrong etag should fail");
    }

    #[test]
    fn test_complete_with_non_contiguous_parts() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let part1 = Bytes::from(vec![1u8; 100]);
        let part3 = Bytes::from(vec![3u8; 100]);
        let etag1 = store
            .upload_part(&upload_id, "bucket", "key.bin", 1, part1)
            .unwrap();
        let etag3 = store
            .upload_part(&upload_id, "bucket", "key.bin", 3, part3)
            .unwrap();

        // Parts 1 and 3 (skip 2) — should succeed per S3 spec
        let result = store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag1), (3, etag3)])
            .unwrap();
        assert_eq!(result.data.len(), 200);
        assert_eq!(&result.data[..100], &[1u8; 100]);
        assert_eq!(&result.data[100..], &[3u8; 100]);
    }

    #[test]
    fn test_max_uploads_limit() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        // Override max_uploads for testing
        let store = MultipartStore {
            max_uploads: 3,
            ..store
        };

        // Create 3 uploads (at limit)
        for i in 0..3 {
            store
                .create("bucket", &format!("key{}.bin", i), None, HashMap::new())
                .unwrap();
        }

        // 4th upload should fail
        let result = store.create("bucket", "key3.bin", None, HashMap::new());
        assert!(result.is_err());
    }

    // === C4 security fix: state-machine tests ===

    fn seed_upload(store: &MultipartStore) -> String {
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();
        let data = Bytes::from(vec![0u8; 100]);
        store
            .upload_part(&upload_id, "bucket", "key.bin", 1, data)
            .unwrap();
        upload_id
    }

    #[test]
    fn test_complete_flips_state_to_completing() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };

        // Before complete → Open.
        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Open
        );

        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        // After complete, upload stays in map but as Completing.
        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Completing
        );
    }

    #[test]
    fn test_abort_refused_when_completing() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };

        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        let err = store.abort(&upload_id, "bucket", "key.bin").unwrap_err();
        assert!(matches!(err, S3Error::InvalidRequest(_)));
        // Upload still in map, still Completing.
        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Completing
        );
    }

    #[test]
    fn test_upload_part_refused_when_completing() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };
        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        let err = store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                2,
                Bytes::from(vec![0u8; 50]),
            )
            .unwrap_err();
        assert!(matches!(err, S3Error::InvalidRequest(_)));
    }

    #[test]
    fn test_rollback_upload_returns_to_open() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };
        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        // Simulate engine.store* failure → rollback.
        store.rollback_upload(&upload_id);

        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Open
        );

        // Client can now retry Complete or abort.
        store.abort(&upload_id, "bucket", "key.bin").unwrap();
    }

    #[test]
    fn test_finish_upload_removes_entry() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };
        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        store.finish_upload(&upload_id);

        assert!(store.uploads.read().get(&upload_id).is_none());
    }

    #[test]
    fn test_double_complete_returns_conflict() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };

        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag.clone())])
            .unwrap();
        let err = store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap_err();
        assert!(
            matches!(err, S3Error::InvalidRequest(_)),
            "double-complete should return InvalidRequest while in Completing, got {:?}",
            err
        );
    }

    #[test]
    fn test_validation_failure_does_not_change_state() {
        // If complete() fails validation (wrong etag), state must stay Open
        // so the client can retry with correct metadata.
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);

        let err = store
            .complete(
                &upload_id,
                "bucket",
                "key.bin",
                &[(1, "\"wrong-etag\"".to_string())],
            )
            .unwrap_err();
        assert!(matches!(err, S3Error::InvalidPart(_)));
        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Open,
            "validation failure must leave upload Open for retry"
        );
    }

    #[test]
    fn test_abort_while_open_drops_upload() {
        // Baseline: abort on an Open upload still works normally.
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        store.abort(&upload_id, "bucket", "key.bin").unwrap();
        assert!(store.uploads.read().get(&upload_id).is_none());
    }
}
