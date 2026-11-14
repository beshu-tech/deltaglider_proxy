// SPDX-License-Identifier: BUSL-1.1

//! The conditional-write probe for a backend or bucket.
//!
//! Leases and reference locks use BOTH conditions: `If-None-Match:*` to
//! create, `If-Match:<etag>` to steal, renew and release. A backend that
//! honours only the first passes a create-only probe and then lets two nodes
//! renew or steal the same lock. So the probe tests both, on a caller-owned
//! key, and the verdict is a pure function of the four step outcomes.
//!
//! A fifth step tests `If-Match` on DELETE (a wrong ETag must answer 412).
//! It does not change the CAS verdict: a lock or lease release on a bucket
//! that ignores it reads the object first and deletes only its own
//! ([`ProbeResult::conditional_delete`]).
//!
//! The `If-Match` dialect: S3 returns ETags quoted, and AWS and MinIO
//! compare `If-Match` with that quoted form. Ceph RGW (Hetzner Object
//! Storage) compares the `If-Match` of a PUT with the bare ETag, so the
//! correct ETag, quoted, gets 412. When the quoted right-ETag step gets 412,
//! the probe tries the unquoted form with a wrong and with the right ETag;
//! both enforced → CAS in the unquoted dialect
//! ([`super::if_match::IfMatchDialect`]). The DELETE step then uses that
//! dialect too.
//!
//! A backend that drops the connection on a conditional PUT (Backblaze B2,
//! eu-central-003, 2026-10-31) gives no answer at all. When an
//! unconditional PUT of the same key goes through right after, the drop is
//! the backend's answer to the condition: the probe reads it as a refusal
//! (definitive non-CAS), not as an unreachable backend.

use super::if_match::{IfMatchDialect, IfMatchRequest};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;

/// The ETag of the probe's wrong-ETag steps.
const WRONG_ETAG: &str = "00000000000000000000000000000000";

/// How one conditional step answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The write went through.
    Written,
    /// 412: the condition was enforced.
    PreconditionFailed,
    /// 501 / NotImplemented: the condition was rejected (Backblaze B2).
    NotImplemented,
    /// No HTTP answer: the connection dropped, reset or timed out. See
    /// [`resolve_dropped`].
    Dropped(String),
    /// Anything else: the probe cannot tell.
    Other(String),
}

/// Pure: what a [`StepOutcome::Dropped`] conditional step means, given
/// whether an unconditional PUT of the same key went through right after
/// (`control_written`). The backend answers plain writes but drops the
/// conditional one: it refuses the condition ([`StepOutcome::NotImplemented`]).
/// Otherwise the backend cannot be reached and the step is indeterminate.
/// Other outcomes pass through.
pub fn resolve_dropped(step: StepOutcome, control_written: bool) -> StepOutcome {
    match step {
        StepOutcome::Dropped(_) if control_written => StepOutcome::NotImplemented,
        StepOutcome::Dropped(e) => StepOutcome::Other(format!("connection dropped: {e}")),
        other => other,
    }
}

/// Verdict from the three conditional steps (after an unconditional PUT
/// that returned the key's etag). `Ok(true)` = CAS enforced; `Ok(false)` =
/// DEFINITIVELY not enforced; `Err` = indeterminate (never treat as either).
pub fn classify_cas_probe(
    create_on_existing: &StepOutcome,
    if_match_wrong_etag: &StepOutcome,
    if_match_right_etag: &StepOutcome,
) -> Result<bool, String> {
    use StepOutcome::*;
    for (step, outcome) in [
        ("If-None-Match:* on an existing key", create_on_existing),
        ("If-Match with a wrong etag", if_match_wrong_etag),
    ] {
        match outcome {
            PreconditionFailed => {}
            Written | NotImplemented => return Ok(false),
            Dropped(e) | Other(e) => return Err(format!("{step}: {e}")),
        }
    }
    match if_match_right_etag {
        Written => Ok(true),
        // Refusing the CORRECT etag breaks renew and release just as surely.
        PreconditionFailed | NotImplemented => Ok(false),
        Dropped(e) | Other(e) => Err(format!("If-Match with the current etag: {e}")),
    }
}

/// The two `If-Match` steps in the unquoted form, run only when the quoted
/// form refused the CORRECT ETag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnquotedSteps {
    /// `If-Match: <wrong etag>`, unquoted: must be 412.
    pub wrong: StepOutcome,
    /// `If-Match: <current etag>`, unquoted: must be written.
    pub right: StepOutcome,
}

/// Pure: the CAS verdict and the `If-Match` dialect. The quoted form first
/// ([`classify_cas_probe`]). A backend that refused the correct ETag quoted
/// (412) is CAS in the unquoted dialect when `If-None-Match` and the quoted
/// wrong ETag were refused, and the unquoted form refuses a wrong ETag AND
/// writes with the correct one. Otherwise it is non-CAS. `Err` =
/// indeterminate.
pub fn classify_cas_dialect(
    create_on_existing: &StepOutcome,
    if_match_wrong_etag: &StepOutcome,
    if_match_right_etag: &StepOutcome,
    unquoted: Option<&UnquotedSteps>,
) -> Result<(bool, IfMatchDialect), String> {
    use StepOutcome::*;
    let (PreconditionFailed, Some(u)) = (if_match_right_etag, unquoted) else {
        return classify_cas_probe(create_on_existing, if_match_wrong_etag, if_match_right_etag)
            .map(|cas| (cas, IfMatchDialect::Quoted));
    };
    if !classify_cas_probe(create_on_existing, if_match_wrong_etag, &u.right)? {
        return Ok((false, IfMatchDialect::Quoted));
    }
    match &u.wrong {
        PreconditionFailed => Ok((true, IfMatchDialect::Unquoted)),
        Written | NotImplemented => Ok((false, IfMatchDialect::Quoted)),
        Dropped(e) | Other(e) => Err(format!("unquoted If-Match with a wrong etag: {e}")),
    }
}

/// Pure: did the backend enforce `If-Match` on DELETE? Only a real 412
/// counts. A DELETE that went through, a 501 or an unclear answer all mean
/// "not proven", and the release then reads before it deletes.
pub fn classify_conditional_delete(delete_wrong_etag: &StepOutcome) -> bool {
    matches!(delete_wrong_etag, StepOutcome::PreconditionFailed)
}

/// The result of [`probe_cas`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeResult {
    /// `If-None-Match` and `If-Match` are enforced on PUT (the CAS verdict).
    pub cas: bool,
    /// `If-Match` is enforced on DELETE: a release may delete by its kept
    /// ETag with no read first.
    pub conditional_delete: bool,
    /// The form of the ETag in `If-Match` that the backend enforces.
    pub if_match: IfMatchDialect,
}

impl ProbeResult {
    /// The profile that every conditional request to this backend uses.
    pub fn profile(&self) -> super::if_match::ConditionalProfile {
        super::if_match::ConditionalProfile {
            if_match: self.if_match,
            conditional_delete: self.conditional_delete,
            // Not part of this probe: see [`probe_conditional_complete`].
            conditional_complete: None,
        }
    }
}

/// Pure: does CompleteMultipartUpload enforce a condition? `create` is the
/// answer to a Complete with `If-None-Match: *` over an existing key,
/// `wrong` the one with an `If-Match` of another ETag (both must be 412 and
/// leave the old object), `right` the one with the current ETag (it must
/// write: a backend that refuses every condition proves nothing, and a
/// dialect that does not fit the backend shows here). `Some(true)`: proven.
/// `Some(false)`: a condition is ignored or refused. `None`: the probe
/// could not tell (no answer, an unclear one): never a proof, so the write
/// with a condition of a large object stays refused.
pub fn classify_conditional_complete(
    create: &StepOutcome,
    wrong: &StepOutcome,
    right: &StepOutcome,
) -> Option<bool> {
    use StepOutcome::*;
    for outcome in [create, wrong] {
        match outcome {
            PreconditionFailed => {}
            Written | NotImplemented => return Some(false),
            Dropped(_) | Other(_) => return None,
        }
    }
    match right {
        Written => Some(true),
        PreconditionFailed | NotImplemented => Some(false),
        Dropped(_) | Other(_) => None,
    }
}

