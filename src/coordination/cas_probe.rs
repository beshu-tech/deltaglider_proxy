// SPDX-License-Identifier: BUSL-1.1

//! The conditional-write probe for a backend or bucket.
//!
//! Leases and reference locks use BOTH conditions: `If-None-Match:*` to
//! create, `If-Match:<etag>` to steal, renew and release. A backend that
//! honours only the first passes a create-only probe and then lets two nodes
//! renew or steal the same lock. So the probe tests both, on a caller-owned
//! key, and the verdict is a pure function of the four step outcomes.

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
    /// Anything else: the probe cannot tell.
    Other(String),
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
            Other(e) => return Err(format!("{step}: {e}")),
        }
    }
    match if_match_right_etag {
        Written => Ok(true),
        // Refusing the CORRECT etag breaks renew and release just as surely.
        PreconditionFailed | NotImplemented => Ok(false),
        Other(e) => Err(format!("If-Match with the current etag: {e}")),
    }
}

fn outcome<T, E>(res: &Result<T, aws_sdk_s3::error::SdkError<E>>) -> StepOutcome
where
    E: std::error::Error + aws_sdk_s3::error::ProvideErrorMetadata + 'static,
{
    match res {
        Ok(_) => StepOutcome::Written,
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

/// Run the probe on `bucket/key` and delete the key afterwards (best
/// effort). Same three-way result as [`classify_cas_probe`].
pub async fn probe_cas(client: &Client, bucket: &str, key: &str) -> Result<bool, String> {
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
    let _ = client.delete_object().bucket(bucket).key(key).send().await;
    classify_cas_probe(&create, &wrong, &right)
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
            verdict,
            Ok(true),
            "the probe's own landed write read as NonCas"
        );
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
