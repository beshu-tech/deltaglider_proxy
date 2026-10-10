// SPDX-License-Identifier: BUSL-1.1

//! Object reads, writes and deletes, fenced and plain.

use super::*;

/// Resolve an object's `created_at` from its (optional) `dg-created-at`
/// metadata value, falling back to `fallback` (the object's stable S3
/// `LastModified`) when the value is absent OR unparseable.
///
/// CRITICAL: the fallback is a STABLE per-object timestamp, never `Utc::now()`.
/// A synthesised "now" makes replication's NewerWins policy (which compares
/// `created_at`) re-copy the object on every tick forever — the root cause in
/// docs/plan/rca-replication-recopy-2026-06-30.md. Pure so the truth table is
/// unit-tested without an S3 client.
/// Percent-encode an S3 object key for the `x-amz-copy-source` header, which is
/// URL-decoded server-side. Encodes each `/`-separated segment (preserving the
/// path separators) so keys with `%`, `?`, `+`, `#`, spaces, etc. survive the
/// server-side decode. Pure — unit-tested without an S3 client.
pub(super) fn encode_copy_source_key(key: &str) -> String {
    key.split('/')
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Waits before the retries of a Hetzner 400 ([`is_unidentified_400`]).
pub(super) const UNIDENTIFIED_400_BACKOFF_MS: [u64; 3] = [100, 200, 400];

/// Pure: a 400 with no `x-amz-request-id`. Hetzner answers about 1-2 % of
/// requests with it (and `connection: close`), and a retry clears it. A 400
/// that carries a request id is the backend's answer to the request
/// (EntityTooLarge, InvalidArgument): a retry gets it again. The SDK
/// retries no 400, so the upload loops retry this one; every other error
/// (throttle, 5xx, timeout, a broken connection) has the SDK's retries
/// only, never a second loop on top (4 × 3 PUTs per write before).
pub(super) fn is_unidentified_400<E>(e: &SdkError<E>) -> bool {
    matches!(e, SdkError::ServiceError(svc)
        if svc.raw().status().as_u16() == 400
            && svc.raw().headers().get("x-amz-request-id").is_none())
}

impl S3Backend {
    /// The per-request config of an upload of `bytes`: an operation
    /// deadline sized for the body ([`upload_deadline`] over the request
    /// deadline of `client`). The upload client has no deadline of its own,
    /// so a stalled backend held a PUT for minutes. `None` when the request
    /// deadline is off.
    pub(super) fn upload_config(&self, bytes: u64) -> Option<aws_sdk_s3::config::Builder> {
        let base = self
            .client
            .config()
            .timeout_config()
            .and_then(|t| t.operation_timeout());
        super::client::upload_deadline(base, bytes).map(|t| {
            aws_sdk_s3::config::Builder::new().timeout_config(
                aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .operation_timeout(t)
                    .build(),
            )
        })
    }

    /// Rewrite an object's metadata WITHOUT moving its bytes: a server-side
    /// self-copy with `MetadataDirective: REPLACE`. Shared by
    /// `put_reference_metadata` and `put_object_metadata`.
    ///
    /// x-amz-copy-source is URL-DECODED server-side, so the source must be
    /// percent-encoded — a legal key char like '%', '?', '+' or '#' (e.g.
    /// "sale 50% off/") would otherwise be an invalid escape and AWS/MinIO
    /// reject the CopyObject with 400. Encoded per path segment so the '/'
    /// separators are preserved.
    ///
    /// REPLACE resets content-type along with the metadata, so the metadata's
    /// content_type is re-asserted explicitly. Native SSE headers are applied
    /// the same way the PUT path does — a bucket policy that enforces
    /// encryption would otherwise reject the copy. CopyObject caps at 5 GiB
    /// on AWS; larger objects fail here with the service error (callers
    /// record it per-object rather than aborting a whole job).
    pub(super) async fn replace_metadata_in_place(
        &self,
        bucket: &str,
        key: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.replace_metadata_in_place_fenced(bucket, key, metadata, &RefFence::Unfenced)
            .await
            .map(|_| ())
    }

    /// `replace_metadata_in_place` fenced on the object's ETag: the self-copy
    /// carries `x-amz-copy-source-if-match`, a condition every S3 backend
    /// supports. Returns the new ETag.
    pub(super) async fn replace_metadata_in_place_fenced(
        &self,
        bucket: &str,
        key: &str,
        metadata: &FileMetadata,
        fence: &RefFence,
    ) -> Result<Option<String>, StorageError> {
        let copy_source = format!("{}/{}", bucket, encode_copy_source_key(key));
        // HEAD first: REPLACE drops everything the request does not restate
        // (a foreign object's own metadata and headers, D17).
        BACKEND_HEAD_REQUESTS.inc();
        let head = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::HeadObject))?;
        let expect_etag = match fence {
            RefFence::ETag(e) if !e.is_empty() => Some(e.clone()),
            _ => None,
        };
        if let Some(e) = &expect_etag {
            if head.e_tag() != Some(e.as_str()) {
                return Err(reference_fence_lost(bucket, key));
            }
        }
        // The ACL is not on the HEAD. A backend that does not serve it (or a
        // key without s3:GetObjectAcl) keeps the old behaviour: the copy
        // gets the bucket default.
        let acl = match self
            .client
            .get_object_acl()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(acl) => Some(acl),
            Err(e) => {
                debug!("S3 metadata rewrite {bucket}/{key}: ACL not read, not restated: {e}");
                None
            }
        };
        let plan = self_copy_plan(
            &head,
            acl.as_ref(),
            self.metadata_to_headers(metadata),
            !matches!(self.native_encryption, NativeEncryptionConfig::None),
            std::time::SystemTime::now(),
        );

        let mut request = self
            .bulk_client
            .copy_object()
            .bucket(bucket)
            .copy_source(&copy_source)
            .key(key)
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace)
            .set_cache_control(plan.cache_control)
            .set_content_disposition(plan.content_disposition)
            .set_content_encoding(plan.content_encoding)
            .set_content_language(plan.content_language)
            .set_expires(plan.expires)
            .set_website_redirect_location(plan.website_redirect_location)
            .set_storage_class(plan.storage_class)
            .set_server_side_encryption(plan.sse)
            .set_ssekms_key_id(plan.kms_key_id)
            .set_bucket_key_enabled(plan.bucket_key_enabled)
            .set_object_lock_mode(plan.object_lock_mode)
            .set_object_lock_retain_until_date(plan.object_lock_retain_until)
            .set_object_lock_legal_hold_status(plan.object_lock_legal_hold);
        if let Some(acl) = plan.acl {
            request = request
                .set_grant_full_control(acl.full_control)
                .set_grant_read(acl.read)
                .set_grant_read_acp(acl.read_acp)
                .set_grant_write_acp(acl.write_acp);
        }
        if let Some(ct) = metadata.content_type.as_deref() {
            request = request.content_type(ct);
        }
        {
            use aws_sdk_s3::types::ServerSideEncryption;
            match &self.native_encryption {
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
        }

        let wanted_meta = plan.metadata.clone();
        for (k, v) in plan.metadata {
            request = request.metadata(k, v);
        }
        if let Some(e) = &expect_etag {
            request = request.copy_source_if_match(e);
        }

        let resp = match request.send().await {
            Ok(resp) => resp,
            Err(e)
                if expect_etag.is_some()
                    && crate::coordination::cas::conditional_write_lost(
                        &crate::coordination::cas::sdk_error_signal(&e),
                    ) =>
            {
                // An SDK retry of a copy whose response was lost meets our
                // own copy (a new ETag on SSE-KMS and similar): ours when
                // the object carries exactly the metadata we wrote.
                return self
                    .own_metadata_write_or_lost(bucket, key, &wanted_meta)
                    .await;
            }
            Err(e) => {
                return Err(self.classify(bucket, &e, S3Op::Other("copy_object (metadata update)")))
            }
        };
        Ok(resp
            .copy_object_result()
            .and_then(|r| r.e_tag())
            .map(str::to_string))
    }

    /// See [`Self::own_write_or_lost`]; for a metadata-only rewrite the
    /// proof is the object's user metadata, which a peer's write changes.
    pub(super) async fn own_metadata_write_or_lost(
        &self,
        bucket: &str,
        key: &str,
        wanted: &HashMap<String, String>,
    ) -> Result<Option<String>, StorageError> {
        BACKEND_HEAD_REQUESTS.inc();
        let head = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await;
        match head {
            Ok(h) if user_metadata_equal(h.metadata(), wanted) => {
                debug!("S3 metadata rewrite {bucket}/{key}: the refused retry met its own copy");
                Ok(h.e_tag().map(str::to_string))
            }
            _ => Err(reference_fence_lost(bucket, key)),
        }
    }

    /// Put an object to S3 with metadata headers. The SDK retries throttles,
    /// 5xx and timeouts; the Hetzner 400 is retried here
    /// ([`is_unidentified_400`]). Data is already fully buffered — retry is safe.
    pub(super) async fn put_object_with_metadata(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.put_object_with_metadata_fenced(bucket, key, data, metadata, &RefFence::Unfenced)
            .await
            .map(|_| ())
    }

    /// `put_object_with_metadata` with a write precondition; returns the new
    /// ETag. See [`fenced_write_verdict`] for how a refused condition reads.
    pub(super) async fn put_object_with_metadata_fenced(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        metadata: &FileMetadata,
        fence: &RefFence,
    ) -> Result<Option<String>, StorageError> {
        let mut fence = fence.clone();
        let mut headers = self.metadata_to_headers(metadata);
        // Stamp the native-encryption marker so reads know this object
        // was encrypted by AWS (not by the proxy's `EncryptingBackend`
        // wrapper). The marker is plaintext in user-metadata — SSE-KMS
        // does NOT encrypt `x-amz-meta-*` headers, only the body.
        // This is acceptable because DG metadata is never considered
        // secret (see docs/product/reference/encryption-at-rest.md).
        if let Some(marker) = self.native_encryption.marker() {
            headers.insert("dg-encrypted-native".to_string(), marker.to_string());
        }

        check_metadata_size(&headers, bucket, key)?;

        let mut backoff = UNIDENTIFIED_400_BACKOFF_MS.iter();
        for attempt in 0.. {
            let mut request = self
                .bulk_client
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from(data.to_vec()))
                .content_type("application/octet-stream");

            for (k, v) in &headers {
                request = request.metadata(k.clone(), v.clone());
            }
            request = apply_native_encryption(request, &self.native_encryption);
            request = apply_put_fence(request, &fence);
            let sent = match self.upload_config(data.len() as u64) {
                Some(c) => request.customize().config_override(c).send().await,
                None => request.send().await,
            };

            match sent {
                Ok(resp) => {
                    self.remember_listed_facts(
                        bucket,
                        key,
                        resp.e_tag().unwrap_or_default(),
                        data.len() as u64,
                        metadata,
                    );
                    self.persist_listing_facts(
                        bucket,
                        key,
                        resp.e_tag().unwrap_or_default(),
                        data.len() as u64,
                        metadata,
                    );
                    let etag = resp.e_tag().map(str::to_string);
                    if attempt > 0 {
                        debug!(
                            "S3 PUT {}/{} succeeded on attempt {} ({} bytes)",
                            bucket,
                            key,
                            attempt + 1,
                            data.len()
                        );
                    } else {
                        debug!(
                            "S3 PUT {}/{} ({} bytes) with DG metadata",
                            bucket,
                            key,
                            data.len()
                        );
                    }
                    return Ok(etag);
                }
                Err(e) => {
                    match fenced_write_verdict(
                        &fence,
                        &crate::coordination::cas::sdk_error_signal(&e),
                    ) {
                        FencedWriteVerdict::Lost => {
                            let md5 = hex::encode(<md5::Md5 as md5::Digest>::digest(data));
                            let etag = self.own_write_or_lost(bucket, key, &md5).await?;
                            let stored = data.len() as u64;
                            self.remember_listed_facts(bucket, key, &etag, stored, metadata);
                            self.persist_listing_facts(bucket, key, &etag, stored, metadata);
                            return Ok(Some(etag));
                        }
                        // Retry at once without the condition. Unfenced,
                        // the write cannot get this verdict again.
                        FencedWriteVerdict::Unsupported => {
                            warn!("S3 PUT {bucket}/{key}: the backend has no conditional writes (501); writing without the fence");
                            fence = RefFence::Unfenced;
                            continue;
                        }
                        FencedWriteVerdict::Other => {}
                    }
                    if let Some(ms) = is_unidentified_400(&e).then(|| backoff.next()).flatten() {
                        warn!(
                            "S3 PUT {}/{} ({} bytes): a 400 without a request id (attempt {}), retrying in {}ms: {:?}",
                            bucket,
                            key,
                            data.len(),
                            attempt + 1,
                            ms,
                            e,
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                        continue;
                    }
                    return Err(self.classify(bucket, &e, S3Op::PutObject));
                }
            }
        }

        // Unreachable: the loop always returns (success on Ok, error on final attempt).
        // Kept as a safety net — if control flow changes, this is better than silent success.
        unreachable!("retry loop must return on every path")
    }

    /// Put an object to S3 from a source file path with metadata headers.
    /// Uses ByteStream::from_path to avoid buffering the full payload in memory.
    pub(super) async fn put_object_file_with_metadata(
        &self,
        bucket: &str,
        key: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.put_object_file_with_metadata_fenced(
            bucket,
            key,
            source_path,
            metadata,
            &RefFence::Unfenced,
        )
        .await
        .map(|_| ())
    }

    /// A fenced PUT met a refused condition. When an earlier attempt of the
    /// same PUT (ours, or an SDK-level retry) landed but its response was
    /// lost, the retry meets our own write: the object then holds exactly
    /// our bytes, and its ETag is their MD5. That is a success, not a lost
    /// fence (a false SlowDown, and a rollback that deleted our own
    /// baseline). Any other object is a peer's write: lost.
    pub(super) async fn own_write_or_lost(
        &self,
        bucket: &str,
        key: &str,
        body_md5_hex: &str,
    ) -> Result<String, StorageError> {
        BACKEND_HEAD_REQUESTS.inc();
        let head = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await;
        match head.ok().and_then(|h| h.e_tag().map(str::to_string)) {
            Some(etag) if etag_is_body_md5(&etag, body_md5_hex) => {
                debug!("S3 PUT {bucket}/{key}: the refused retry met its own landed write");
                Ok(etag)
            }
            _ => Err(reference_fence_lost(bucket, key)),
        }
    }

    /// `put_object_file_with_metadata` with a write precondition; returns
    /// the new ETag.
    pub(super) async fn put_object_file_with_metadata_fenced(
        &self,
        bucket: &str,
        key: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
        fence: &RefFence,
    ) -> Result<Option<String>, StorageError> {
        let mut fence = fence.clone();
        let mut headers = self.metadata_to_headers(metadata);
        if let Some(marker) = self.native_encryption.marker() {
            headers.insert("dg-encrypted-native".to_string(), marker.to_string());
        }
        check_metadata_size(&headers, bucket, key)?;

        // The stored size for the listing-size cache: what this PUT sends.
        let stored_size = tokio::fs::metadata(source_path).await.ok().map(|m| m.len());
        let mut backoff = UNIDENTIFIED_400_BACKOFF_MS.iter();
        loop {
            let body = ByteStream::from_path(source_path.to_path_buf())
                .await
                .map_err(|e| {
                    StorageError::S3(format!("Failed to open source file stream: {}", e))
                })?;
            let mut request = self
                .bulk_client
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .content_type("application/octet-stream");
            for (k, v) in &headers {
                request = request.metadata(k.clone(), v.clone());
            }
            request = apply_native_encryption(request, &self.native_encryption);
            request = apply_put_fence(request, &fence);
            let sent = match self.upload_config(stored_size.unwrap_or(0)) {
                Some(c) => request.customize().config_override(c).send().await,
                None => request.send().await,
            };

            match sent {
                Ok(resp) => {
                    if let Some(size) = stored_size {
                        self.remember_listed_facts(
                            bucket,
                            key,
                            resp.e_tag().unwrap_or_default(),
                            size,
                            metadata,
                        );
                        self.persist_listing_facts(
                            bucket,
                            key,
                            resp.e_tag().unwrap_or_default(),
                            size,
                            metadata,
                        );
                    }
                    return Ok(resp.e_tag().map(str::to_string));
                }
                Err(e) => {
                    match fenced_write_verdict(
                        &fence,
                        &crate::coordination::cas::sdk_error_signal(&e),
                    ) {
                        FencedWriteVerdict::Lost => {
                            let Some(md5) = md5_hex_of_file(source_path).await else {
                                return Err(reference_fence_lost(bucket, key));
                            };
                            let etag = self.own_write_or_lost(bucket, key, &md5).await?;
                            if let Some(size) = stored_size {
                                self.remember_listed_facts(bucket, key, &etag, size, metadata);
                                self.persist_listing_facts(bucket, key, &etag, size, metadata);
                            }
                            return Ok(Some(etag));
                        }
                        // Retry at once without the condition. Unfenced,
                        // the write cannot get this verdict again.
                        FencedWriteVerdict::Unsupported => {
                            warn!("S3 PUT {bucket}/{key}: the backend has no conditional writes (501); writing without the fence");
                            fence = RefFence::Unfenced;
                            continue;
                        }
                        FencedWriteVerdict::Other => {}
                    }
                    if let Some(ms) = is_unidentified_400(&e).then(|| backoff.next()).flatten() {
                        warn!(
                            "S3 PUT {bucket}/{key}: a 400 without a request id, retrying in {ms}ms: {e:?}"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                        continue;
                    }
                    return Err(self.classify(bucket, &e, S3Op::PutObject));
                }
            }
        }
    }

    /// Get an object from S3
    pub(super) async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| self.classify_get(bucket, key, &e))?;

        let data = self.collect_body(bucket, key, response).await?;

        debug!("S3 GET {}/{} ({} bytes)", bucket, key, data.len());
        Ok(data)
    }

    /// Delete an object from S3
    /// DELETE with `If-Match` on the fence's ETag. A backend that does not
    /// support conditional deletes (501) gets the plain DELETE.
    pub(super) async fn delete_s3_object_fenced(
        &self,
        bucket: &str,
        key: &str,
        fence: &RefFence,
    ) -> Result<(), StorageError> {
        let RefFence::ETag(etag) = fence else {
            return self.delete_s3_object(bucket, key).await;
        };
        if etag.is_empty() {
            return self.delete_s3_object(bucket, key).await;
        }
        // HEAD first as well: not every S3 backend honours If-Match on
        // DELETE, and one that ignores it must still not delete a peer's
        // newer baseline (the HEAD leaves only the request-time window).
        BACKEND_HEAD_REQUESTS.inc();
        match self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(head) if head.e_tag() == Some(etag.as_str()) => {}
            Ok(_) => return Err(reference_fence_lost(bucket, key)),
            Err(e) => {
                return match self.classify(bucket, &e, S3Op::HeadObject) {
                    StorageError::NotFound(_) => Ok(()),
                    other => Err(other),
                }
            }
        }
        match self
            .client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .if_match(etag)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                match fenced_write_verdict(fence, &crate::coordination::cas::sdk_error_signal(&e)) {
                    FencedWriteVerdict::Lost => Err(reference_fence_lost(bucket, key)),
                    FencedWriteVerdict::Unsupported => self.delete_s3_object(bucket, key).await,
                    FencedWriteVerdict::Other => Err(self.classify(bucket, &e, S3Op::DeleteObject)),
                }
            }
        }
    }

    pub(super) async fn delete_s3_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(), StorageError> {
        self.delete_s3_object_dated(bucket, key).await.map(|_| ())
    }

    /// DELETE, and return the server time of the delete (the response's
    /// `Date`, when it has one): the facts cleanup keeps entries written
    /// after it.
    pub(super) async fn delete_s3_object_dated(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<i64>, StorageError> {
        let date = crate::coordination::server_clock::ServerDate::default();
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .customize()
            .interceptor(date.clone())
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::DeleteObject))?;

        debug!("S3 DELETE {}/{}", bucket, key);
        Ok(date.get())
    }
}