/// One tiny upload (one part of 3 bytes) of `key`, completed with the
/// condition that `condition` sets. The upload is aborted when the Complete
/// did not write it, so the backend keeps no open upload.
async fn complete_with<F>(client: &Client, bucket: &str, key: &str, condition: F) -> StepOutcome
where
    F: FnOnce(
        aws_sdk_s3::operation::complete_multipart_upload::builders::CompleteMultipartUploadFluentBuilder,
    ) -> aws_sdk_s3::operation::complete_multipart_upload::builders::CompleteMultipartUploadFluentBuilder,
{
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let created = match client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
    {
        Ok(c) => c,
        Err(e) => return StepOutcome::Other(format!("CreateMultipartUpload: {e:?}")),
    };
    let Some(upload_id) = created.upload_id().map(str::to_string) else {
        return StepOutcome::Other("CreateMultipartUpload returned no upload id".into());
    };
    // Armed until the Complete wrote the object: every other way out, a
    // cut included, aborts the upload (no open upload stays on the backend).
    let guard = {
        let (client, bucket, key, upload_id) = (
            client.clone(),
            bucket.to_string(),
            key.to_string(),
            upload_id.clone(),
        );
        crate::storage::mpu_guard::MpuAbortGuard::new(move || {
            let (client, bucket, key, upload_id) = (
                client.clone(),
                bucket.clone(),
                key.clone(),
                upload_id.clone(),
            );
            async move {
                let _ = client
                    .abort_multipart_upload()
                    .bucket(&bucket)
                    .key(&key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
            }
        })
    };
    let part = match client
        .upload_part()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from_static(b"new"))
        .send()
        .await
    {
        Ok(p) => p,
        Err(e) => {
            guard.abort().await;
            return StepOutcome::Other(format!("UploadPart: {e:?}"));
        }
    };
    let request = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .parts(
                    CompletedPart::builder()
                        .part_number(1)
                        .set_e_tag(part.e_tag().map(str::to_string))
                        .build(),
                )
                .build(),
        );
    let result = condition(request).send().await;
    let step = outcome(&result);
    if step == StepOutcome::Written {
        guard.disarm();
    } else {
        guard.abort().await;
    }
    step
}

/// Probe whether CompleteMultipartUpload enforces `If-None-Match` and
/// `If-Match` on `bucket/key` (a key of its own: the probe writes it and
/// removes it with every version). Three tiny uploads, each of one part:
/// a Complete with `If-None-Match: *` over the existing key and one with a
/// wrong `If-Match` (both must get 412 and leave the old object, and no
/// upload stays open), then one with the current ETag (must write). Run it
/// after [`probe_cas`] proved the conditional PUT, with its `dialect`.
/// The verdict is [`classify_conditional_complete`].
pub async fn probe_conditional_complete(
    client: &Client,
    bucket: &str,
    key: &str,
    dialect: IfMatchDialect,
) -> Option<bool> {
    // Armed from before the first write: a cut future leaves the key, and
    // the guard removes it (see [`SweepOnDrop`]).
    let sweep = SweepOnDrop::new(client, bucket, key);
    let seed = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from_static(b"old"))
        .send()
        .await
        .ok();
    let verdict = match seed {
        None => None,
        Some(seed) => {
            let etag = seed.e_tag().unwrap_or_default().to_string();
            async {
                let create = complete_with(client, bucket, key, |r| r.if_none_match("*")).await;
                // The old object must be intact after each refusal: a
                // refusal that went with a changed (or an unreadable) key
                // proves nothing.
                if create == StepOutcome::PreconditionFailed {
                    match key_holds_checked(client, bucket, key, b"old").await {
                        Some(true) => {}
                        Some(false) => return Some(false),
                        None => return None,
                    }
                }
                let wrong = complete_with(client, bucket, key, |r| {
                    r.if_match_etag(WRONG_ETAG, dialect)
                })
                .await;
                if wrong == StepOutcome::PreconditionFailed {
                    match key_holds_checked(client, bucket, key, b"old").await {
                        Some(true) => {}
                        Some(false) => return Some(false),
                        None => return None,
                    }
                }
                if etag.is_empty() {
                    return None;
                }
                let right =
                    complete_with(client, bucket, key, |r| r.if_match_etag(&etag, dialect)).await;
                classify_conditional_complete(&create, &wrong, &right)
            }
            .await
        }
    };
    remove_all_versions(client, bucket, key).await;
    sweep.disarm();
    verdict
}

/// What a probe that gave no verdict says about the bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeErrorKind {
    /// The backend ANSWERED the probe's plain write with a refusal that no
    /// retry changes: the bucket does not exist, the credentials are wrong
    /// or have no right to write. The configuration is wrong.
    Misconfigured,
    /// No answer, or an answer that a later attempt can change (timeout,
    /// connection error, 5xx, throttling). The probe says nothing.
    Indeterminate,
}

/// Why [`probe_cas`] gave no verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError {
    pub kind: ProbeErrorKind,
    pub detail: String,
    /// A refusal that a new bucket or a new access key can still cause
    /// ([`code_may_propagate`]): the boot waits a short time before it
    /// takes the refusal as final.
    pub propagates: bool,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// A conditional step that the probe could not read is never a verdict on
/// the configuration: the plain write went through before it.
impl From<String> for ProbeError {
    fn from(detail: String) -> Self {
        Self {
            kind: ProbeErrorKind::Indeterminate,
            detail,
            propagates: false,
        }
    }
}

/// S3 error codes of an answer that a later attempt can change, whatever
/// the HTTP status they come with.
const RETRYABLE_CODES: &[&str] = &[
    "RequestTimeout",
    "TemporaryRedirect",
    "OperationAborted",
    "ExpiredToken",
    // A full store or an account over its quota is the store's state, not a
    // wrong configuration.
    "QuotaExceeded",
    // MinIO: a full bucket quota (`400`).
    "XMinioAdminBucketQuotaExceeded",
    "StorageQuotaExceeded",
    "storage_cap_exceeded",
    "RequestTimeTooSkewed",
    concat!("Slow", "Down"),
    "Throttling",
    "ThrottlingException",
    "TooManyRequests",
    "RequestLimitExceeded",
    concat!("Service", "Unavailable"),
    "InternalError",
];

/// Codes of a refusal that a new bucket or a new access key can still
/// cause: they take a moment to reach every part of the store.
const PROPAGATING_CODES: &[&str] = &[
    "AccessDenied",
    "InvalidAccessKeyId",
    "NoSuchBucket",
    // A key that was made a moment ago may not be known to every part of
    // the store yet.
    "SignatureDoesNotMatch",
];

/// Pure: can a bucket or a key that was made a moment ago explain `code`?
pub fn code_may_propagate(code: &str) -> bool {
    PROPAGATING_CODES.contains(&code)
}

/// Pure: what the answer to the probe's first, UNCONDITIONAL write says.
/// `status` is the HTTP status (`None`: no HTTP answer), `code` the S3
/// error code that the answer's XML body carries (empty when it has none:
/// an ingress, a WAF or a proxy answered, not the store).
///
/// Definitive ONLY for a parsed S3 error code that no retry changes, on a
/// 3xx or 4xx status: the bucket is missing (`NoSuchBucket`), the key is
/// unknown (`InvalidAccessKeyId`), the signature or the region is wrong,
/// the credentials may not write (`AccessDenied`), the bucket lives
/// elsewhere (`PermanentRedirect`). Everything else is indeterminate: no
/// answer, 5xx, no S3 code, 408, 429 and the codes in `RETRYABLE_CODES`
/// (answers that depend on load, on time or on a token).
pub fn classify_write_refusal(status: Option<u16>, code: &str) -> ProbeErrorKind {
    let Some(status) = status else {
        return ProbeErrorKind::Indeterminate;
    };
    if code.is_empty() || RETRYABLE_CODES.contains(&code) || matches!(status, 408 | 429) {
        return ProbeErrorKind::Indeterminate;
    }
    match status {
        300..=499 => ProbeErrorKind::Misconfigured,
        _ => ProbeErrorKind::Indeterminate,
    }
}

