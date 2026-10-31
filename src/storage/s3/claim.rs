// SPDX-License-Identifier: BUSL-1.1

//! The bucket-level objects that this proxy writes with conditions, on S3:
//! the key claim ([`KEY_CLAIM_KEY`], `storage/encrypting/sentinel.rs`) and
//! the conversion marker ([`CONVERSION_MARKER_KEY`],
//! `storage/encrypting/conversion.rs`). Each is one small object, written
//! with `If-None-Match: *` / `If-Match`, so two proxies cannot both create
//! or both replace it. A backend without conditional writes (B2, 501) gets
//! the write without the condition.

use super::errors::S3Op;
use super::S3Backend;
use crate::coordination::cas::{conditional_write_lost, is_not_implemented, sdk_error_signal};
use crate::coordination::server_clock::ServerDate;
use crate::storage::listing_facts::{CONVERSION_MARKER_KEY, KEY_CLAIM_KEY};
use crate::storage::{
    ClaimOutcome, ClaimWrite, ConversionMarker, KeyClaim, MarkerRead, MarkerWrite, StorageError,
};

/// A claim body longer than this is not a fingerprint label.
const MAX_CLAIM_LEN: usize = 64;

/// A bucket-level object longer than this is read as an empty body (which
/// no reader accepts): a planted large object must not cost the memory of
/// its size.
const MAX_SMALL_OBJECT: i64 = 4096;

/// One read of a small bucket-level object.
struct SmallRead {
    /// The body and the ETag; `None` when the object is absent.
    found: Option<(Vec<u8>, String)>,
    /// The `Date` of the response (unix seconds).
    server_now: Option<i64>,
}

impl S3Backend {
    /// GET the small object `key` of `bucket`, with the server's clock.
    async fn read_small(&self, bucket: &str, key: &str) -> Result<SmallRead, StorageError> {
        let date = ServerDate::default();
        let got = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .customize()
            .interceptor(date.clone())
            .send()
            .await;
        let out = match got {
            Ok(out) => out,
            Err(e) if crate::coordination::cas::is_object_absent(&sdk_error_signal(&e)) => {
                return Ok(SmallRead {
                    found: None,
                    server_now: date.get(),
                })
            }
            Err(e) => return Err(self.classify(bucket, &e, S3Op::GetObject)),
        };
        let version = out.e_tag().unwrap_or_default().to_string();
        let declared_large = out.content_length().is_some_and(|n| n > MAX_SMALL_OBJECT);
        let mut body = if declared_large {
            Vec::new()
        } else {
            self.collect_body(bucket, key, out).await?
        };
        if body.len() as i64 > MAX_SMALL_OBJECT {
            body.clear();
        }
        Ok(SmallRead {
            found: Some((body, version)),
            server_now: date.get(),
        })
    }

    /// Write the small object `key` of `bucket` with the condition of `op`.
    async fn write_small(
        &self,
        bucket: &str,
        key: &str,
        op: MarkerWrite<'_>,
    ) -> Result<ClaimOutcome, StorageError> {
        // The bucket's native SSE, as every other write: a bucket policy
        // that requires it refuses the write without it.
        let native = self.native_encryption.for_bucket(bucket);
        let put = |body: &[u8]| {
            let request = self
                .client
                .put_object()
                .bucket(bucket)
                .key(key)
                .content_type("text/plain")
                .body(aws_sdk_s3::primitives::ByteStream::from(body.to_vec()));
            super::metadata::apply_native_encryption(request, &native)
        };
        let delete = || self.client.delete_object().bucket(bucket).key(key);
        let (conditional, op_name) = match op {
            MarkerWrite::Create(body) => (
                put(body).if_none_match("*").send().await.map(drop),
                S3Op::PutObject,
            ),
            MarkerWrite::Replace { body, version } => (
                put(body).if_match(version).send().await.map(drop),
                S3Op::PutObject,
            ),
            MarkerWrite::Remove { version } => {
                match delete().if_match(version).send().await {
                    Ok(_) => return Ok(ClaimOutcome::Written),
                    Err(e) => {
                        let signal = sdk_error_signal(&e);
                        if conditional_write_lost(&signal) {
                            return Ok(ClaimOutcome::Lost);
                        }
                        if !is_not_implemented(&signal) {
                            return Err(self.classify(bucket, &e, S3Op::DeleteObject));
                        }
                    }
                }
                delete()
                    .send()
                    .await
                    .map_err(|e| self.classify(bucket, &e, S3Op::DeleteObject))?;
                return Ok(ClaimOutcome::Written);
            }
        };
        let Err(e) = conditional else {
            return Ok(ClaimOutcome::Written);
        };
        let signal = sdk_error_signal(&e);
        if conditional_write_lost(&signal) {
            return Ok(ClaimOutcome::Lost);
        }
        if !is_not_implemented(&signal) {
            return Err(self.classify(bucket, &e, op_name));
        }
        let body = match op {
            MarkerWrite::Create(body) | MarkerWrite::Replace { body, .. } => body,
            MarkerWrite::Remove { .. } => unreachable!("handled above"),
        };
        put(body)
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::PutObject))?;
        Ok(ClaimOutcome::Written)
    }

    pub(super) async fn read_claim(&self, bucket: &str) -> Result<Option<KeyClaim>, StorageError> {
        let Some((body, version)) = self.read_small(bucket, KEY_CLAIM_KEY).await?.found else {
            return Ok(None);
        };
        let text = String::from_utf8_lossy(&body);
        let text = crate::security::str_prefix(&text, MAX_CLAIM_LEN);
        Ok(Some(KeyClaim {
            fingerprint: text.trim().to_string(),
            version,
        }))
    }

    pub(super) async fn write_claim(
        &self,
        bucket: &str,
        op: ClaimWrite<'_>,
    ) -> Result<ClaimOutcome, StorageError> {
        let op = match op {
            ClaimWrite::Create(fp) => MarkerWrite::Create(fp.as_bytes()),
            ClaimWrite::Replace {
                fingerprint,
                version,
            } => MarkerWrite::Replace {
                body: fingerprint.as_bytes(),
                version,
            },
            ClaimWrite::Remove { version } => MarkerWrite::Remove { version },
        };
        self.write_small(bucket, KEY_CLAIM_KEY, op).await
    }

    pub(super) async fn read_marker(&self, bucket: &str) -> Result<MarkerRead, StorageError> {
        let read = self.read_small(bucket, CONVERSION_MARKER_KEY).await?;
        Ok(MarkerRead {
            marker: read.found.map(|(body, version)| ConversionMarker {
                content: body,
                version,
            }),
            server_now: read.server_now,
        })
    }

    pub(super) async fn write_marker(
        &self,
        bucket: &str,
        op: MarkerWrite<'_>,
    ) -> Result<ClaimOutcome, StorageError> {
        self.write_small(bucket, CONVERSION_MARKER_KEY, op).await
    }
}
