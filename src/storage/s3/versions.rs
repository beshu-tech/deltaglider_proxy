// SPDX-License-Identifier: BUSL-1.1
//! Old versions of a versioned backend bucket. The proxy does not expose
//! versions, so its clients cannot delete them; without this, a versioned
//! bucket (Backblaze B2 always keeps them) that lists empty could never be
//! deleted through the proxy.

use super::*;
use crate::storage::VersionPurgePage;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use futures::{stream, StreamExt, TryStreamExt};
use std::collections::{BTreeSet, HashMap};
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

/// Requests that the purge of plaintext versions sends at once.
const PURGE_PARALLEL: usize = 8;

impl S3Backend {
    /// One page of [`StorageBackend::purge_plaintext_versions`]: list one
    /// page of versions, HEAD each noncurrent candidate (by version id) and
    /// the current object of each stored form of its key, and delete the
    /// versions that [`purgeable`] allows in one `DeleteObjects` request.
    pub(super) async fn purge_plaintext_page(
        &self,
        bucket: &str,
        cursor: Option<&str>,
    ) -> Result<VersionPurgePage, StorageError> {
        let (key_marker, version_marker) = match cursor {
            Some(c) => decode_cursor(c)?,
            None => (None, None),
        };
        let page = self
            .client
            .list_object_versions()
            .bucket(bucket)
            .set_key_marker(key_marker)
            .set_version_id_marker(version_marker)
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
        let next = (page.is_truncated() == Some(true))
            .then(|| encode_cursor(page.next_key_marker(), page.next_version_id_marker()));
        let candidates = noncurrent_candidates(
            page.versions()
                .iter()
                .map(|v| (v.key(), v.version_id(), v.is_latest())),
        );
        if candidates.is_empty() {
            return Ok(VersionPurgePage { deleted: 0, next });
        }

        // The current object of every stored form of the candidates' keys,
        // one HEAD each: `None` absent, `Some(sealed)`.
        let forms: BTreeSet<String> = candidates
            .iter()
            .flat_map(|(k, _)| stored_forms(k))
            .collect();
        let current: HashMap<String, Option<bool>> = stream::iter(forms)
            .map(|key| async move {
                let state = self.sealed_state(bucket, &key, None).await;
                state.map(|s| (key, s))
            })
            .buffer_unordered(PURGE_PARALLEL)
            .try_collect()
            .await?;
        let judged: Vec<(String, String, Option<bool>)> = stream::iter(candidates)
            .map(|(key, version)| async move {
                let state = self.sealed_state(bucket, &key, Some(&version)).await;
                state.map(|s| (key, version, s))
            })
            .buffer_unordered(PURGE_PARALLEL)
            .try_collect()
            .await?;

        let ids: Vec<ObjectIdentifier> = judged
            .iter()
            .filter(|(key, _, version_sealed)| {
                let currents: Vec<Option<bool>> = stored_forms(key)
                    .iter()
                    .map(|f| current.get(f).copied().flatten())
                    .collect();
                purgeable(key, *version_sealed, &currents)
            })
            .filter_map(|(key, version, _)| identifier(Some(key), Some(version)))
            .collect();
        let deleted = self.delete_versions(bucket, ids).await? as u64;
        if deleted > 0 {
            info!("bucket {bucket}: deleted {deleted} noncurrent plaintext versions");
        }
        Ok(VersionPurgePage { deleted, next })
    }

    /// Does the object at `key` (its version `version`, else the current
    /// one) carry the encryption marker? `None`: there is none (404).
    async fn sealed_state(
        &self,
        bucket: &str,
        key: &str,
        version: Option<&str>,
    ) -> Result<Option<bool>, StorageError> {
        BACKEND_HEAD_REQUESTS.inc();
        match self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .set_version_id(version.map(String::from))
            .send()
            .await
        {
            // The marker as the encrypting wrapper reads it: the user
            // metadata of the backend's own parse. Metadata that does not
            // parse but names a marker is an error there: sealed here (kept).
            Ok(head) => Ok(Some(
                self.metadata_of_response(
                    bucket,
                    key,
                    &ObjectHeaders {
                        user: head.metadata(),
                        last_modified: head.last_modified(),
                        e_tag: head.e_tag(),
                        content_length: head.content_length(),
                        content_type: head.content_type(),
                    },
                )
                .map_or(true, |meta| {
                    meta.user_metadata
                        .contains_key(crate::storage::encrypting::ENCRYPTION_MARKER_KEY)
                }),
            )),
            Err(e) => match self.classify(bucket, &e, S3Op::HeadObject) {
                StorageError::NotFound(_) => Ok(None),
                other => Err(other),
            },
        }
    }
}

/// Pure: the noncurrent versions of a listing page that the purge looks
/// at: `(key, version id, is latest)` with `is latest == false`, outside
/// the `.dg/facts/` namespace.
fn noncurrent_candidates<'a>(
    versions: impl Iterator<Item = (Option<&'a str>, Option<&'a str>, Option<bool>)>,
) -> Vec<(String, String)> {
    versions
        .filter(|(_, _, latest)| *latest == Some(false))
        .filter_map(|(k, v, _)| Some((k?.to_string(), v?.to_string())))
        .filter(|(k, _)| !crate::storage::listing_facts::is_facts_key(k))
        .collect()
}

