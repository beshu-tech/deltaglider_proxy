// SPDX-License-Identifier: BUSL-1.1

//! DG metadata on S3 objects: headers to and from `FileMetadata`, the
//! metadata self-copy plan, native SSE headers and the size limit.

use super::*;

/// What a metadata self-copy (MetadataDirective REPLACE) must restate so
/// the object keeps everything that is not DG metadata (D17).
/// NOT restated: the ACL (CopyObject never copies it; the object gets the
/// bucket default), Object Lock settings, and tags (copied by default).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct SelfCopyPlan {
    pub metadata: HashMap<String, String>,
    pub cache_control: Option<String>,
    pub content_disposition: Option<String>,
    pub content_encoding: Option<String>,
    pub content_language: Option<String>,
    pub expires: Option<aws_sdk_s3::primitives::DateTime>,
    pub website_redirect_location: Option<String>,
    pub storage_class: Option<aws_sdk_s3::types::StorageClass>,
    pub sse: Option<aws_sdk_s3::types::ServerSideEncryption>,
    pub kms_key_id: Option<String>,
    pub bucket_key_enabled: Option<bool>,
}

/// Metadata keys DG owns: the new metadata restates them, so stale values
/// from the HEAD are dropped (the pre-D17 behaviour for these keys).
pub(super) fn is_dg_owned_meta_key(key: &str) -> bool {
    const LEGACY: &[&str] = &[
        "tool",
        "original-name",
        "source-name",
        "file-sha256",
        "file-size",
        "created-at",
        "note",
        "ref-path",
        "ref-key",
        "ref-sha256",
        "delta-size",
        "delta-cmd",
        "content-type",
    ];
    let k = key.to_ascii_lowercase();
    k.starts_with("dg-") || k.starts_with("user-") || LEGACY.contains(&k.as_str())
}

/// Pure: build the self-copy plan from the pre-copy HEAD and the new DG
/// metadata. `native_sse_configured` = the backend sets its own SSE on the
/// request; otherwise the object's current SSE is kept.
pub(crate) fn self_copy_plan(
    head: &aws_sdk_s3::operation::head_object::HeadObjectOutput,
    dg_metadata: HashMap<String, String>,
    native_sse_configured: bool,
) -> SelfCopyPlan {
    use aws_sdk_s3::types::{ServerSideEncryption, StorageClass};
    let mut metadata: HashMap<String, String> = head
        .metadata()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !is_dg_owned_meta_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    metadata.extend(dg_metadata);
    let (sse, kms_key_id, bucket_key_enabled) = if native_sse_configured {
        (None, None, None)
    } else {
        let sse = head.server_side_encryption().cloned();
        let kms = matches!(
            sse,
            Some(ServerSideEncryption::AwsKms | ServerSideEncryption::AwsKmsDsse)
        );
        (
            sse,
            head.ssekms_key_id().filter(|_| kms).map(String::from),
            head.bucket_key_enabled().filter(|_| kms),
        )
    };
    SelfCopyPlan {
        metadata,
        cache_control: head.cache_control().map(String::from),
        content_disposition: head.content_disposition().map(String::from),
        content_encoding: head.content_encoding().map(String::from),
        content_language: head.content_language().map(String::from),
        expires: head.expires_string().and_then(|s| {
            aws_sdk_s3::primitives::DateTime::from_str(
                s,
                aws_sdk_s3::primitives::DateTimeFormat::HttpDate,
            )
            .ok()
        }),
        website_redirect_location: head.website_redirect_location().map(String::from),
        storage_class: head
            .storage_class()
            .filter(|c| **c != StorageClass::Standard)
            .cloned(),
        sse,
        kms_key_id,
        bucket_key_enabled,
    }
}

pub(super) fn resolve_created_at(
    meta_value: Option<String>,
    fallback: DateTime<Utc>,
) -> DateTime<Utc> {
    let Some(raw) = meta_value else {
        return fallback;
    };
    let raw = raw.trim();
    // Try full RFC3339 first — it correctly handles `Z`, lowercase `z`, and any
    // numeric offset (`+02:00`), converting to UTC. Only then fall back to the
    // proxy's historical no-offset naive shape. NEVER string-surgery the offset:
    // the old `trim_end_matches('Z') + "+00:00"` silently dropped offset/`z`
    // values to the fallback (multi-agent review finding).
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.fZ").map(|n| n.and_utc())
        })
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f").map(|n| n.and_utc())
        })
        .unwrap_or(fallback)
}

