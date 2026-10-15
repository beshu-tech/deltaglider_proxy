// SPDX-License-Identifier: BUSL-1.1
//! Old versions of a versioned backend bucket. The proxy does not expose
//! versions, so its clients cannot delete them; without this, a versioned
//! bucket (Backblaze B2 always keeps them) that lists empty could never be
//! deleted through the proxy.

use super::*;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use tracing::info;

/// Pages of `ListObjectVersions` read per pass. A bucket with more old
/// versions keeps refusing DeleteBucket (409) after this request.
const MAX_VERSION_PAGES: usize = 100;

impl S3Backend {
    /// Delete the noncurrent versions, then the delete markers, of a bucket
    /// that has no current object. Returns `false` (nothing deleted) when a
    /// current object exists. A version that becomes current during the
    /// purge (a concurrent PUT) is never deleted: DeleteBucket then fails.
    pub(super) async fn purge_old_versions(&self, bucket: &str) -> Result<bool, StorageError> {
        let listed = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .max_keys(1)
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
        if !listed.contents().is_empty() {
            return Ok(false);
        }
        // Versions first: deleting a delete marker before the version under
        // it would bring that version back for a moment.
        let versions = self.purge_version_pass(bucket, false).await?;
        let markers = self.purge_version_pass(bucket, true).await?;
        info!(
            "DeleteBucket {bucket}: removed {versions} old versions and {markers} delete markers"
        );
        Ok(true)
    }

    /// One pass over `ListObjectVersions`: delete the delete markers
    /// (`markers`) or the noncurrent versions. Returns the count deleted.
    async fn purge_version_pass(&self, bucket: &str, markers: bool) -> Result<usize, StorageError> {
        let mut deleted = 0;
        let mut key_marker: Option<String> = None;
        let mut version_marker: Option<String> = None;
        for _ in 0..MAX_VERSION_PAGES {
            let page = self
                .client
                .list_object_versions()
                .bucket(bucket)
                .set_key_marker(key_marker.take())
                .set_version_id_marker(version_marker.take())
                .send()
                .await
                .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
            let ids: Vec<ObjectIdentifier> = if markers {
                page.delete_markers()
                    .iter()
                    .filter_map(|m| identifier(m.key(), m.version_id()))
                    .collect()
            } else {
                page.versions()
                    .iter()
                    .filter(|v| v.is_latest() == Some(false))
                    .filter_map(|v| identifier(v.key(), v.version_id()))
                    .collect()
            };
            deleted += self.delete_versions(bucket, ids).await?;
            if page.is_truncated() != Some(true) {
                break;
            }
            key_marker = page.next_key_marker().map(String::from);
            version_marker = page.next_version_id_marker().map(String::from);
        }
        Ok(deleted)
    }

    async fn delete_versions(
        &self,
        bucket: &str,
        ids: Vec<ObjectIdentifier>,
    ) -> Result<usize, StorageError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let count = ids.len();
        let delete = Delete::builder()
            .set_objects(Some(ids))
            .quiet(true)
            .build()
            .map_err(|e| StorageError::Other(format!("DeleteObjects request: {e}")))?;
        let resp = self
            .client
            .delete_objects()
            .bucket(bucket)
            .delete(delete)
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::Other("delete_objects")))?;
        // A version left behind makes DeleteBucket fail, as it must.
        Ok(count - resp.errors().len())
    }
}

fn identifier(key: Option<&str>, version_id: Option<&str>) -> Option<ObjectIdentifier> {
    ObjectIdentifier::builder()
        .key(key?)
        .version_id(version_id?)
        .build()
        .ok()
}
