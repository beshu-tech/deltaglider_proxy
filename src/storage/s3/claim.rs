// SPDX-License-Identifier: BUSL-1.1

//! The key claim of a bucket on S3 (`storage/encrypting/sentinel.rs`): one
//! small object, [`KEY_CLAIM_KEY`], written with `If-None-Match: *` /
//! `If-Match`, so two proxies that encrypt one bucket with different keys
//! cannot both claim it. A backend without conditional writes (B2, 501)
//! gets the write without the condition.

use super::errors::S3Op;
use super::S3Backend;
use crate::coordination::cas::{conditional_write_lost, is_not_implemented, sdk_error_signal};
use crate::storage::listing_facts::KEY_CLAIM_KEY;
use crate::storage::{ClaimOutcome, ClaimWrite, KeyClaim, StorageError};

/// A claim body longer than this is not a fingerprint label.
const MAX_CLAIM_LEN: usize = 64;

impl S3Backend {
    pub(super) async fn read_claim(&self, bucket: &str) -> Result<Option<KeyClaim>, StorageError> {
        let got = self
            .client
            .get_object()
            .bucket(bucket)
            .key(KEY_CLAIM_KEY)
            .send()
            .await;
        let out = match got {
            Ok(out) => out,
            Err(e) if crate::coordination::cas::is_object_absent(&sdk_error_signal(&e)) => {
                return Ok(None)
            }
            Err(e) => return Err(self.classify(bucket, &e, S3Op::GetObject)),
        };
        let version = out.e_tag().unwrap_or_default().to_string();
        let body = self.collect_body(bucket, KEY_CLAIM_KEY, out).await?;
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
        let put = |fingerprint: &str| {
            self.client
                .put_object()
                .bucket(bucket)
                .key(KEY_CLAIM_KEY)
                .content_type("text/plain")
                .body(aws_sdk_s3::primitives::ByteStream::from(
                    fingerprint.as_bytes().to_vec(),
                ))
        };
        let delete = || {
            self.client
                .delete_object()
                .bucket(bucket)
                .key(KEY_CLAIM_KEY)
        };
        let (conditional, op_name) = match op {
            ClaimWrite::Create(fp) => (
                put(fp).if_none_match("*").send().await.map(drop),
                S3Op::PutObject,
            ),
            ClaimWrite::Replace {
                fingerprint,
                version,
            } => (
                put(fingerprint).if_match(version).send().await.map(drop),
                S3Op::PutObject,
            ),
            ClaimWrite::Remove { version } => {
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
        let fingerprint = match op {
            ClaimWrite::Create(fp)
            | ClaimWrite::Replace {
                fingerprint: fp, ..
            } => fp,
            ClaimWrite::Remove { .. } => unreachable!("handled above"),
        };
        put(fingerprint)
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::PutObject))?;
        Ok(ClaimOutcome::Written)
    }
}
