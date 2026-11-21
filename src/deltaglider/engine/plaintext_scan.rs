// SPDX-License-Identifier: BUSL-1.1

//! The boot's scan of a bucket that encrypts: it reads the STORED view (every
//! stored form of every key, every baseline) and stops at the first sealed object.

use super::*;
use crate::storage::{ObjectVariant, StoredObject};
use futures::StreamExt;

/// HEADs in flight at once.
const SCAN_HEADS: usize = 8;

/// The baseline names that a stream of them (the filesystem) is held in at once.
pub(super) const BASELINE_BATCH: usize = 256;

/// How much of the bucket a scan reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanScope {
    /// Every object and every baseline: the only scan that may end in a
    /// conversion window.
    Whole,
    /// The first `n` objects of the listing: the boot warning of a bucket
    /// that the proxy already recorded as encrypting.
    Sample(u64),
}

/// What [`DeltaGliderEngine::plaintext_scan`](super::DeltaGliderEngine::plaintext_scan) found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PlaintextScan {
    /// The first scanned object that a client cannot read because it is
    /// not encrypted.
    pub first_refused: Option<String>,
    /// A stored form or a baseline is sealed (or carries a seal that does
    /// not open).
    pub encrypted: bool,
    /// Objects that the scan read.
    pub listed: u64,
    /// Listed names that the storage refused as client keys: no evidence.
    pub unreadable_names: u64,
    /// The scan read the whole listing and found nothing sealed. Never true
    /// after a sample, a cut or a sealed object.
    pub complete: bool,
    /// The most keys that the scan held at once.
    pub peak_held: u64,
    /// The most baseline names that the scan held at once (one page of them).
    pub peak_baselines: u64,
}

/// What one HEAD says about a stored object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    Sealed,
    Plaintext,
    /// Gone since the listing, a folder marker, or a name that is no client
    /// key: no evidence either way.
    Neither,
}

/// Pure: the evidence of one HEAD. A seal that does not open is sealed; a name the
/// storage refuses as a key is no evidence; any other error stops the scan.
pub(crate) fn seen_of(head: Result<&FileMetadata, &StorageError>) -> Result<Seen, String> {
    use crate::storage::encrypting::{seal, ENCRYPTION_MARKER_KEY};
    match head {
        Ok(meta) if meta.user_metadata.contains_key(ENCRYPTION_MARKER_KEY) => Ok(Seen::Sealed),
        Ok(_) => Ok(Seen::Neither),
        Err(e) if seal::is_plaintext_refusal(e) => Ok(Seen::Plaintext),
        Err(StorageError::KeyNotHeld { .. } | StorageError::Encryption(_)) => Ok(Seen::Sealed),
        Err(StorageError::NotFound(_) | StorageError::InvalidKey(_)) => Ok(Seen::Neither),
        Err(e) => Err(e.to_string()),
    }
}