/// The HTTP status and the S3 error code of an SDK error (`None` status:
/// the backend gave no HTTP answer).
fn answer_of<E>(e: &aws_sdk_s3::error::SdkError<E>) -> (Option<u16>, String)
where
    E: aws_sdk_s3::error::ProvideErrorMetadata,
{
    use aws_sdk_s3::error::ProvideErrorMetadata;
    let status = match e {
        aws_sdk_s3::error::SdkError::ServiceError(svc) => Some(svc.raw().status().as_u16()),
        _ => None,
    };
    (status, e.code().unwrap_or_default().to_string())
}

fn outcome<T, E>(res: &Result<T, aws_sdk_s3::error::SdkError<E>>) -> StepOutcome
where
    E: std::error::Error + aws_sdk_s3::error::ProvideErrorMetadata + 'static,
{
    match res {
        Ok(_) => StepOutcome::Written,
        Err(
            e @ (aws_sdk_s3::error::SdkError::DispatchFailure(_)
            | aws_sdk_s3::error::SdkError::TimeoutError(_)
            | aws_sdk_s3::error::SdkError::ResponseError(_)),
        ) => StepOutcome::Dropped(format!("{e:?}")),
        Err(e) => {
            let signal = crate::coordination::cas::sdk_error_signal(e);
            if crate::coordination::cas::conditional_write_lost(&signal) {
                StepOutcome::PreconditionFailed
            } else if crate::coordination::cas::is_not_implemented(&signal) {
                StepOutcome::NotImplemented
            } else {
                StepOutcome::Other(format!("{signal}: {e:?}"))
            }
        }
    }
}

/// Does `bucket/key` hold exactly `body`? `Some(false)`: another body, or
/// no key (the backend answered `NoSuchKey`). `None`: the backend could not
/// be asked (a transport error, another error answer, a broken body): that
/// proves nothing either way.
async fn key_holds_checked(client: &Client, bucket: &str, key: &str, body: &[u8]) -> Option<bool> {
    match client.get_object().bucket(bucket).key(key).send().await {
        Ok(out) => out
            .body
            .collect()
            .await
            .ok()
            .map(|b| b.into_bytes().as_ref() == body),
        Err(e) if e.as_service_error().is_some_and(|s| s.is_no_such_key()) => Some(false),
        Err(_) => None,
    }
}

/// [`key_holds_checked`] for a caller that takes "cannot tell" as "no".
async fn key_holds(client: &Client, bucket: &str, key: &str, body: &[u8]) -> bool {
    key_holds_checked(client, bucket, key, body).await == Some(true)
}

/// Removes a probe key with every version when it is dropped, unless it was
/// disarmed: a probe future that is cut (its deadline, a caller that gives
/// up) stops between its writes and its cleanup, and the key would stay in a
/// data bucket where a client lists it. The sweep runs on a task of its own.
struct SweepOnDrop {
    client: Client,
    bucket: String,
    key: String,
    armed: bool,
}

impl SweepOnDrop {
    fn new(client: &Client, bucket: &str, key: &str) -> Self {
        Self {
            client: client.clone(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            armed: true,
        }
    }

    /// The probe removed its key itself.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for SweepOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), self.key.clone());
        runtime.spawn(async move { remove_all_versions(&client, &bucket, &key).await });
    }
}

/// On a bucket that keeps versions, the probe's writes and its delete stay
/// as noncurrent versions and a delete marker. The probe runs in data
/// buckets (the re-encrypt job probes the bucket it converts), so it
/// removes every version of its own key, best effort. A bucket without
/// versioning lists none, or only the deleted key's.
async fn purge_versions(client: &Client, bucket: &str, key: &str) {
    let Ok(page) = client
        .list_object_versions()
        .bucket(bucket)
        .prefix(key)
        .send()
        .await
    else {
        return;
    };
    let ids = page
        .versions()
        .iter()
        .filter(|v| v.key() == Some(key))
        .filter_map(|v| v.version_id())
        .chain(
            page.delete_markers()
                .iter()
                .filter(|m| m.key() == Some(key))
                .filter_map(|m| m.version_id()),
        );
    for id in ids {
        let _ = client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .version_id(id)
            .send()
            .await;
    }
}

/// Remove `key` with every version of it (best effort): the cleanup of a
/// probe, also of one that a deadline cut short.
pub async fn remove_all_versions(client: &Client, bucket: &str, key: &str) {
    let _ = client.delete_object().bucket(bucket).key(key).send().await;
    purge_versions(client, bucket, key).await;
}

/// The folder of every probe key. It is not a client object: the plaintext
/// sample of the boot and the encrypting wrapper's refusal log skip it
/// ([`is_probe_key`]).
pub const PROBE_PREFIX: &str = ".deltaglider/_cwprobe/";

/// A probe key of a fresh probe.
pub fn new_probe_key() -> String {
    format!("{PROBE_PREFIX}{}", uuid::Uuid::new_v4())
}

/// Is `key` the exact shape of a probe key: [`PROBE_PREFIX`], a lowercase
/// hyphenated UUID, and an optional `-mpu`, nothing more? A client key under
/// the folder (the folder is reserved for new writes, but an older release
/// may have stored one) is never a probe key, so nothing removes it.
pub fn is_probe_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix(PROBE_PREFIX) else {
        return false;
    };
    let id = rest.strip_suffix("-mpu").unwrap_or(rest);
    // 36 characters can only be the hyphenated form; the lowercase test
    // keeps `new_probe_key`'s exact text.
    id.len() == 36
        && !id.bytes().any(|b| b.is_ascii_uppercase())
        && uuid::Uuid::parse_str(id).is_ok()
}

/// A probe key or open upload older than this is litter of a probe that
/// never ended. A live probe of a peer ends within seconds, so it is never
/// touched.
pub const STALE_PROBE_AGE_SECS: i64 = 3600;

/// Pure: is something that was written at `written_unix` litter at `now_unix`?
pub fn is_stale_probe(written_unix: i64, now_unix: i64) -> bool {
    is_older_than(written_unix, now_unix, STALE_PROBE_AGE_SECS)
}

fn is_older_than(written_unix: i64, now_unix: i64, min_age_secs: i64) -> bool {
    now_unix.saturating_sub(written_unix) > min_age_secs
}

/// Pure: which of `entries` (key, version or upload id, written unix
/// seconds) to remove. `now` is the BACKEND's clock (the `Date` of the
/// listing), never this node's: a node clock that runs fast must not make a
/// live probe look stale. Without a backend time, nothing goes. Only the
/// exact probe shape ([`is_probe_key`]) and only older than
/// `min_age_secs` (production: [`STALE_PROBE_AGE_SECS`]).
pub fn stale_probe_entries(
    now: Option<i64>,
    min_age_secs: i64,
    entries: impl IntoIterator<Item = (String, String, i64)>,
) -> Vec<(String, String)> {
    let Some(now) = now else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|(key, _, at)| is_probe_key(key) && is_older_than(*at, now, min_age_secs))
        .map(|(key, id, _)| (key, id))
        .collect()
}

/// Remove the litter of probes that never ended (the process stopped
/// before the probe's own cleanup ran): every version and delete marker of
/// a probe key under [`PROBE_PREFIX`] of `bucket`, and its open multipart
/// uploads, each older than [`STALE_PROBE_AGE_SECS`] on the backend's clock.
/// Best effort and bounded: one page of each listing, and it ends at
/// `deadline`. It never fails a probe.
pub async fn sweep_stale_probes(client: &Client, bucket: &str, deadline: tokio::time::Instant) {
    sweep_probes_older_than(client, bucket, deadline, STALE_PROBE_AGE_SECS).await;
}