/// Add the fence of a reference write to a PutObject.
pub(super) fn apply_put_fence(
    req: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    fence: &RefFence,
) -> aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder {
    match fence {
        RefFence::Absent => req.if_none_match("*"),
        RefFence::ETag(e) if !e.is_empty() => req.if_match(e),
        _ => req,
    }
}

/// Pure: does an object's user metadata (as a HEAD returns it; S3
/// lowercases the keys) equal what a write sent? Empty values count as
/// absent on both sides.
pub(super) fn user_metadata_equal(
    got: Option<&HashMap<String, String>>,
    wanted: &HashMap<String, String>,
) -> bool {
    let norm = |m: &HashMap<String, String>| -> std::collections::BTreeMap<String, String> {
        m.iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect()
    };
    !wanted.is_empty() && norm(got.unwrap_or(&HashMap::new())) == norm(wanted)
}

/// Pure: is `etag` (as S3 returns it) the MD5 of a single-PUT body? A
/// multipart or SSE-KMS ETag never is, so those stay lost fences.
pub(super) fn etag_is_body_md5(etag: &str, body_md5_hex: &str) -> bool {
    !body_md5_hex.is_empty() && etag.trim_matches('"').eq_ignore_ascii_case(body_md5_hex)
}

/// MD5 of a file, hex; `None` when it cannot be read.
pub(super) async fn md5_hex_of_file(path: &std::path::Path) -> Option<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use md5::Digest;
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let mut h = md5::Md5::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = f.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Some(hex::encode(h.finalize()))
    })
    .await
    .ok()
    .flatten()
}

