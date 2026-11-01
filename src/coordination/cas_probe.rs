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
//! A backend that drops the connection on a conditional PUT (Backblaze B2,
//! eu-central-003, 2026-10-31) gives no answer at all. When an
//! unconditional PUT of the same key goes through right after, the drop is
//! the backend's answer to the condition: the probe reads it as a refusal
//! (definitive non-CAS), not as an unreachable backend.

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;

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

/// Does `bucket/key` hold exactly `body`? `false` when it cannot be read.
async fn key_holds(client: &Client, bucket: &str, key: &str, body: &[u8]) -> bool {
    match client.get_object().bucket(bucket).key(key).send().await {
        Ok(out) => out
            .body
            .collect()
            .await
            .is_ok_and(|b| b.into_bytes().as_ref() == body),
        Err(_) => false,
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

/// Run the probe on `bucket/key` and delete the key afterwards (best
/// effort). `Err` = indeterminate, as in [`classify_cas_probe`]. Seven
/// requests: four PUTs, the DELETE with a wrong ETag, the cleanup, and the
/// version listing of the version cleanup (plus one DELETE for each version
/// of the key on a bucket that keeps versions).
pub async fn probe_cas(client: &Client, bucket: &str, key: &str) -> Result<ProbeResult, String> {
    let first = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from_static(b"1"))
        .send()
        .await
        .map_err(|e| format!("probe could not write to '{bucket}': {e:?}"))?;
    let etag = first.e_tag().unwrap_or_default().to_string();
    let put = |body: &'static [u8]| {
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(body))
    };
    let create = outcome(&put(b"2").if_none_match("*").send().await);
    let wrong = outcome(
        &put(b"3")
            .if_match("\"00000000000000000000000000000000\"")
            .send()
            .await,
    );
    let right = if etag.is_empty() {
        StepOutcome::Other("the first PUT returned no etag".into())
    } else {
        match outcome(&put(b"4").if_match(&etag).send().await) {
            // The write landed but its response was lost, and the SDK retry
            // met the probe's own new ETag. The key then holds the probe's
            // body: the condition held, it is no refusal.
            StepOutcome::PreconditionFailed if key_holds(client, bucket, key, b"4").await => {
                StepOutcome::Written
            }
            other => other,
        }
    };
    // A conditional step without an HTTP answer: does a plain PUT of the
    // key go through now? Then the backend dropped the condition, not the
    // connection (Backblaze B2).
    let (create, wrong, right) = if [&create, &wrong, &right]
        .iter()
        .any(|o| matches!(o, StepOutcome::Dropped(_)))
    {
        let control = put(b"5").send().await.is_ok();
        (
            resolve_dropped(create, control),
            resolve_dropped(wrong, control),
            resolve_dropped(right, control),
        )
    } else {
        (create, wrong, right)
    };
    // The key exists here (the probe wrote it). A DELETE with a wrong ETag
    // must leave it; a backend that ignores the condition deletes it, and
    // the cleanup below is then a no-op.
    let delete_wrong = outcome(
        &client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .if_match("\"00000000000000000000000000000000\"")
            .send()
            .await,
    );
    remove_all_versions(client, bucket, key).await;
    Ok(ProbeResult {
        cas: classify_cas_probe(&create, &wrong, &right)?,
        conditional_delete: classify_conditional_delete(&delete_wrong),
    })
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
                conditional_delete: true
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
                conditional_delete: false
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