/// [`sweep_stale_probes`] with the age as an argument, so that a test of a
/// real versioned backend does not wait an hour. No caller in the proxy
/// passes another age.
pub async fn sweep_probes_older_than(
    client: &Client,
    bucket: &str,
    deadline: tokio::time::Instant,
    min_age_secs: i64,
) {
    let _ = tokio::time::timeout_at(deadline, async {
        let date = super::server_clock::ServerDate::default();
        if let Ok(page) = client
            .list_object_versions()
            .bucket(bucket)
            .prefix(PROBE_PREFIX)
            .max_keys(100)
            .customize()
            .interceptor(date.clone())
            .send()
            .await
        {
            let versions = page.versions().iter().filter_map(|v| {
                Some((
                    v.key()?.to_string(),
                    v.version_id()?.to_string(),
                    v.last_modified()?.secs(),
                ))
            });
            let markers = page.delete_markers().iter().filter_map(|m| {
                Some((
                    m.key()?.to_string(),
                    m.version_id()?.to_string(),
                    m.last_modified()?.secs(),
                ))
            });
            for (key, id) in stale_probe_entries(date.get(), min_age_secs, versions.chain(markers))
            {
                let _ = client
                    .delete_object()
                    .bucket(bucket)
                    .key(key)
                    .version_id(id)
                    .send()
                    .await;
            }
        } else {
            // A backend that cannot list versions (Cloudflare R2): list the
            // keys of the folder and delete each match without a version id.
            // Same shape rule, backend clock, page bound and deadline.
            let date = super::server_clock::ServerDate::default();
            if let Ok(page) = client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(PROBE_PREFIX)
                .max_keys(100)
                .customize()
                .interceptor(date.clone())
                .send()
                .await
            {
                let keys = page.contents().iter().filter_map(|o| {
                    Some((
                        o.key()?.to_string(),
                        String::new(),
                        o.last_modified()?.secs(),
                    ))
                });
                for (key, _) in stale_probe_entries(date.get(), min_age_secs, keys) {
                    let _ = client.delete_object().bucket(bucket).key(key).send().await;
                }
            }
        }
        let date = super::server_clock::ServerDate::default();
        if let Ok(page) = client
            .list_multipart_uploads()
            .bucket(bucket)
            .prefix(PROBE_PREFIX)
            .max_uploads(100)
            .customize()
            .interceptor(date.clone())
            .send()
            .await
        {
            let uploads = page.uploads().iter().filter_map(|u| {
                Some((
                    u.key()?.to_string(),
                    u.upload_id()?.to_string(),
                    u.initiated()?.secs(),
                ))
            });
            for (key, id) in stale_probe_entries(date.get(), min_age_secs, uploads) {
                let _ = client
                    .abort_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(id)
                    .send()
                    .await;
            }
        }
    })
    .await;
}

/// The ETag of `bucket/key` now (`None` when the HEAD fails).
async fn current_etag(client: &Client, bucket: &str, key: &str) -> Option<String> {
    crate::storage::BACKEND_HEAD_REQUESTS.inc();
    let head = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .ok()?;
    head.e_tag().map(str::to_string).filter(|e| !e.is_empty())
}

/// Run the probe on `bucket/key` and delete the key afterwards (best
/// effort). `Err` = indeterminate, as in [`classify_cas_probe`]. Seven
/// requests: four PUTs, the DELETE with a wrong ETag, the cleanup, and the
/// version listing of the version cleanup (plus one DELETE for each version
/// of the key on a bucket that keeps versions). A backend that refuses the
/// correct ETag quoted costs four more: the GET that tells a refusal from a
/// lost response, two unquoted PUTs and a HEAD.
pub async fn probe_cas(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<ProbeResult, ProbeError> {
    // Armed from before the first write (see [`SweepOnDrop`]).
    let sweep = SweepOnDrop::new(client, bucket, key);
    let first = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from_static(b"1"))
        .send()
        .await
        .map_err(|e| {
            let (status, code) = answer_of(&e);
            let kind = classify_write_refusal(status, &code);
            ProbeError {
                kind,
                detail: format!("probe could not write to '{bucket}': {e:?}"),
                propagates: kind == ProbeErrorKind::Misconfigured && code_may_propagate(&code),
            }
        })?;
    let etag = first.e_tag().unwrap_or_default().to_string();
    let put = |body: &'static [u8]| {
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(body))
    };
    // One `If-Match` PUT of `body` on `etag` in `dialect`.
    let put_if = |body: &'static [u8], etag: String, dialect: IfMatchDialect| async move {
        match outcome(&put(body).if_match_etag(&etag, dialect).send().await) {
            // The write landed but its response was lost, and the SDK retry
            // met the probe's own new ETag. The key then holds the probe's
            // body: the condition held, it is no refusal.
            StepOutcome::PreconditionFailed if key_holds(client, bucket, key, body).await => {
                StepOutcome::Written
            }
            other => other,
        }
    };
    let create = outcome(&put(b"2").if_none_match("*").send().await);
    let wrong = outcome(
        &put(b"3")
            .if_match_etag(WRONG_ETAG, IfMatchDialect::Quoted)
            .send()
            .await,
    );
    let right = if etag.is_empty() {
        StepOutcome::Other("the first PUT returned no etag".into())
    } else {
        put_if(b"4", etag, IfMatchDialect::Quoted).await
    };
    // The correct ETag, quoted, refused (Ceph RGW: Hetzner): the unquoted
    // form, with a wrong and with the current ETag (read again: the key
    // may hold another write of the probe's by now).
    let unquoted = if right == StepOutcome::PreconditionFailed {
        let wrong = outcome(
            &put(b"6")
                .if_match_etag(WRONG_ETAG, IfMatchDialect::Unquoted)
                .send()
                .await,
        );
        let right = match current_etag(client, bucket, key).await {
            Some(now) => put_if(b"7", now, IfMatchDialect::Unquoted).await,
            None => StepOutcome::Other("the probe could not read its key's ETag".into()),
        };
        Some(UnquotedSteps { wrong, right })
    } else {
        None
    };
    // A conditional step without an HTTP answer: does a plain PUT of the
    // key go through now? Then the backend dropped the condition, not the
    // connection (Backblaze B2).
    let dropped = [&create, &wrong, &right]
        .into_iter()
        .chain(unquoted.iter().flat_map(|u| [&u.wrong, &u.right]))
        .any(|o| matches!(o, StepOutcome::Dropped(_)));
    let (create, wrong, right, unquoted) = if dropped {
        let control = put(b"5").send().await.is_ok();
        (
            resolve_dropped(create, control),
            resolve_dropped(wrong, control),
            resolve_dropped(right, control),
            unquoted.map(|u| UnquotedSteps {
                wrong: resolve_dropped(u.wrong, control),
                right: resolve_dropped(u.right, control),
            }),
        )
    } else {
        (create, wrong, right, unquoted)
    };
    let verdict = classify_cas_dialect(&create, &wrong, &right, unquoted.as_ref());
    let dialect = verdict
        .as_ref()
        .map_or(IfMatchDialect::Quoted, |(_, dialect)| *dialect);
    // The key exists here (the probe wrote it). A DELETE with a wrong ETag
    // must leave it; a backend that ignores the condition deletes it, and
    // the cleanup below is then a no-op.
    let delete_wrong = outcome(
        &client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .if_match_etag(WRONG_ETAG, dialect)
            .send()
            .await,
    );
    remove_all_versions(client, bucket, key).await;
    sweep.disarm();
    let (cas, if_match) = verdict?;
    Ok(ProbeResult {
        cas,
        conditional_delete: classify_conditional_delete(&delete_wrong),
        if_match,
    })
}

#[cfg(test)]
mod drop_and_unknown_tests {
    use super::*;
    use std::time::Duration;