/// Pure: the stored keys under which the object of `key` can live: a
/// reference is one; an object is itself and its delta (`x` and
/// `x.delta`), because a rewrite can change the form.
fn stored_forms(key: &str) -> Vec<String> {
    if S3Backend::deltaspace_of_reference_key(key).is_some() {
        return vec![key.to_string()];
    }
    match key.strip_suffix(".delta") {
        Some(object) => vec![object.to_string(), key.to_string()],
        None => vec![key.to_string(), format!("{key}.delta")],
    }
}

/// Pure: may the purge delete a noncurrent version of `key`? Only when the
/// version carries no encryption marker (`Some(false)`: plaintext; `None`:
/// gone already), and every current stored form of the key carries the
/// marker (`currents`: one entry per form, `None` absent, `Some(sealed)`).
/// An object must also exist now in one form: the history of an object
/// that a client deleted stays. A reference is internal (no client reads
/// its history), so the history of a reference that the proxy removed
/// goes too.
fn purgeable(key: &str, version_sealed: Option<bool>, currents: &[Option<bool>]) -> bool {
    let reference = S3Backend::deltaspace_of_reference_key(key).is_some();
    version_sealed == Some(false)
        && (reference || currents.iter().any(Option::is_some))
        && currents.iter().flatten().all(|sealed| *sealed)
}

/// The cursor of the next page: the key and version-id markers.
fn encode_cursor(key: Option<&str>, version: Option<&str>) -> String {
    serde_json::json!({ "k": key, "v": version }).to_string()
}

fn decode_cursor(c: &str) -> Result<(Option<String>, Option<String>), StorageError> {
    #[derive(serde::Deserialize)]
    struct Cursor {
        k: Option<String>,
        v: Option<String>,
    }
    let c: Cursor = serde_json::from_str(c)
        .map_err(|e| StorageError::Other(format!("invalid version cursor: {e}")))?;
    Ok((c.k, c.v))
}

#[cfg(test)]
mod purge_tests {
    use super::*;

    #[test]
    fn only_noncurrent_versions_outside_the_facts_are_candidates() {
        let page = [
            (Some("a.txt"), Some("v2"), Some(true)),
            (Some("a.txt"), Some("v1"), Some(false)),
            (Some(".dg/facts/!!conversion"), Some("m1"), Some(false)),
            (Some("b.txt"), None, Some(false)),
            (Some("c.txt"), Some("c1"), None),
        ];
        assert_eq!(
            noncurrent_candidates(page.into_iter()),
            vec![("a.txt".to_string(), "v1".to_string())]
        );
    }

    #[test]
    fn an_object_has_two_stored_forms_and_a_reference_one() {
        assert_eq!(stored_forms("p/a.zip"), ["p/a.zip", "p/a.zip.delta"]);
        assert_eq!(stored_forms("p/a.zip.delta"), ["p/a.zip", "p/a.zip.delta"]);
        assert_eq!(stored_forms("p/reference.bin"), ["p/reference.bin"]);
        assert_eq!(stored_forms("reference.bin"), ["reference.bin"]);
    }

    #[test]
    fn only_plaintext_history_of_an_encrypted_current_object_goes() {
        let sealed = Some(true);
        let plain = Some(false);
        // (key, version, current forms, purge)
        type Case<'a> = (&'a str, Option<bool>, &'a [Option<bool>], bool);
        let cases: &[Case] = &[
            ("a.zip", plain, &[sealed, None], true),
            ("a.zip.delta", plain, &[None, sealed], true),
            ("a.zip", sealed, &[sealed, None], false),
            ("a.zip", None, &[sealed, None], false),
            ("a.zip", plain, &[plain, None], false),
            ("a.zip", plain, &[sealed, plain], false),
            // A deleted object keeps its history.
            ("a.zip", plain, &[None, None], false),
            ("a.zip", plain, &[], false),
            // A reference: its plaintext history goes, also once removed.
            ("p/reference.bin", plain, &[sealed], true),
            ("p/reference.bin", plain, &[None], true),
            ("p/reference.bin", plain, &[plain], false),
            ("p/reference.bin", sealed, &[None], false),
        ];
        for (key, version, currents, want) in cases {
            assert_eq!(
                purgeable(key, *version, currents),
                *want,
                "{key} {version:?} {currents:?}"
            );
        }
    }

    #[test]
    fn the_cursor_round_trips() {
        let c = encode_cursor(Some("a\"b"), Some("v1"));
        assert_eq!(
            decode_cursor(&c).unwrap(),
            (Some("a\"b".to_string()), Some("v1".to_string()))
        );
        assert_eq!(
            decode_cursor(&encode_cursor(None, None)).unwrap(),
            (None, None)
        );
        assert!(decode_cursor("x").is_err());
    }
}