/// Pure: the fence for a reference.bin that a HEAD found. A HEAD with no
/// ETag gave `ETag("")`, which every fenced write then skipped in silence;
/// now it is an explicit `Unfenced` with a warning, like a backend without
/// conditional writes, and the caller asks `has_reference` itself.
pub(super) fn fence_from_head_etag(etag: Option<&str>, bucket: &str, key: &str) -> RefFence {
    match etag {
        Some(e) if !e.is_empty() => RefFence::ETag(e.to_string()),
        _ => {
            warn!("HEAD {bucket}/{key} returned no ETag: reference writes are unfenced");
            RefFence::Unfenced
        }
    }
}

#[cfg(test)]
mod one_retry_layer_tests {
    //! One retry layer per error: the SDK retries throttles, 5xx and
    //! timeouts; the application loop retries only the Hetzner 400.

    use super::*;
    use crate::storage::DynStorageBackend;

    fn s3_config(endpoint: &str) -> BackendConfig {
        BackendConfig::S3 {
            endpoint: Some(endpoint.to_string()),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            allow_local: true,
            session_token: None,
        }
    }

    fn meta() -> FileMetadata {
        FileMetadata::new_passthrough("k".into(), "0".repeat(64), "0".repeat(32), 1, None)
    }

    fn puts(fake: &crate::storage::FakeS3, prefix: &str) -> usize {
        fake.requests()
            .iter()
            .filter(|r| r.starts_with(&format!("PUT {prefix}")))
            .count()
    }