    fn client_for(endpoint: &str) -> Client {
        Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .credentials_provider(aws_sdk_s3::config::Credentials::new(
                    "a", "b", None, None, "t",
                ))
                .force_path_style(true)
                .endpoint_url(endpoint)
                .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
                .build(),
        )
    }

    /// Wait until `fake` holds no object under `prefix`, at most 5 s.
    async fn gone(fake: &crate::storage::fake_s3::FakeS3, prefix: &str) -> bool {
        for _ in 0..100 {
            if fake.stored_count(prefix) == 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// A probe future that is cut (its deadline) between its writes and its
    /// cleanup leaves no key in the data bucket: the drop guard removes it.
    /// Stage 1 is cut inside its DELETE with the key written.
    #[tokio::test]
    async fn a_cut_conditional_put_probe_leaves_no_key() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let client = client_for(&endpoint);
        fake.set_delay_ms("DELETE", 5_000);
        let cut = tokio::time::timeout(
            Duration::from_millis(1_500),
            probe_cas(&client, "b", "probe/stage1"),
        )
        .await;
        assert!(cut.is_err(), "the probe was cut by its deadline");
        assert_eq!(fake.stored_count("b/probe/stage1"), 1, "the key is there");
        fake.set_delay_ms("DELETE", 0);
        assert!(
            gone(&fake, "b/probe/stage1").await,
            "the drop guard removed the key"
        );
    }

    /// The same for the conditional Complete probe: the seed object of a cut
    /// stage 2 (cut inside its CreateMultipartUpload) is removed.
    #[tokio::test]
    async fn a_cut_conditional_complete_probe_leaves_no_seed() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let client = client_for(&endpoint);
        fake.set_delay_ms("POST", 5_000);
        let cut = tokio::time::timeout(
            Duration::from_secs(1),
            probe_conditional_complete(&client, "b", "probe/stage2-mpu", IfMatchDialect::Quoted),
        )
        .await;
        assert!(cut.is_err(), "the probe was cut by its deadline");
        assert_eq!(
            fake.stored_count("b/probe/stage2-mpu"),
            1,
            "the seed is there"
        );
        fake.set_delay_ms("POST", 0);
        assert!(
            gone(&fake, "b/probe/stage2-mpu").await,
            "the drop guard removed the seed"
        );
    }

    /// `key_holds_checked`: the key's body, another body, no key, and a
    /// backend that cannot be asked (a 500): only the last is "cannot tell".
    #[tokio::test]
    async fn a_failed_read_is_unknown_not_false() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let client = client_for(&endpoint);
        fake.insert_object("b/k", b"old", &[]);
        assert_eq!(
            key_holds_checked(&client, "b", "k", b"old").await,
            Some(true)
        );
        assert_eq!(
            key_holds_checked(&client, "b", "k", b"new").await,
            Some(false)
        );
        assert_eq!(
            key_holds_checked(&client, "b", "missing", b"old").await,
            Some(false),
            "no key: it does not hold the body"
        );
        fake.fail("GET", "b/k", 500, "InternalError", u32::MAX);
        assert_eq!(
            key_holds_checked(&client, "b", "k", b"old").await,
            None,
            "a read error says nothing"
        );
        assert!(!key_holds(&client, "b", "k", b"old").await);
    }
}

#[cfg(test)]
mod conditional_complete_tests {
    use super::StepOutcome::*;
    use super::*;

    /// The verdict over the three Completes of the probe: proven only when
    /// both conditions are refused (412) AND the right one writes. A
    /// condition that is ignored or refused is a definite `false`; an
    /// answer that is unclear is no verdict (`None`), never a proof.
    #[test]
    fn the_conditional_complete_verdict_truth_table() {
        let d = || Dropped("reset".into());
        let o = || Other("500".into());
        let table: &[(StepOutcome, StepOutcome, StepOutcome, Option<bool>)] = &[
            (PreconditionFailed, PreconditionFailed, Written, Some(true)),
            // A condition that the backend ignores.
            (Written, PreconditionFailed, Written, Some(false)),
            (PreconditionFailed, Written, Written, Some(false)),
            (NotImplemented, PreconditionFailed, Written, Some(false)),
            (PreconditionFailed, NotImplemented, Written, Some(false)),
            // Refuses the correct condition too (or a wrong dialect).
            (
                PreconditionFailed,
                PreconditionFailed,
                PreconditionFailed,
                Some(false),
            ),
            (
                PreconditionFailed,
                PreconditionFailed,
                NotImplemented,
                Some(false),
            ),
            // No clear answer.
            (d(), PreconditionFailed, Written, None),
            (PreconditionFailed, o(), Written, None),
            (PreconditionFailed, PreconditionFailed, d(), None),
            (PreconditionFailed, PreconditionFailed, o(), None),
        ];
        for (create, wrong, right, want) in table {
            assert_eq!(
                classify_conditional_complete(create, wrong, right),
                *want,
                "{create:?} {wrong:?} {right:?}"
            );
        }
    }
}

#[cfg(test)]
mod write_refusal_tests {
    use super::ProbeErrorKind::{Indeterminate, Misconfigured};
    use super::*;

    /// The truth table over the answers a plain PUT gets from S3, MinIO,
    /// Ceph and Backblaze B2.
    #[test]
    fn a_refusal_is_definitive_only_when_no_retry_changes_it() {
        let table: &[(Option<u16>, &str, ProbeErrorKind)] = &[
            // The backend answered: the configuration is wrong.
            (Some(404), "NoSuchBucket", Misconfigured),
            (Some(403), "AccessDenied", Misconfigured),
            (Some(403), "InvalidAccessKeyId", Misconfigured),
            (Some(403), "SignatureDoesNotMatch", Misconfigured),
            (Some(403), "AllAccessDisabled", Misconfigured),
            (Some(400), "AuthorizationHeaderMalformed", Misconfigured),
            (Some(400), "InvalidBucketName", Misconfigured),
            (Some(301), "PermanentRedirect", Misconfigured),
            (Some(405), "MethodNotAllowed", Misconfigured),
            // A later attempt can get another answer.
            (None, "", Indeterminate),
            (None, "NoSuchBucket", Indeterminate),
            (Some(500), "InternalError", Indeterminate),
            (Some(502), "", Indeterminate),
            (Some(503), "SlowDown", Indeterminate),
            (Some(503), "ServiceUnavailable", Indeterminate),
            (Some(504), "", Indeterminate),
            (Some(408), "", Indeterminate),
            (Some(429), "", Indeterminate),
            (Some(400), "RequestTimeout", Indeterminate),
            (Some(400), "Throttling", Indeterminate),
            (Some(403), "RequestTimeTooSkewed", Indeterminate),
            (Some(200), "", Indeterminate),
            // No S3 error code: an ingress or a WAF answered, not the store.
            (Some(404), "", Indeterminate),
            (Some(403), "", Indeterminate),
            (Some(301), "", Indeterminate),
            // A token or a redirect that a retry changes.
            (Some(400), "ExpiredToken", Indeterminate),
            (Some(307), "TemporaryRedirect", Indeterminate),
            // A full store is its state, not the configuration.
            (Some(403), "QuotaExceeded", Indeterminate),
            // MinIO answers a full bucket quota with this code (measured).
            (Some(400), "XMinioAdminBucketQuotaExceeded", Indeterminate),
            (Some(507), "", Indeterminate),
            (Some(400), "storage_cap_exceeded", Indeterminate),
            (Some(409), "OperationAborted", Indeterminate),
        ];
        for (status, code, want) in table {
            assert_eq!(
                classify_write_refusal(*status, code),
                *want,
                "status {status:?} code {code:?}"
            );
        }
    }

    #[test]
    fn only_four_codes_may_propagate() {
        for code in [
            "AccessDenied",
            "InvalidAccessKeyId",
            "NoSuchBucket",
            "SignatureDoesNotMatch",
        ] {
            assert!(code_may_propagate(code), "{code}");
        }
        for code in ["PermanentRedirect", "InvalidBucketName", "", "SlowDown"] {
            assert!(!code_may_propagate(code), "{code}");
        }
    }

    /// A step after the first write never names the configuration.
    #[test]
    fn a_later_step_is_indeterminate() {
        assert_eq!(ProbeError::from("x".to_string()).kind, Indeterminate);
    }
}

#[cfg(test)]
mod lost_response_tests {
    //! A conditional PUT whose write lands but whose response is lost: the
    //! SDK retries it, and the retry meets the probe's own write.

    use super::*;
    use aws_sdk_s3::config::{BehaviorVersion, Region};
    use aws_smithy_runtime_api::client::http::{
        HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
    };
    use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
    use aws_smithy_runtime_api::client::result::ConnectorError;
    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
    use aws_smithy_types::body::SdkBody;
    use std::sync::{Arc, Mutex};

    /// The object's body and ETag.
    type Object = Option<(Vec<u8>, String)>;

    /// One object with CAS. The first `If-Match` write that matches lands,
    /// and then the connection resets before the response.
    #[derive(Debug, Clone, Default)]
    struct LossyCas {
        object: Arc<Mutex<Object>>,
        writes: Arc<Mutex<u32>>,
        lost_one: Arc<Mutex<bool>>,
    }