/// Pure: the evidence of all forms of one key: sealed beats an error beats plaintext.
pub(crate) fn seen_of_forms(
    forms: &[Result<Option<FileMetadata>, StorageError>],
) -> Result<Seen, String> {
    let mut error = None;
    let mut seen = Seen::Neither;
    for form in forms {
        let one = match form {
            Ok(None) => Ok(Seen::Neither),
            Ok(Some(meta)) => seen_of(Ok(meta)),
            Err(e) => seen_of(Err(e)),
        };
        match one {
            Ok(Seen::Sealed) => return Ok(Seen::Sealed),
            Ok(Seen::Plaintext) => seen = Seen::Plaintext,
            Ok(Seen::Neither) => {}
            Err(e) => error = error.or(Some(e)),
        }
    }
    error.map_or(Ok(seen), Err)
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// The stored forms of `key` that the scan reads: the one that the page
    /// proved, else both.
    fn forms_to_read(proved: Option<ObjectVariant>) -> Vec<ObjectVariant> {
        proved.map_or_else(
            || vec![ObjectVariant::Delta, ObjectVariant::Passthrough],
            |one| vec![one],
        )
    }

    /// HEAD the baselines `prefixes`: is one of them sealed?
    async fn baselines_sealed(
        &self,
        bucket: &str,
        prefixes: Vec<String>,
    ) -> Result<bool, EngineError> {
        use crate::job_loop::{retry_transient, ListFailure};
        let mut heads = futures::stream::iter(prefixes)
            .map(|prefix| async move {
                retry_transient(
                    &format!("the scan of '{bucket}': the HEAD of the baseline '{prefix}'"),
                    || {
                        self.storage
                            .head_object(bucket, &prefix, StoredObject::Reference)
                    },
                    ListFailure::of_storage,
                )
                .await
            })
            .buffered(SCAN_HEADS);
        while let Some(head) = heads.next().await {
            let seen = seen_of(head.as_ref()).map_err(|e| {
                EngineError::Storage(StorageError::Other(format!("the scan of '{bucket}': {e}")))
            })?;
            if seen == Seen::Sealed {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The scan of `bucket`: pages with a HEAD per stored form and the HEADs of the
    /// baselines that each page reports.
    /// An error or a cut listing is an `Err` or an incomplete scan: no window.
    pub async fn plaintext_scan(
        &self,
        bucket: &str,
        scope: ScanScope,
    ) -> Result<PlaintextScan, EngineError> {
        self.plaintext_scan_paged(bucket, scope, crate::job_loop::Pager::fresh())
            .await
    }

    /// [`Self::plaintext_scan`] with the pager of the listing (tests cut its page budget).
    pub(crate) async fn plaintext_scan_paged(
        &self,
        bucket: &str,
        scope: ScanScope,
        mut pager: crate::job_loop::Pager,
    ) -> Result<PlaintextScan, EngineError> {
        use crate::job_loop::{retry_transient, ListFailure};
        let scan_err = |e: String| {
            EngineError::Storage(StorageError::Other(format!("the scan of '{bucket}': {e}")))
        };
        let limit = match scope {
            ScanScope::Whole => u64::MAX,
            ScanScope::Sample(n) => n,
        };
        let mut out = PlaintextScan::default();
        let mut finished = false;
        // A page that reports no baselines (the filesystem): they are listed after the pass.
        let mut unreported = false;
        // The baselines of the page before: a page reports those that sit at
        // its edge too. One page of names at most.
        let mut previous_page: std::collections::HashSet<String> = Default::default();
        let mut next_report = PROGRESS_EVERY;
        while let Some(_page_no) = pager.begin_page() {
            let token = pager.token().map(str::to_string);
            let page = retry_transient(
                &format!("the scan of '{bucket}': a page listing"),
                || {
                    self.list_job_page(
                        bucket,
                        crate::maintenance::worker::PAGE_SIZE,
                        token.as_deref(),
                    )
                },
                ListFailure::of,
            )
            .await?;
            if page.reference_prefixes.is_none() {
                unreported = true;
            }
            let keys: Vec<(String, Vec<ObjectVariant>)> = page
                .page
                .objects
                .iter()
                .map(|(k, _)| k)
                .filter(|k| !k.ends_with('/') && !crate::coordination::cas_probe::is_probe_key(k))
                .take(usize::try_from(limit.saturating_sub(out.listed)).unwrap_or(usize::MAX))
                .map(|k| (k.clone(), Self::forms_to_read(page.forms.of(k))))
                .collect();
            out.peak_held = out.peak_held.max(keys.len() as u64);
            let mut checks = futures::stream::iter(keys)
                .map(|(key, forms)| async move {
                    let at = ObjectKey::parse(bucket, &key);
                    let mut reads = Vec::new();
                    for form in forms {
                        reads.push(
                            retry_transient(
                                &format!("the scan of '{bucket}': the HEAD of '{key}'"),
                                || {
                                    self.stored_form_metadata(
                                        bucket,
                                        &at.prefix,
                                        &at.filename,
                                        form,
                                    )
                                },
                                ListFailure::of_storage,
                            )
                            .await,
                        );
                    }
                    (key, reads)
                })
                .buffered(SCAN_HEADS);
            while let Some((key, reads)) = checks.next().await {
                out.listed += 1;
                if report_due(out.listed, &mut next_report) {
                    tracing::info!(
                        "boot scan of bucket '{bucket}': read {} objects so far",
                        out.listed
                    );
                }
                out.unreadable_names += reads
                    .iter()
                    .filter(|r| matches!(r, Err(StorageError::InvalidKey(_))))
                    .count() as u64;
                match seen_of_forms(&reads).map_err(scan_err)? {
                    Seen::Sealed => {
                        out.encrypted = true;
                        return Ok(out);
                    }
                    Seen::Plaintext => {
                        out.first_refused.get_or_insert(key);
                    }
                    Seen::Neither => {}
                }
            }
            drop(checks);
            let more = pager.advance(
                page.page.is_truncated,
                page.page.next_continuation_token.clone(),
            );
            // The baselines of this page, read now and kept nowhere. A page
            // that ended the pass because its budget ran out reads none: a
            // cut pass opens no window.
            let cut = pager.truncated_by_page_budget();
            if scope == ScanScope::Whole && !cut {
                if let Some(found) = &page.reference_prefixes {
                    out.peak_baselines = out.peak_baselines.max(found.len() as u64);
                    let new: Vec<String> = found
                        .iter()
                        .filter(|name| !previous_page.contains(*name))
                        .cloned()
                        .collect();
                    if self.baselines_sealed(bucket, new).await? {
                        out.encrypted = true;
                        return Ok(out);
                    }
                    previous_page = found.iter().cloned().collect();
                }
            }
            if !more {
                finished = true;
                break;
            }
            if out.listed >= limit {
                break;
            }
        }
        // A backend whose pages report none (the filesystem) streams them once, in
        // batches, after the objects: a cut scan reads no baseline.
        if scope == ScanScope::Whole && finished && unreported {
            let mut names = self
                .storage
                .list_reference_prefix_stream(bucket, "")
                .await?;
            let mut batch: Vec<String> = Vec::with_capacity(BASELINE_BATCH);
            loop {
                let next = names.next().await.transpose()?;
                let done = next.is_none();
                batch.extend(next);
                if batch.len() >= BASELINE_BATCH || (done && !batch.is_empty()) {
                    out.peak_baselines = out.peak_baselines.max(batch.len() as u64);
                    if self
                        .baselines_sealed(bucket, std::mem::take(&mut batch))
                        .await?
                    {
                        out.encrypted = true;
                        return Ok(out);
                    }
                }
                if done {
                    break;
                }
            }
        }
        out.complete = finished && scope == ScanScope::Whole && !pager.truncated_by_page_budget();
        Ok(out)
    }
}

/// Objects between two progress lines of a long scan.
const PROGRESS_EVERY: u64 = 100_000;

/// Pure: `true` once per [`PROGRESS_EVERY`] objects; moves `next` on.
fn report_due(listed: u64, next: &mut u64) -> bool {
    let due = listed >= *next;
    if due {
        *next += PROGRESS_EVERY;
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal() -> StorageError {
        StorageError::Encryption(format!(
            "object 'x' in bucket 'b' {}",
            seal::PLAINTEXT_REFUSAL
        ))
    }
    use crate::storage::encrypting::seal;

    #[test]
    fn a_long_scan_reports_every_hundred_thousand_objects() {
        let mut next = PROGRESS_EVERY;
        let due: Vec<u64> = (1..=250_000)
            .filter(|n| report_due(*n, &mut next))
            .collect();
        assert_eq!(due, [100_000, 200_000]);
    }

    #[test]
    fn what_one_head_says() {
        let marked = {
            let mut m = FileMetadata::fallback(
                "x".into(),
                1,
                "e".into(),
                chrono::Utc::now(),
                None,
                crate::types::StorageInfo::Passthrough,
            );
            m.user_metadata.insert(
                crate::storage::encrypting::ENCRYPTION_MARKER_KEY.into(),
                "v2".into(),
            );
            m
        };
        assert_eq!(seen_of(Ok(&marked)), Ok(Seen::Sealed));
        assert_eq!(seen_of(Err(&refusal())), Ok(Seen::Plaintext));
        // A seal of another key, or an unknown marker, is sealed evidence.
        let foreign = StorageError::KeyNotHeld {
            key: "fp-1".into(),
            write: false,
            detail: String::new(),
        };
        assert_eq!(seen_of(Err(&foreign)), Ok(Seen::Sealed));
        let unknown = StorageError::Encryption("carries an encryption marker".into());
        assert_eq!(seen_of(Err(&unknown)), Ok(Seen::Sealed));
        assert_eq!(
            seen_of(Err(&StorageError::NotFound("x".into()))),
            Ok(Seen::Neither)
        );
        assert!(seen_of(Err(&StorageError::S3("boom".into()))).is_err());
        // A name that is no client key says nothing; an I/O or backend error stops the scan.
        assert_eq!(
            seen_of(Err(&StorageError::InvalidKey("a\\b".into()))),
            Ok(Seen::Neither)
        );
        for e in [
            StorageError::Io(std::io::Error::other("eio")),
            StorageError::Throttled("slow".into()),
            StorageError::AccessDenied("no".into()),
        ] {
            assert!(seen_of(Err(&e)).is_err(), "{e}");
        }
    }

    fn sealed_meta() -> FileMetadata {
        let mut m = FileMetadata::fallback(
            "x".into(),
            1,
            "e".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        m.user_metadata.insert(
            crate::storage::encrypting::ENCRYPTION_MARKER_KEY.into(),
            "v2".into(),
        );
        m
    }

    #[test]
    fn one_sealed_form_beats_a_plaintext_twin() {
        let twin = Err(refusal());
        assert_eq!(
            seen_of_forms(&[Ok(Some(sealed_meta())), twin]),
            Ok(Seen::Sealed)
        );
        assert_eq!(
            seen_of_forms(&[Err(refusal()), Ok(Some(sealed_meta()))]),
            Ok(Seen::Sealed)
        );
        // A seal that does not open is sealed too.
        let foreign = StorageError::Encryption("carries an encryption marker".into());
        assert_eq!(
            seen_of_forms(&[Err(refusal()), Err(foreign)]),
            Ok(Seen::Sealed)
        );
        // A form that cannot be read may be the sealed one: the scan fails.
        let eio = || StorageError::Io(std::io::Error::other("eio"));
        assert!(seen_of_forms(&[Err(refusal()), Err(eio())]).is_err());
        assert_eq!(
            seen_of_forms(&[Err(eio()), Ok(Some(sealed_meta()))]),
            Ok(Seen::Sealed)
        );
        assert_eq!(
            seen_of_forms(&[Err(refusal()), Ok(None)]),
            Ok(Seen::Plaintext)
        );
        assert_eq!(seen_of_forms(&[Ok(None), Ok(None)]), Ok(Seen::Neither));
    }
}