/// The headers of a HEAD or GET object response that carry its metadata.
pub(super) struct ObjectHeaders<'a> {
    pub(super) user: Option<&'a HashMap<String, String>>,
    pub(super) last_modified: Option<&'a aws_sdk_s3::primitives::DateTime>,
    pub(super) e_tag: Option<&'a str>,
    pub(super) content_length: Option<i64>,
    pub(super) content_type: Option<&'a str>,
}

impl S3Backend {
    /// Convert FileMetadata to S3 metadata headers (bare dg-* keys).
    /// The S3 SDK auto-prepends `x-amz-meta-` when using `.metadata()`.
    /// Delegates to `FileMetadata::to_bare_metadata_map()` (single source of truth).
    pub(super) fn metadata_to_headers(&self, metadata: &FileMetadata) -> HashMap<String, String> {
        metadata.to_bare_metadata_map()
    }

    /// Convert S3 metadata headers to FileMetadata
    /// `created_at_fallback` is the object's stable S3 `LastModified`, used when
    /// the DG metadata omits `dg-created-at`. NEVER fall back to `Utc::now()`
    /// here: a synthesised "now" makes replication's NewerWins (which compares
    /// `created_at`) re-copy the object every tick. See the RCA in
    /// docs/plan/rca-replication-recopy-2026-06-30.md.
    pub(super) fn headers_to_metadata(
        &self,
        headers: &HashMap<String, String>,
        created_at_fallback: DateTime<Utc>,
    ) -> Result<FileMetadata, StorageError> {
        use crate::types::meta_keys as mk;

        let get_value = |keys: &[&str]| -> Option<String> {
            for key in keys {
                if let Some(v) = headers.get(*key) {
                    if !v.is_empty() {
                        return Some(v.clone());
                    }
                }
            }
            None
        };

        let tool = get_value(&[mk::TOOL, "tool"])
            .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::TOOL)))?;
        let original_name = get_value(&[
            mk::ORIGINAL_NAME,
            "original-name",
            mk::SOURCE_NAME,
            "source-name",
        ])
        .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::ORIGINAL_NAME)))?;
        let file_sha256 = get_value(&[mk::FILE_SHA256, "file-sha256"])
            .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::FILE_SHA256)))?;
        let file_size_str =
            get_value(&[mk::FILE_SIZE, "file-size"]).unwrap_or_else(|| "0".to_string());
        let file_size: u64 = file_size_str
            .parse()
            .map_err(|_| StorageError::Other(format!("Invalid file size: {}", file_size_str)))?;
        // Missing/malformed `dg-created-at` → the object's stable S3
        // LastModified, NOT `now()` (see the doc-comment above + the RCA).
        let created_at = resolve_created_at(
            get_value(&[mk::CREATED_AT, "created-at"]),
            created_at_fallback,
        );

        let note = get_value(&[mk::NOTE, "note"]);
        // Read ref path: try new name (dg-ref-path) first, fall back to legacy (dg-ref-key, ref-key)
        let ref_path_opt = get_value(&[mk::REF_PATH, mk::REF_KEY, "ref-path", "ref-key"]);
        let is_reference = note.as_deref() == Some("reference");
        let is_delta = ref_path_opt.is_some()
            || note
                .as_ref()
                .map(|n| n == "delta" || n.starts_with("zero-diff"))
                .unwrap_or(false);

        let storage_info = if is_reference {
            let source_name = get_value(&[mk::SOURCE_NAME, "source-name"])
                .unwrap_or_else(|| original_name.clone());
            StorageInfo::Reference { source_name }
        } else if is_delta {
            let raw_ref_path = ref_path_opt
                .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::REF_PATH)))?;
            // Normalize: if absolute (legacy), extract just the filename (typically "reference.bin")
            let ref_path = if raw_ref_path.contains('/') {
                raw_ref_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&raw_ref_path)
                    .to_string()
            } else {
                raw_ref_path
            };
            let ref_sha256 = get_value(&[mk::REF_SHA256, "ref-sha256"])
                .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::REF_SHA256)))?;
            let delta_size_str =
                get_value(&[mk::DELTA_SIZE, "delta-size"]).unwrap_or_else(|| "0".to_string());
            let delta_size: u64 = delta_size_str.parse().map_err(|_| {
                StorageError::Other(format!("Invalid delta size: {}", delta_size_str))
            })?;
            let delta_cmd = get_value(&[mk::DELTA_CMD, "delta-cmd"]).unwrap_or_default();
            StorageInfo::Delta {
                ref_path,
                ref_sha256,
                delta_size,
                delta_cmd,
            }
        } else {
            StorageInfo::Passthrough
        };

        let md5 = headers
            .get(mk::MD5)
            .cloned()
            .unwrap_or_else(|| "".to_string());

        let user_metadata: std::collections::HashMap<String, String> = headers
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix("user-")
                    .map(|suffix| (suffix.to_string(), v.clone()))
            })
            .collect();

        let multipart_etag = get_value(&["dg-multipart-etag"]);
        Ok(FileMetadata {
            tool,
            original_name,
            file_sha256,
            file_size,
            md5,
            multipart_etag,
            created_at,
            // Use the empty-skipping `get_value` (not raw `headers.get`): an
            // object stored with no/blank content-type leaves an empty
            // `content-type` user-metadata value, which must read back as None
            // so the output layer can apply the octet-stream default. A raw
            // `.cloned()` would yield Some("") and emit a blank content-type.
            content_type: get_value(&["content-type"]),
            user_metadata,
            storage_info,
        })
    }

    /// Get object metadata from S3 headers
    pub(super) async fn get_object_metadata(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<FileMetadata, StorageError> {
        BACKEND_HEAD_REQUESTS.inc();
        let response = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if let SdkError::ServiceError(service_error) = &e {
                    if matches!(
                        service_error.err(),
                        aws_sdk_s3::operation::head_object::HeadObjectError::NotFound(_)
                    ) {
                        return StorageError::NotFound(key.to_string());
                    }
                }
                self.classify(bucket, &e, S3Op::HeadObject)
            })?;

        Ok(self.metadata_of_response(
            bucket,
            key,
            &ObjectHeaders {
                user: response.metadata(),
                last_modified: response.last_modified(),
                e_tag: response.e_tag(),
                content_length: response.content_length(),
                content_type: response.content_type(),
            },
        ))
    }

    /// The metadata of the object `key` from the headers of a HEAD or GET
    /// response: its DG metadata, or the fallback passthrough metadata of
    /// an object without it.
    pub(super) fn metadata_of_response(
        &self,
        bucket: &str,
        key: &str,
        response: &ObjectHeaders<'_>,
    ) -> FileMetadata {
        let headers: HashMap<String, String> = response
            .user
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        // The object's S3 LastModified — a STABLE per-object timestamp. Used as
        // the `created_at` fallback when DG metadata is present but carries no
        // `dg-created-at` (e.g. checksum sidecars uploaded with partial DG
        // metadata). Without this, `headers_to_metadata` would synthesise
        // `Utc::now()` on every read, making replication's NewerWins re-copy the
        // object every tick forever. See docs/plan/rca-replication-recopy-2026-06-30.md.
        let s3_last_modified = response
            .last_modified
            .and_then(|t| {
                DateTime::parse_from_rfc3339(&t.to_string())
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            })
            .unwrap_or_else(Utc::now);

        // A `.delta` / `reference.bin` file is delta-machinery: missing DG
        // metadata on one of those genuinely breaks reconstruction and is worth
        // a loud WARN. Any other object (e.g. a `.sha1`/`.sha512` checksum
        // sidecar, an image, anything copied without `--metadata`) falling back
        // to passthrough is entirely benign — it just has no delta to track.
        // Logging that at WARN per-object floods the log on every
        // `metadata=true` listing (25k+ lines observed on a build-artifact
        // bucket), so it goes to DEBUG. This is the level gate, computed once.
        let is_delta_file = key.ends_with(".delta");
        let is_reference = key.ends_with("reference.bin");
        let delta_critical = is_delta_file || is_reference;

        // Try parsing DG metadata from headers. If headers are empty or
        // corrupted (missing required fields), fall back to passthrough metadata
        // from the HEAD response itself.
        if !headers.is_empty() {
            match self.headers_to_metadata(&headers, s3_last_modified) {
                Ok(meta) => {
                    let stored_etag = response.e_tag.unwrap_or_default();
                    let stored_size = response.content_length.unwrap_or(0).max(0) as u64;
                    self.remember_listed_facts(bucket, key, stored_etag, stored_size, &meta);
                    self.backfill_listing_facts(bucket, key, stored_etag, stored_size, &meta);
                    return meta;
                }
                Err(e) if delta_critical => {
                    warn!(
                        "PATHOLOGICAL | {} file {}/{} has missing/corrupt DG metadata — \
                         delta reconstruction will not work. Was this file copied without \
                         preserving S3 metadata? Re-copy with: \
                         rclone copy src:bucket dst:bucket --metadata. Error: {}",
                        if is_reference { "Reference" } else { "Delta" },
                        bucket,
                        key,
                        e
                    );
                }
                Err(e) => {
                    debug!(
                        "No DG metadata for {}/{} — serving as passthrough \
                         (likely copied without --metadata). Error: {}",
                        bucket, key, e
                    );
                }
            }
        } else if delta_critical {
            // Object exists on upstream S3 but carries NO metadata at all, and
            // it's a delta/reference file — reconstruction is broken.
            warn!(
                "PATHOLOGICAL | {} file {}/{} has NO DG metadata! \
                 Delta reconstruction will not work. Was this file copied without preserving S3 metadata? \
                 Re-copy with: rclone copy src:bucket dst:bucket --metadata",
                if is_reference { "Reference" } else { "Delta" },
                bucket,
                key
            );
        }
        // Treat as passthrough with best-effort metadata from HEAD response.
        let file_size = response.content_length.unwrap_or(0).max(0) as u64;
        let last_modified = s3_last_modified;
        // Upstream S3 returns the ETag already wrapped in quotes (e.g.
        // `"abc123"`). FileMetadata.md5 must hold the BARE value — the
        // response-emit layer re-adds the quotes when forming the HEAD/GET
        // ETag. Storing it quoted here produced a doubled-quote ETag
        // (`""abc123""`) that strict S3 clients reject. Strip to match the
        // listing path (`object.e_tag.map(|e| e.trim_matches('"'))`).
        let etag = response
            .e_tag
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let content_type = response.content_type.map(|s| s.to_string());
        FileMetadata::fallback(
            key.rsplit('/').next().unwrap_or(key).to_string(),
            file_size,
            etag,
            last_modified,
            content_type,
            StorageInfo::Passthrough,
        )
    }
}