    fn status(code: u16, error: Option<&str>) -> HttpResponse {
        let body = error.map_or_else(SdkBody::empty, |c| {
            SdkBody::from(format!(
                "<Error><Code>{c}</Code><Message>x</Message></Error>"
            ))
        });
        HttpResponse::new(code.try_into().unwrap(), body)
    }

    impl HttpConnector for LossyCas {
        fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
            let mut object = self.object.lock().unwrap();
            let resp = match request.method() {
                "PUT" => {
                    let body = request.body().bytes().unwrap_or_default().to_vec();
                    let current = object.as_ref().map(|(_, e)| e.clone());
                    let if_match = request.headers().get("if-match").map(str::to_string);
                    let if_none = request.headers().get("if-none-match").is_some();
                    if (if_none && current.is_some())
                        || if_match
                            .as_ref()
                            .is_some_and(|m| Some(m) != current.as_ref())
                    {
                        status(412, Some("PreconditionFailed"))
                    } else {
                        let mut writes = self.writes.lock().unwrap();
                        *writes += 1;
                        let etag = format!("\"e{writes}\"");
                        *object = Some((body, etag.clone()));
                        let mut lost = self.lost_one.lock().unwrap();
                        if if_match.is_some() && !*lost {
                            *lost = true;
                            return HttpConnectorFuture::ready(Err(ConnectorError::io(Box::new(
                                std::io::Error::new(
                                    std::io::ErrorKind::ConnectionReset,
                                    "reset after the write",
                                ),
                            ))));
                        }
                        let mut r = status(200, None);
                        r.headers_mut().insert("etag", etag);
                        r
                    }
                }
                "GET" => match object.as_ref() {
                    Some((body, etag)) => {
                        let mut r =
                            HttpResponse::new(200.try_into().unwrap(), SdkBody::from(body.clone()));
                        r.headers_mut().insert("etag", etag.clone());
                        r
                    }
                    None => status(404, Some("NoSuchKey")),
                },
                "DELETE" => {
                    *object = None;
                    status(204, None)
                }
                _ => status(501, Some("NotImplemented")),
            };
            HttpConnectorFuture::ready(Ok(resp))
        }
    }

    impl HttpClient for LossyCas {
        fn http_connector(
            &self,
            _: &HttpConnectorSettings,
            _: &RuntimeComponents,
        ) -> SharedHttpConnector {
            SharedHttpConnector::new(self.clone())
        }
    }

    /// Step 4 is an `If-Match` PUT. Its write landed, the response was
    /// lost, and the SDK retry got 412 against the probe's own new ETag:
    /// the verdict was NonCas, cached until restart, so every later config
    /// apply was refused (and a boot exited).
    #[tokio::test]
    async fn a_lost_response_on_the_probes_own_write_is_cas() {
        let fake = LossyCas::default();
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(aws_credential_types::Credentials::new(
                "k", "s", None, None, "test",
            ))
            .endpoint_url("http://127.0.0.1:1")
            .force_path_style(true)
            .http_client(fake.clone())
            .retry_config(
                aws_sdk_s3::config::retry::RetryConfig::standard()
                    .with_max_attempts(3)
                    .with_initial_backoff(std::time::Duration::from_millis(1)),
            )
            .build();
        let client = aws_sdk_s3::Client::from_conf(conf);
        let verdict = probe_cas(&client, "coord", "_dgp/probe").await;
        assert!(*fake.lost_one.lock().unwrap(), "the fake lost a response");
        assert_eq!(
            verdict.map(|p| p.cas),
            Ok(true),
            "the probe's own landed write read as NonCas"
        );
    }
}

/// A TCP-level fake of a backend that drops the connection on every
/// conditional PUT (Backblaze B2, eu-central-003): no HTTP answer at all.
/// Plain PUTs, DELETEs and listings work. Shared with the atomicity tests.
#[cfg(test)]
pub(crate) mod b2_like {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The fake's counters.
    #[derive(Debug, Default)]
    pub(crate) struct Counts {
        /// Conditional PUTs whose connection the fake dropped.
        pub dropped: AtomicU32,
        /// Version listings (the cleanup of every version of a key).
        pub version_lists: AtomicU32,
    }

    /// Start the fake; returns its endpoint and counters. `reachable =
    /// false`: every request is dropped (an unreachable backend).
    pub(crate) async fn start(reachable: bool) -> (String, Arc<Counts>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let counts = Arc::new(Counts::default());
        let c = counts.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c = c.clone();
                tokio::spawn(async move { serve(sock, c, reachable).await });
            }
        });
        (endpoint, counts)
    }

    async fn serve(mut sock: tokio::net::TcpStream, c: Arc<Counts>, reachable: bool) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        while buf.len() < head_end + len {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let line = head.lines().next().unwrap_or("").to_string();
        let conditional = head.contains("\nif-match:") || head.contains("\nif-none-match:");
        if !reachable || (line.starts_with("put ") && conditional) {
            if reachable {
                c.dropped.fetch_add(1, Ordering::SeqCst);
            }
            return; // close without a byte of answer
        }
        let (status, extra, body) = if line.starts_with("put ") {
            ("200 OK", "ETag: \"e1\"\r\n", String::new())
        } else if line.starts_with("delete ") {
            ("204 No Content", "", String::new())
        } else if line.starts_with("get / ") || line.starts_with("get /?") {
            (
                "200 OK",
                "",
                "<ListAllMyBucketsResult><Buckets><Bucket><Name>coord</Name>\
                 </Bucket></Buckets></ListAllMyBucketsResult>"
                    .to_string(),
            )
        } else if line.contains("versions") {
            c.version_lists.fetch_add(1, Ordering::SeqCst);
            (
                "200 OK",
                "",
                "<ListVersionsResult></ListVersionsResult>".to_string(),
            )
        } else {
            (
                "404 Not Found",
                "",
                "<Error><Code>NoSuchKey</Code></Error>".to_string(),
            )
        };
        let resp = format!(
            "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    }
}

#[cfg(test)]
mod dropped_connection_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    async fn client(endpoint: String) -> Client {
        crate::config_db_sync::ConfigDbSync::build_client(&crate::config::BackendConfig::S3 {
            endpoint: Some(endpoint),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            allow_local: true,
            session_token: None,
        })
        .await
        .unwrap()
    }

    /// Backblaze B2 drops the connection on `If-None-Match` / `If-Match`
    /// PUTs. A plain PUT works, so the drop is the backend's answer to the
    /// condition: a definitive non-CAS verdict, after a bounded number of
    /// attempts (the SDK's retries), not an endless retry.
    #[tokio::test]
    async fn a_backend_that_drops_conditional_puts_is_not_cas() {
        let (endpoint, counts) = b2_like::start(true).await;
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            probe_cas(&client(endpoint).await, "coord", "_probe"),
        )
        .await
        .expect("the probe ends");
        assert_eq!(got.map(|p| p.cas), Ok(false));
        let dropped = counts.dropped.load(Ordering::SeqCst);
        assert!(
            (3..=12).contains(&dropped),
            "three conditional PUTs, each retried a bounded number of times: {dropped}"
        );
    }

    /// A backend that answers nothing at all is unreachable, not non-CAS.
    #[tokio::test]
    async fn an_unreachable_backend_is_indeterminate() {
        let (endpoint, _) = b2_like::start(false).await;
        let got = probe_cas(&client(endpoint).await, "coord", "_probe").await;
        assert!(got.is_err(), "{got:?}");
    }

    #[test]
    fn a_dropped_step_is_a_refusal_only_when_a_plain_put_works() {
        let dropped = || StepOutcome::Dropped("reset".into());
        assert_eq!(
            resolve_dropped(dropped(), true),
            StepOutcome::NotImplemented
        );
        assert!(matches!(
            resolve_dropped(dropped(), false),
            StepOutcome::Other(_)
        ));
        assert_eq!(
            resolve_dropped(StepOutcome::PreconditionFailed, true),
            StepOutcome::PreconditionFailed
        );
    }
}

#[cfg(test)]
mod conditional_delete_tests {
    use super::*;