    /// Prod: a backend that answers 503 SlowDown got up to 4 application
    /// attempts of 3 SDK attempts each: 12 PUTs, each sending the body
    /// again, to a backend that asked to slow down.
    #[tokio::test]
    async fn a_put_the_backend_throttles_is_sent_at_most_three_times() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let s3 = S3Backend::new(&s3_config(&endpoint), NativeEncryptionConfig::None)
            .await
            .unwrap();
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            std::sync::Arc::new(backend),
            &crate::config::Config::default(),
            None,
        );
        engine.create_bucket("b").await.unwrap();
        fake.fail("PUT", "b/", 503, "SlowDown", u32::MAX);
        fake.clear();
        let stored = engine
            .store("b", "photo.jpg", b"x", None, Default::default())
            .await;
        assert!(stored.is_err());
        let sent = puts(&fake, "/b/");
        assert!(sent <= 3, "{sent} PUTs for one store");
    }

    /// A 400 that carries a request id is the backend's answer to this
    /// request (EntityTooLarge, InvalidArgument): sending it again gives
    /// the same 400. It was sent 4 times.
    #[tokio::test]
    async fn a_deterministic_400_is_not_sent_again() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = hits.clone();
        let app = axum::Router::new().fallback(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    [("x-amz-request-id", "R1")],
                    "<Error><Code>EntityTooLarge</Code><Message>too large</Message></Error>",
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let s3 = super::super::test_support::for_test_endpoint(&endpoint);
        let meta = meta();
        let res = s3.put_object_with_metadata("b", "k", b"x", &meta).await;
        assert!(res.is_err());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The Hetzner 400 (no request id, `connection: close`, about 1 % of
    /// requests) clears on a retry: it is still retried.
    #[tokio::test]
    async fn a_400_without_a_request_id_is_retried() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let s3 = super::super::test_support::for_test_endpoint(&endpoint);
        fake.fail("PUT", "b/k", 400, "BadRequest", 1);
        let meta = meta();
        s3.put_object_with_metadata("b", "k", b"x", &meta)
            .await
            .expect("the retry succeeds");
        assert_eq!(puts(&fake, "/b/k"), 2);
    }

    /// Source guard: an application retry in the S3 backend retries only
    /// the Hetzner 400 ([`is_unidentified_400`]). Every other error has the
    /// SDK's retries only; a loop on top of them multiplied the requests
    /// to a backend that asked to slow down (4 x 3 PUTs per write). Every
    /// retry wait must follow an `is_unidentified_400` check (within 15 lines). The body
    /// resume (`body.rs`) re-reads a broken GET body and is no retry of a
    /// request.
    #[test]
    fn s3_backend_retries_only_the_unidentified_400() {
        let mut offenders = Vec::new();
        for (rel, text) in crate::source_scan::prod_sources("src/storage/s3") {
            if rel.ends_with("/body.rs") || rel.ends_with("/tests.rs") {
                continue;
            }
            let lines = crate::source_scan::prod_lines(&text);
            for (i, (n, line)) in lines.iter().enumerate() {
                if !line.contains("time::sleep(") {
                    continue;
                }
                let gated = lines[i.saturating_sub(15)..i]
                    .iter()
                    .any(|(_, l)| l.contains("is_unidentified_400("));
                if !gated {
                    offenders.push(format!("{rel}:{n}: {}", line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "an application retry on top of the SDK's retries:\n{}",
            offenders.join("\n")
        );
    }

    /// Each backend has its own SDK retry partition, shared by its two
    /// clients: the token bucket and the adaptive rate limiter were
    /// process-wide per region, so SlowDowns from one backend slowed every
    /// other backend in the same region.
    #[tokio::test]
    async fn each_backend_has_its_own_retry_partition() {
        let a = S3Backend::new(
            &s3_config("http://127.0.0.1:1"),
            NativeEncryptionConfig::None,
        )
        .await
        .unwrap();
        let b = S3Backend::new(
            &s3_config("http://127.0.0.2:1"),
            NativeEncryptionConfig::None,
        )
        .await
        .unwrap();
        let name = |c: &Client| c.config().retry_partition().map(|p| p.to_string());
        let pa = name(&a.client);
        assert!(
            pa.as_deref().is_some_and(|n| n.starts_with("dgp-backend-")),
            "{pa:?}"
        );
        assert_eq!(pa, name(&a.bulk_client), "one partition per backend");
        assert_ne!(pa, name(&b.client), "backends share a partition");
    }

    /// The upload client had no operation deadline: a tiny `.sha512` PUT
    /// to a stalled backend answered after 196 s (a 504 from Hetzner), and
    /// the CI upload failed. An upload now has the request deadline plus
    /// time for its body.
    #[tokio::test]
    async fn an_upload_has_a_deadline_sized_for_its_body() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.set_put_delay_ms(5_000);
        let config = s3_config(&endpoint);
        let mut s3 = S3Backend::new(&config, NativeEncryptionConfig::None)
            .await
            .unwrap();
        // The request deadline (`DGP_BACKEND_REQUEST_TIMEOUT_SECS`) at 1 s.
        s3.client = S3Backend::build_client_with(&config, Some(std::time::Duration::from_secs(1)))
            .await
            .unwrap();
        let meta = meta();
        let started = std::time::Instant::now();
        let res = s3.put_object_with_metadata("b", "k", b"x", &meta).await;
        assert!(matches!(res, Err(StorageError::Unavailable(_))), "{res:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "a 1-byte PUT ran {:?}",
            started.elapsed()
        );
    }
}