/// Apply native S3 encryption headers to a PutObject builder in
/// accordance with the configured mode.
///
/// Kept as a free function (not a method on `S3Backend`) so the
/// signature doesn't get borrowed-self awkward during retry loops
/// that rebuild the request object on each attempt. Takes the
/// `NativeEncryptionConfig` by reference — the builder absorbs the
/// `String` clone only when SseKms is actually in use.
pub(super) fn apply_native_encryption(
    mut request: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    cfg: &NativeEncryptionConfig,
) -> aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder {
    use aws_sdk_s3::types::ServerSideEncryption;
    match cfg {
        NativeEncryptionConfig::None => {}
        NativeEncryptionConfig::SseS3 => {
            request = request.server_side_encryption(ServerSideEncryption::Aes256);
        }
        NativeEncryptionConfig::SseKms {
            kms_key_id,
            bucket_key_enabled,
        } => {
            request = request
                .server_side_encryption(ServerSideEncryption::AwsKms)
                .ssekms_key_id(kms_key_id.clone())
                .bucket_key_enabled(*bucket_key_enabled);
        }
    }
    request
}

/// Apply native SSE to a `create_multipart_upload` request (Phase B).
/// Mirrors `apply_native_encryption` for the multipart builder shape.
pub(super) fn apply_native_encryption_mpu(
    mut request: aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
    cfg: &NativeEncryptionConfig,
) -> aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder {
    use aws_sdk_s3::types::ServerSideEncryption;
    match cfg {
        NativeEncryptionConfig::None => {}
        NativeEncryptionConfig::SseS3 => {
            request = request.server_side_encryption(ServerSideEncryption::Aes256);
        }
        NativeEncryptionConfig::SseKms {
            kms_key_id,
            bucket_key_enabled,
        } => {
            request = request
                .server_side_encryption(ServerSideEncryption::AwsKms)
                .ssekms_key_id(kms_key_id.clone())
                .bucket_key_enabled(*bucket_key_enabled);
        }
    }
    request
}

/// S3's limit on the user metadata of one object, in bytes (keys + values).
pub(super) const S3_USER_METADATA_MAX_BYTES: usize = 2048;

/// Pure: whether the headers of a write (the proxy's own `dg-*` metadata
/// plus the client's) fit S3's 2 KB limit. Over it is `MetadataTooLarge`
/// (400): the client's metadata can pass the adapter's 2 KB gate while the
/// proxy's own fields push the total over, and that is not a server fault.
pub(super) fn check_metadata_size(
    headers: &HashMap<String, String>,
    bucket: &str,
    key: &str,
) -> Result<(), StorageError> {
    let total: usize = headers.iter().map(|(k, v)| k.len() + v.len()).sum();
    if total > S3_USER_METADATA_MAX_BYTES {
        return Err(StorageError::MetadataTooLarge(format!(
            "the metadata of {bucket}/{key} is {total} bytes with the proxy's own fields; \
             an S3 backend stores at most {S3_USER_METADATA_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}