    async fn fake_client() -> (Client, std::sync::Arc<crate::storage::FakeS3>) {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let client =
            crate::config_db_sync::ConfigDbSync::build_client(&crate::config::BackendConfig::S3 {
                endpoint: Some(endpoint),
                region: "us-east-1".into(),
                force_path_style: true,
                access_key_id: Some("k".into()),
                secret_access_key: Some("s".into()),
                allow_local: true,
                session_token: None,
            })
            .await
            .unwrap();
        (client, fake)
    }

    #[tokio::test]
    async fn the_probe_reports_an_enforced_conditional_delete() {
        let (client, fake) = fake_client().await;
        let got = probe_cas(&client, "coord", "_probe").await;
        assert_eq!(
            got,
            Ok(ProbeResult {
                cas: true,
                conditional_delete: true,
                if_match: IfMatchDialect::Quoted,
            })
        );
        assert_eq!(fake.requests().len(), 7, "{:?}", fake.requests());
    }

    /// A backend that enforces `If-Match` on PUT only: CAS holds, but a
    /// release must not delete by its kept ETag alone.
    #[tokio::test]
    async fn the_probe_reports_an_ignored_conditional_delete() {
        let (client, fake) = fake_client().await;
        fake.set_delete_if_match_ignored(true);
        let got = probe_cas(&client, "coord", "_probe").await;
        assert_eq!(
            got,
            Ok(ProbeResult {
                cas: true,
                conditional_delete: false,
                if_match: IfMatchDialect::Quoted,
            })
        );
    }

    #[test]
    fn only_a_412_proves_conditional_delete() {
        assert!(classify_conditional_delete(
            &StepOutcome::PreconditionFailed
        ));
        assert!(!classify_conditional_delete(&StepOutcome::Written));
        assert!(!classify_conditional_delete(&StepOutcome::NotImplemented));
        assert!(!classify_conditional_delete(&StepOutcome::Other(
            "timeout".into()
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StepOutcome::*;

    #[test]
    fn full_cas_is_verified() {
        assert_eq!(
            classify_cas_probe(&PreconditionFailed, &PreconditionFailed, &Written),
            Ok(true)
        );
    }

    /// The case a create-only probe passed: `If-None-Match` enforced,
    /// `If-Match` ignored. Renew and steal would then silently clobber.
    #[test]
    fn ignored_if_match_is_not_cas() {
        assert_eq!(
            classify_cas_probe(&PreconditionFailed, &Written, &Written),
            Ok(false)
        );
        assert_eq!(
            classify_cas_probe(&PreconditionFailed, &NotImplemented, &Written),
            Ok(false)
        );
        assert_eq!(
            classify_cas_probe(
                &PreconditionFailed,
                &PreconditionFailed,
                &PreconditionFailed
            ),
            Ok(false),
            "refusing the current etag breaks renew"
        );
    }

    #[test]
    fn ignored_or_rejected_create_is_not_cas() {
        assert_eq!(
            classify_cas_probe(&Written, &PreconditionFailed, &Written),
            Ok(false)
        );
        assert_eq!(
            classify_cas_probe(&NotImplemented, &PreconditionFailed, &Written),
            Ok(false)
        );
    }

    /// The `If-Match` dialect: the quoted form decides unless it refused
    /// the correct ETag; then the unquoted form must refuse a wrong ETag
    /// AND write with the right one.
    #[test]
    fn the_dialect_truth_table() {
        use super::super::if_match::IfMatchDialect::*;
        let pf = || PreconditionFailed;
        let u = |wrong, right| UnquotedSteps { wrong, right };
        // AWS / MinIO: quoted works, the unquoted steps never ran.
        assert_eq!(
            classify_cas_dialect(&pf(), &pf(), &Written, None),
            Ok((true, Quoted))
        );
        // Ceph RGW (Hetzner).
        assert_eq!(
            classify_cas_dialect(&pf(), &pf(), &pf(), Some(&u(pf(), Written))),
            Ok((true, Unquoted))
        );
        // Unquoted ignored (a wrong ETag written): not CAS.
        assert_eq!(
            classify_cas_dialect(&pf(), &pf(), &pf(), Some(&u(Written, Written))),
            Ok((false, Quoted))
        );
        // Both forms refuse the correct ETag: not CAS, as before.
        assert_eq!(
            classify_cas_dialect(&pf(), &pf(), &pf(), Some(&u(pf(), pf()))),
            Ok((false, Quoted))
        );
        // The quoted wrong ETag written: not CAS whatever the unquoted say.
        assert_eq!(
            classify_cas_dialect(&pf(), &Written, &pf(), Some(&u(pf(), Written))),
            Ok((false, Quoted))
        );
        // If-None-Match ignored: not CAS.
        assert_eq!(
            classify_cas_dialect(&Written, &pf(), &pf(), Some(&u(pf(), Written))),
            Ok((false, Quoted))
        );
        // An unclear unquoted step: indeterminate.
        assert!(
            classify_cas_dialect(&pf(), &pf(), &pf(), Some(&u(Other("x".into()), Written)))
                .is_err()
        );
        assert!(
            classify_cas_dialect(&pf(), &pf(), &pf(), Some(&u(pf(), Other("x".into())))).is_err()
        );
        // A refusal of the correct ETag with no unquoted steps: not CAS.
        assert_eq!(
            classify_cas_dialect(&pf(), &pf(), &pf(), None),
            Ok((false, Quoted))
        );
    }

    #[test]
    fn transport_errors_are_indeterminate() {
        assert!(classify_cas_probe(&Other("timeout".into()), &Written, &Written).is_err());
        assert!(classify_cas_probe(&PreconditionFailed, &Other("x".into()), &Written).is_err());
        assert!(
            classify_cas_probe(&PreconditionFailed, &PreconditionFailed, &Other("x".into()))
                .is_err()
        );
    }
}

#[cfg(test)]
mod stale_probe_tests {
    use super::*;

    #[test]
    fn the_age_rule_removes_only_what_is_older_than_the_limit() {
        let now = 1_000_000;
        assert!(is_stale_probe(now - STALE_PROBE_AGE_SECS - 1, now));
        assert!(!is_stale_probe(now - STALE_PROBE_AGE_SECS, now));
        assert!(!is_stale_probe(now - 5, now), "a live peer's probe");
        assert!(!is_stale_probe(now + 60, now), "a clock ahead of ours");
        assert!(!is_stale_probe(i64::MIN, i64::MIN), "no overflow");
        assert!(is_stale_probe(i64::MIN, i64::MAX), "no overflow");
    }

    #[test]
    fn only_the_exact_probe_shape_is_a_probe_key() {
        assert!(is_probe_key(&new_probe_key()));
        assert!(is_probe_key(&format!("{}-mpu", new_probe_key())));
        let id = uuid::Uuid::new_v4().to_string();
        for not in [
            "_cwprobe/x".to_string(),
            ".deltaglider/other/x".to_string(),
            "releases/.deltaglider/_cwprobe/x".to_string(),
            format!("{PROBE_PREFIX}report.pdf"),
            format!("{PROBE_PREFIX}old"),
            format!("{PROBE_PREFIX}{id}.pdf"),
            format!("{PROBE_PREFIX}{id}-mpu-x"),
            format!("{PROBE_PREFIX}{id}/more"),
            format!("{PROBE_PREFIX}sub/{id}"),
            format!("{PROBE_PREFIX}{}", id.to_uppercase()),
            format!("{PROBE_PREFIX}{}", id.replace('-', "")),
            format!("{PROBE_PREFIX}{id}-"),
            PROBE_PREFIX.to_string(),
        ] {
            assert!(!is_probe_key(&not), "{not}");
        }
    }

    #[test]
    fn no_backend_time_removes_nothing() {
        let old = || [("k".to_string(), "v".to_string(), 0)];
        let key = new_probe_key();
        let entries = [(key.clone(), "v".to_string(), 0)];
        assert!(stale_probe_entries(None, STALE_PROBE_AGE_SECS, entries.clone()).is_empty());
        assert_eq!(
            stale_probe_entries(
                Some(STALE_PROBE_AGE_SECS + 1),
                STALE_PROBE_AGE_SECS,
                entries
            ),
            [(key, "v".to_string())]
        );
        assert!(
            stale_probe_entries(Some(STALE_PROBE_AGE_SECS + 1), STALE_PROBE_AGE_SECS, old())
                .is_empty(),
            "a key that is no probe key"
        );
    }

    async fn fake_client() -> (aws_sdk_s3::Client, std::sync::Arc<crate::storage::FakeS3>) {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let client =
            crate::config_db_sync::ConfigDbSync::build_client(&crate::config::BackendConfig::S3 {
                endpoint: Some(endpoint),
                region: "us-east-1".into(),
                force_path_style: true,
                access_key_id: Some("k".into()),
                secret_access_key: Some("s".into()),
                allow_local: true,
                session_token: None,
            })
            .await
            .unwrap();
        (client, fake)
    }

    fn in_ten_seconds() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(10)
    }

    /// An old probe key and an old open upload go; a fresh pair and every
    /// client key (also one under the probe folder, also an old upload of a
    /// client) stay.
    #[tokio::test]
    async fn the_sweep_removes_stale_probe_keys_and_uploads_only() {
        let (client, fake) = fake_client().await;
        let now = chrono::Utc::now().timestamp();
        let old = now - STALE_PROBE_AGE_SECS - 60;
        let path = |key: &str| format!("b/{PROBE_PREFIX}{key}");
        let (old_id, fresh_id) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let (old_key, fresh_key) = (path(&old_id), path(&fresh_id));
        let clients = [
            path("report.pdf"),
            path(&format!("x{old_id}.pdf")),
            path(&format!("sub/{old_id}")),
            "b/releases/app.bin".to_string(),
        ];
        for p in [&old_key, &fresh_key].into_iter().chain(&clients) {
            fake.insert_object(p, b"1", &[]);
            fake.set_modified(p, if *p == fresh_key { now } else { old });
        }
        let old_mpu = path(&format!("{old_id}-mpu"));
        fake.insert_upload(&old_mpu, "u-old", old);
        fake.insert_upload(&path(&format!("{fresh_id}-mpu")), "u-fresh", now);
        fake.insert_upload(&path("big.iso"), "u-client-1", old);
        fake.insert_upload("b/releases/big.bin", "u-client-2", old);

        sweep_stale_probes(&client, "b", in_ten_seconds()).await;

        assert_eq!(fake.stored_count(&old_key), 0, "the old probe key stays");
        assert_eq!(fake.stored_count(&fresh_key), 1, "a live probe's key went");
        for c in &clients {
            assert_eq!(fake.stored_count(c), 1, "the client key {c} went");
        }
        assert_eq!(fake.open_uploads(&old_mpu), 0, "the old upload stays");
        assert_eq!(fake.open_uploads("b/"), 3, "a fresh or client upload went");
    }

    /// A node clock that runs an hour fast must not make a live probe look
    /// stale: the age is judged by the backend's `Date`.
    #[tokio::test]
    async fn a_fast_node_clock_does_not_make_a_live_probe_stale() {
        let (client, fake) = fake_client().await;
        let backend_now = chrono::Utc::now().timestamp() - 2 * STALE_PROBE_AGE_SECS;
        fake.set_clock(backend_now);
        let key = format!("b/{}", new_probe_key());
        fake.insert_object(&key, b"1", &[]);
        fake.set_modified(&key, backend_now - 5);
        let mpu = format!("{key}-mpu");
        fake.insert_upload(&mpu, "u", backend_now - 5);
        assert!(
            chrono::Utc::now().timestamp() - (backend_now - 5) > STALE_PROBE_AGE_SECS,
            "by this node's clock the probe looks old"
        );

        sweep_stale_probes(&client, "b", in_ten_seconds()).await;

        assert_eq!(fake.stored_count(&key), 1, "a live probe's key went");
        assert_eq!(fake.open_uploads(&mpu), 1, "a live probe's upload went");
        // The backend's clock moves on past the limit: now it is litter.
        fake.set_clock(backend_now + STALE_PROBE_AGE_SECS + 60);
        sweep_stale_probes(&client, "b", in_ten_seconds()).await;
        assert_eq!(fake.stored_count(&key), 0);
        assert_eq!(fake.open_uploads(&mpu), 0);
    }

    fn litter(fake: &crate::storage::FakeS3) -> String {
        let key = format!("b/{}", new_probe_key());
        fake.insert_object(&key, b"1", &[]);
        fake.set_modified(
            &key,
            chrono::Utc::now().timestamp() - 2 * STALE_PROBE_AGE_SECS,
        );
        key
    }

    fn group(endpoint: String) -> crate::coordination::capability::ClientWritableGroup {
        crate::coordination::capability::ClientWritableGroup {
            backend: crate::config::BackendConfig::S3 {
                endpoint: Some(endpoint),
                region: "us-east-1".into(),
                force_path_style: true,
                access_key_id: Some("k".into()),
                secret_access_key: Some("s".into()),
                allow_local: true,
                session_token: None,
            },
            buckets: vec![],
            probe_bucket: "b".into(),
        }
    }

    /// The capability probe sweeps first (its own entry point).
    #[tokio::test]
    async fn the_capability_probe_removes_old_litter_first() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let key = litter(&fake);
        let verdict = crate::coordination::capability::establish_backend_verdict_within(
            "n",
            &group(endpoint),
            &Default::default(),
            std::time::Duration::from_secs(15),
        )
        .await;
        assert!(
            matches!(
                verdict,
                crate::coordination::CapabilityVerdict::CasVerified { .. }
            ),
            "{verdict:?}"
        );
        assert_eq!(fake.stored_count(&key), 0, "the litter stays");
    }

    /// A sweep that the backend delays stops at its share of the budget; the
    /// probe still ends with its verdict.
    #[tokio::test]
    async fn a_slow_sweep_leaves_the_capability_verdict_unchanged() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        // The upload listing is the sweep's alone (the probe's own cleanup lists versions).
        fake.set_delay_ms("LIST_UPLOADS", 30_000);
        let started = std::time::Instant::now();
        let verdict = crate::coordination::capability::establish_backend_verdict_within(
            "n",
            &group(endpoint),
            &Default::default(),
            std::time::Duration::from_secs(6),
        )
        .await;
        assert!(
            matches!(
                verdict,
                crate::coordination::CapabilityVerdict::CasVerified { .. }
            ),
            "{verdict:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(6));
    }

    /// The probe of the coordination bucket sweeps first too.
    #[tokio::test]
    async fn the_coordination_probe_removes_old_litter_first() {
        let (client, fake) = fake_client().await;
        let key = litter(&fake);
        crate::config_db_sync::validate_cas_bucket(&client, "b", "witness-test")
            .await
            .unwrap();
        assert_eq!(fake.stored_count(&key), 0, "the litter stays");
    }

    /// A backend that cannot list versions (R2) still gets its old probe key
    /// removed, through a plain listing; a client key and a fresh probe key stay.
    #[tokio::test]
    async fn without_a_versions_listing_the_sweep_lists_keys() {
        let (client, fake) = fake_client().await;
        fake.fail("LIST_VERSIONS", "", 501, "NotImplemented", u32::MAX);
        let now = chrono::Utc::now().timestamp();
        let old = now - STALE_PROBE_AGE_SECS - 60;
        let (old_key, fresh_key) = (
            format!("b/{}", new_probe_key()),
            format!("b/{}", new_probe_key()),
        );
        let client_key = format!("b/{PROBE_PREFIX}report.pdf");
        for (p, at) in [(&old_key, old), (&fresh_key, now), (&client_key, old)] {
            fake.insert_object(p, b"1", &[]);
            fake.set_modified(p, at);
        }

        sweep_stale_probes(&client, "b", in_ten_seconds()).await;

        assert_eq!(fake.stored_count(&old_key), 0, "the old probe key stays");
        assert_eq!(fake.stored_count(&fresh_key), 1, "a live probe's key went");
        assert_eq!(fake.stored_count(&client_key), 1, "a client key went");
    }

    /// A sweep that the backend delays ends at its budget.
    #[tokio::test]
    async fn a_slow_sweep_ends_at_its_deadline() {
        let (client, fake) = fake_client().await;
        fake.set_delay_ms("LIST_VERSIONS", 5000);
        let started = std::time::Instant::now();
        sweep_stale_probes(
            &client,
            "b",
            tokio::time::Instant::now() + std::time::Duration::from_millis(300),
        )
        .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }
}
