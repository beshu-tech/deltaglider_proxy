// SPDX-License-Identifier: BUSL-1.1

//! The streamed form POST: the pure decisions (file cap, length range,
//! form length, file part), the handler over an engine on the filesystem
//! backend with a 1 MiB spool threshold (the medium of the file, the RAM
//! peak, and for every refusal: nothing stored, every budget whole, and
//! for a refusal before the file, the file never read), and the source
//! guards of the order "signature first, file last".

use super::*;
use crate::deltaglider::memory_budget::MemoryBudget;
use crate::deltaglider::{DeltaGliderEngine, DynEngine};
use crate::iam::AuthConfig;
use crate::metrics::Metrics;
use crate::storage::{DynStorageBackend, FilesystemBackend};
use proptest::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const THRESHOLD: usize = MIB;
const SPOOL_MIB: usize = 16;
const MAX_OBJECT: usize = 32 * MIB;
const AK: &str = "FORMACCESSKEY";
const SK: &str = "FORMSECRETKEY123";
const BOUNDARY: &str = "----form-boundary";

// ── Pure decisions ──────────────────────────────────────────────────────

#[test]
fn form_file_cap_truth_table() {
    use FileBound::*;
    let cap = |bytes, bound| FileCap { bytes, bound };
    // (limit, form length, policy max) -> cap
    let cases = [
        (100, 50, None, cap(50, Form)),
        (100, 500, None, cap(100, Limit)),
        (100, 100, None, cap(100, Limit)),
        (100, 50, Some(10), cap(10, Policy)),
        (100, 500, Some(10), cap(10, Policy)),
        (100, 500, Some(1000), cap(100, Limit)),
        (100, 50, Some(70), cap(50, Form)),
        // A tie names the policy: the file over it is AccessDenied.
        (100, 50, Some(50), cap(50, Policy)),
        (100, 500, Some(100), cap(100, Policy)),
        (100, 0, Some(0), cap(0, Policy)),
    ];
    for (limit, form, max, want) in cases {
        assert_eq!(
            form_file_cap(limit, form, max),
            want,
            "{limit} {form} {max:?}"
        );
    }
}

proptest! {
    /// The cap is the smallest bound, and the bound it names has that value.
    #[test]
    fn the_file_cap_is_the_smallest_bound(
        limit in 0u64..1 << 40,
        form in 0u64..1 << 40,
        max in proptest::option::of(0u64..1 << 40),
    ) {
        let cap = form_file_cap(limit, form, max);
        let smallest = limit.min(form).min(max.unwrap_or(u64::MAX));
        prop_assert_eq!(cap.bytes, smallest);
        let named = match cap.bound {
            FileBound::Limit => limit,
            FileBound::Form => form,
            FileBound::Policy => max.unwrap(),
        };
        prop_assert_eq!(named, cap.bytes);
        if max == Some(smallest) {
            prop_assert_eq!(cap.bound, FileBound::Policy);
        }
    }
}

#[test]
fn over_cap_refusals_name_the_bound() {
    let code = |bound| over_cap_refusal(FileCap { bytes: 10, bound }, "bkt", "k").code();
    assert_eq!(code(FileBound::Policy), "AccessDenied");
    assert_eq!(code(FileBound::Limit), "EntityTooLarge");
    assert_eq!(code(FileBound::Form), "InvalidRequest");
}

#[test]
fn check_file_len_truth_table() {
    let range = Some(LengthRange { min: 2, max: 5 });
    let ok = |r, len| check_file_len(r, len, "bkt", "k").is_ok();
    assert!(ok(None, 0));
    assert!(ok(None, u64::MAX));
    assert!(!ok(range, 1));
    assert!(ok(range, 2));
    assert!(ok(range, 5));
    assert!(!ok(range, 6));
    // Two ranges: the file must be in both.
    let both = LengthRange { min: 2, max: 5 }.and(LengthRange { min: 3, max: 9 });
    assert_eq!(both, LengthRange { min: 3, max: 5 });
    // An empty range admits nothing.
    assert!(!ok(Some(LengthRange { min: 5, max: 2 }), 3));
}

#[test]
fn form_body_len_truth_table() {
    let headers = |len: Option<&str>| {
        let mut h = HeaderMap::new();
        if let Some(len) = len {
            h.insert(axum::http::header::CONTENT_LENGTH, len.parse().unwrap());
        }
        h
    };
    let code = |h: HeaderMap| form_body_len(&h, 100).map_err(|e| e.code());
    assert_eq!(code(headers(Some("10"))), Ok(10));
    assert_eq!(code(headers(Some("0"))), Ok(0));
    assert_eq!(
        code(headers(Some(&(100 + FORM_OVERHEAD).to_string()))),
        Ok(100 + FORM_OVERHEAD)
    );
    assert_eq!(
        code(headers(Some(&(101 + FORM_OVERHEAD).to_string()))),
        Err("EntityTooLarge")
    );
    assert_eq!(code(headers(None)), Err("MissingContentLength"));
    assert_eq!(code(headers(Some("x"))), Err("MissingContentLength"));
}

#[test]
fn the_file_part_is_named_file_or_has_a_file_name() {
    assert!(is_file_part("file", None));
    assert!(is_file_part("upload", Some("a.bin")));
    assert!(is_file_part("file", Some("")));
    assert!(!is_file_part("key", None));
    assert!(!is_file_part("files", None));
}

/// The policy check reads no file byte: it returns the range, and a
/// malformed `content-length-range` is refused before the file.
#[test]
fn the_policy_check_returns_the_range_and_judges_its_form() {
    let fields: HashMap<String, String> = [("key".to_string(), "k".to_string())].into();
    let check = |conditions: serde_json::Value| {
        let policy = serde_json::json!({
            "expiration": "2099-01-01T00:00:00Z",
            "conditions": conditions,
        });
        validate_form_post_policy(&policy, &fields, "bkt", "k", "text/plain")
    };
    assert_eq!(
        check(serde_json::json!([{ "key": "k" }])).unwrap(),
        None,
        "no range"
    );
    assert_eq!(
        check(serde_json::json!([
            { "key": "k" },
            ["content-length-range", 1, 10],
            ["content-length-range", 4, 20],
        ]))
        .unwrap(),
        Some(LengthRange { min: 4, max: 10 })
    );
    for bad in [
        serde_json::json!(["content-length-range", "a", 10]),
        serde_json::json!(["content-length-range", 1]),
        serde_json::json!(["content-length-range", 1, 2.5]),
    ] {
        let err = check(serde_json::json!([{ "key": "k" }, bad.clone()])).unwrap_err();
        assert_eq!(err.code(), "InvalidArgument", "{bad}");
    }
}

// ── The handler over a filesystem engine ────────────────────────────────

struct Rig {
    _dir: tempfile::TempDir,
    spool_dir: std::path::PathBuf,
    state: Arc<AppState>,
    iam: SharedIamState,
    metrics: Arc<Metrics>,
}

impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
            FilesystemBackend::new(dir.path().join("data"))
                .await
                .unwrap(),
        );
        let mut config = crate::config::Config {
            max_object_size: MAX_OBJECT as u64,
            ..Default::default()
        };
        config.tuning.spool_threshold_bytes = Some(THRESHOLD as u64);
        config.tuning.spool_acquire_timeout_secs = 1;
        let metrics = Arc::new(Metrics::new());
        let spool_dir = dir.path().join("spool");
        let engine: DynEngine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, Some(metrics.clone()))
                .with_memory_budget(MemoryBudget::new(8 * MIB as u64))
                .with_spool_dir(
                    crate::deltaglider::spool::SpoolDir::new(
                        spool_dir.clone(),
                        (SPOOL_MIB * MIB) as u64,
                    )
                    .unwrap(),
                );
        engine.create_bucket("bkt").await.unwrap();
        let iam = Arc::new(arc_swap::ArcSwap::from_pointee(IamState::Legacy(
            AuthConfig {
                access_key_id: AK.into(),
                secret_access_key: SK.into(),
            },
        )));
        Self {
            _dir: dir,
            spool_dir,
            state: AppState::for_tests(engine),
            iam,
            metrics,
        }
    }

    fn engine(&self) -> Arc<DynEngine> {
        self.state.engine.load_full()
    }

    fn bodies(&self, medium: &str, reason: &str) -> u64 {
        self.metrics
            .ingest_bodies_total
            .with_label_values(&[medium, reason])
            .get()
    }

    /// Nothing held: the RAM budget, the spool budget and the spool dir.
    fn assert_restored(&self, when: &str) {
        let engine = self.engine();
        assert_eq!(engine.memory_budget().held_bytes(), 0, "{when}: RAM held");
        assert_eq!(
            engine.spool_dir().free_mib(),
            SPOOL_MIB,
            "{when}: spool budget held"
        );
        let files: Vec<_> = std::fs::read_dir(&self.spool_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(files.is_empty(), "{when}: spool files left: {files:?}");
    }

    async fn stored(&self, key: &str) -> Option<Vec<u8>> {
        self.engine()
            .retrieve("bkt", key)
            .await
            .ok()
            .map(|(d, _)| d)
    }

    /// POST a form of `len` bytes, streamed from `body`.
    async fn post<S>(&self, body: S, len: usize) -> Result<Response, S3Error>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}")
                .parse()
                .unwrap(),
        );
        headers.insert(axum::http::header::CONTENT_LENGTH, len.into());
        handle_form_post_upload(
            &self.state,
            "bkt",
            Some(&self.iam),
            &headers,
            &axum::http::Extensions::new(),
            body,
            None,
        )
        .await
    }

    /// POST `form` in 64 KiB chunks, as a client sends it.
    async fn post_form(&self, form: &Form) -> Result<Response, S3Error> {
        let wire = form.wire();
        let chunks: Vec<_> = wire
            .chunks(64 * KIB)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        self.post(futures::stream::iter(chunks), wire.len()).await
    }
}

fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

/// A form: the signed fields before the file, the file, the fields after.
struct Form {
    before: Vec<(String, String)>,
    file: Vec<u8>,
    after: Vec<(String, String)>,
}

impl Form {
    /// A form for `key` whose policy has `extra` conditions, signed with
    /// `secret`, expiring at `expiration`.
    fn signed(
        key: &str,
        file: Vec<u8>,
        extra: &[serde_json::Value],
        secret: &str,
        expiration: &str,
    ) -> Self {
        let amz_date = "20260507T120000Z";
        let credential = format!("{AK}/20260507/us-east-1/s3/aws4_request");
        let mut conditions = vec![
            serde_json::json!({ "bucket": "bkt" }),
            serde_json::json!(["starts-with", "$key", ""]),
            serde_json::json!({ "x-amz-algorithm": "AWS4-HMAC-SHA256" }),
            serde_json::json!({ "x-amz-credential": credential }),
            serde_json::json!({ "x-amz-date": amz_date }),
        ];
        conditions.extend(extra.iter().cloned());
        let policy = serde_json::json!({ "expiration": expiration, "conditions": conditions });
        let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
        let signing_key = derive_v4_signing_key(secret, "20260507", "us-east-1");
        let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
        let before = [
            ("key", key.to_string()),
            ("policy", policy_b64),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256".into()),
            ("x-amz-credential", credential),
            ("x-amz-date", amz_date.into()),
            ("x-amz-signature", signature),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        Self {
            before,
            file,
            after: Vec::new(),
        }
    }

    fn valid(key: &str, file: Vec<u8>, extra: &[serde_json::Value]) -> Self {
        Self::signed(key, file, extra, SK, "2099-01-01T00:00:00Z")
    }

    fn field(name: &str, value: &str) -> Vec<u8> {
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .into_bytes()
    }

    /// The bytes up to the file's first byte: the fields and the file
    /// part's headers.
    fn head(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (k, v) in &self.before {
            out.extend(Self::field(k, v));
        }
        out.extend(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"upload.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .into_bytes(),
        );
        out
    }

    /// The bytes after the file's last byte.
    fn tail(&self) -> Vec<u8> {
        let mut out = b"\r\n".to_vec();
        for (k, v) in &self.after {
            out.extend(Self::field(k, v));
        }
        out.extend(format!("--{BOUNDARY}--\r\n").into_bytes());
        out
    }

    fn wire(&self) -> Vec<u8> {
        let mut out = self.head();
        out.extend(&self.file);
        out.extend(self.tail());
        out
    }
}

/// A file three times the spool threshold goes to a spool file, with no
/// RAM held for it, and is stored; a small one goes to memory.
#[tokio::test]
async fn a_large_form_file_is_spooled_and_stored() {
    let rig = Rig::new().await;
    let data = body(3 * THRESHOLD);
    let resp = rig
        .post_form(&Form::valid("f/large.jpg", data.clone(), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let etag = resp.headers()["etag"].to_str().unwrap().to_string();
    use md5::Digest as _;
    assert!(
        etag.contains(&hex::encode(md5::Md5::digest(&data))),
        "{etag}"
    );
    assert_eq!(rig.bodies("spool", "over_threshold"), 1);
    assert_eq!(rig.engine().memory_budget().peak_bytes(), 0);
    rig.assert_restored("after a spooled form");
    assert_eq!(rig.stored("f/large.jpg").await.unwrap(), data);

    let small = body(100 * KIB);
    rig.post_form(&Form::valid("f/small.jpg", small.clone(), &[]))
        .await
        .unwrap();
    assert_eq!(rig.bodies("memory", "under_threshold"), 1);
    let peak = rig.engine().memory_budget().peak_bytes();
    assert!(peak > 0 && peak <= THRESHOLD as u64, "RAM peak {peak}");
    rig.assert_restored("after a small form");
    assert_eq!(rig.stored("f/small.jpg").await.unwrap(), small);
}

/// A stream that yields `first`, then is not ready once (a client that
/// has sent only that much), then yields `then`.
fn staged(
    first: Vec<u8>,
    then: Vec<Result<Bytes, std::io::Error>>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let mut first = Some(Bytes::from(first));
    let mut pending_once = true;
    let mut then = then.into_iter();
    futures::stream::poll_fn(move |cx| {
        if let Some(first) = first.take() {
            return std::task::Poll::Ready(Some(Ok(first)));
        }
        if std::mem::take(&mut pending_once) {
            cx.waker().wake_by_ref();
            return std::task::Poll::Pending;
        }
        std::task::Poll::Ready(then.next())
    })
}

/// A form refused for its signature or its policy is refused before a
/// byte of its file is read: the client has sent only the fields and the
/// file part's headers, and the answer comes (a handler that waited for
/// the file would never answer).
#[tokio::test]
async fn a_bad_signature_or_policy_never_reads_the_file() {
    let rig = Rig::new().await;
    let data = body(3 * THRESHOLD);
    let cases = [
        (
            "wrong secret",
            Form::signed(
                "f/x.jpg",
                data.clone(),
                &[],
                "WRONG",
                "2099-01-01T00:00:00Z",
            ),
            "SignatureDoesNotMatch",
        ),
        (
            "expired policy",
            Form::signed("f/x.jpg", data.clone(), &[], SK, "2000-01-01T00:00:00Z"),
            "AccessDenied",
        ),
        (
            "malformed content-length-range",
            Form::valid(
                "f/x.jpg",
                data.clone(),
                &[serde_json::json!(["content-length-range", "a", 1])],
            ),
            "InvalidArgument",
        ),
    ];
    for (what, form, code) in cases {
        // The head, then nothing more: the file never arrives.
        let mut head = Some(Bytes::from(form.head()));
        let stream = futures::stream::poll_fn(move |_| match head.take() {
            Some(head) => std::task::Poll::Ready(Some(Ok::<_, std::io::Error>(head))),
            None => std::task::Poll::Pending,
        });
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            rig.post(stream, form.wire().len()),
        )
        .await
        .unwrap_or_else(|_| panic!("{what}: the handler waited for the file"));
        let err = answer.unwrap_err();
        assert_eq!(err.code(), code, "{what}: {err:?}");
        rig.assert_restored(what);
        assert!(rig.stored("f/x.jpg").await.is_none(), "{what}: stored");
    }
}

/// Every refusal during or after the file: the right code, nothing
/// stored, every budget whole again.
#[tokio::test]
async fn every_file_failure_stores_nothing_and_restores_the_budgets() {
    let rig = Rig::new().await;
    for size in [100 * KIB, 3 * THRESHOLD] {
        let data = body(size);
        let len = size as u64;
        let range = |min: u64, max: u64| [serde_json::json!(["content-length-range", min, max])];
        // (what, form, cut the stream after this many file bytes with an
        // error, code)
        let cases: Vec<(&str, Form, Option<usize>, &str)> = vec![
            (
                "over the range maximum",
                Form::valid("f/x.jpg", data.clone(), &range(0, len - 1)),
                None,
                "AccessDenied",
            ),
            (
                "under the range minimum",
                Form::valid("f/x.jpg", data.clone(), &range(len + 1, len + 10)),
                None,
                "AccessDenied",
            ),
            (
                "stream error in the file",
                Form::valid("f/x.jpg", data.clone(), &[]),
                Some(size / 2),
                "IncompleteBody",
            ),
        ];
        for (what, form, cut, code) in cases {
            let what = format!("{what} ({size} bytes)");
            let err = match cut {
                None => rig.post_form(&form).await.unwrap_err(),
                Some(at) => {
                    // The file has begun to arrive when the stream fails.
                    let wire = form.wire();
                    let end = form.head().len() + at;
                    let stream = staged(
                        wire[..end].to_vec(),
                        vec![Err(std::io::Error::other("connection reset"))],
                    );
                    rig.post(stream, wire.len()).await.unwrap_err()
                }
            };
            assert_eq!(err.code(), code, "{what}: {err:?}");
            rig.assert_restored(&what);
            assert!(rig.stored("f/x.jpg").await.is_none(), "{what}: stored");
        }
        // The edges of the range are inside it.
        rig.post_form(&Form::valid("f/edge.jpg", data.clone(), &range(len, len)))
            .await
            .unwrap();
        assert_eq!(rig.stored("f/edge.jpg").await.unwrap(), data);
        rig.assert_restored("the range edge");
    }
}

/// The fields after the file are read and ignored, as on S3: a `submit`
/// field, and a metadata field no policy condition covers, which before
/// the file would be refused.
#[tokio::test]
async fn fields_after_the_file_are_ignored() {
    let rig = Rig::new().await;
    let data = body(2 * THRESHOLD);
    let mut form = Form::valid("f/after.jpg", data.clone(), &[]);
    form.after = vec![
        ("submit".into(), "Upload to Amazon S3".into()),
        ("x-amz-meta-evil".into(), "yes".into()),
    ];
    rig.post_form(&form).await.unwrap();
    assert_eq!(rig.stored("f/after.jpg").await.unwrap(), data);
    let meta = rig.engine().head("bkt", "f/after.jpg").await.unwrap();
    assert!(!meta.user_metadata.contains_key("evil"), "{meta:?}");
    rig.assert_restored("fields after the file");

    // A field after the file is still bounded by the small-field limit.
    let mut form = Form::valid("f/after2.jpg", data, &[]);
    form.after = vec![("submit".into(), "x".repeat(FORM_FIELD_MAX as usize + 1))];
    let err = rig.post_form(&form).await.unwrap_err();
    assert_eq!(err.code(), "InvalidArgument", "{err:?}");
    assert!(rig.stored("f/after2.jpg").await.is_none());
    rig.assert_restored("a large field after the file");
}

/// A field before the file is held in memory, so it is bounded, and so are
/// the fields together.
#[tokio::test]
async fn fields_before_the_file_are_bounded() {
    let rig = Rig::new().await;
    let mut form = Form::valid("f/big-field.jpg", body(10), &[]);
    form.before
        .push(("x-ignore-a".into(), "x".repeat(FORM_FIELD_MAX as usize + 1)));
    let err = rig.post_form(&form).await.unwrap_err();
    assert_eq!(err.code(), "InvalidArgument", "{err:?}");

    let mut form = Form::valid("f/many-fields.jpg", body(10), &[]);
    let n = (FORM_FIELDS_MAX / FORM_FIELD_MAX) as usize + 1;
    for i in 0..n {
        form.before
            .push((format!("x-ignore-{i}"), "x".repeat(FORM_FIELD_MAX as usize)));
    }
    let err = rig.post_form(&form).await.unwrap_err();
    assert_eq!(err.code(), "InvalidArgument", "{err:?}");
    rig.assert_restored("large fields");
}

/// An `x-amz-checksum-*` field before the file is computed while the file
/// arrives and checked after it: a right one stores, a wrong one is
/// `BadDigest` and stores nothing.
#[tokio::test]
async fn a_checksum_field_before_the_file_is_checked() {
    let rig = Rig::new().await;
    for size in [100 * KIB, 3 * THRESHOLD] {
        let data = body(size);
        let crc = |d: &[u8]| {
            let mut h = crate::api::checksum::ChecksumPlan::only(
                crate::api::checksum::ChecksumAlgorithm::Crc32,
            )
            .hasher();
            h.update(d);
            h.finalize().checksum_crc32.unwrap()
        };
        let cover = [serde_json::json!([
            "starts-with",
            "$x-amz-checksum-crc32",
            ""
        ])];
        let other = body(size + 1)[1..].to_vec();
        for (key, value, ok) in [
            ("f/right.jpg", crc(&data), true),
            ("f/wrong.jpg", crc(&other), false),
        ] {
            let mut form = Form::valid(key, data.clone(), &cover);
            form.before.push(("x-amz-checksum-crc32".into(), value));
            let got = rig.post_form(&form).await;
            if ok {
                got.unwrap_or_else(|e| panic!("{key} {size}: {e:?}"));
                assert_eq!(rig.stored(key).await.unwrap(), data);
            } else {
                assert_eq!(got.unwrap_err().code(), "BadDigest", "{key} {size}");
                assert!(rig.stored(key).await.is_none());
            }
            rig.assert_restored(key);
        }
    }
}

/// A form that declares no length, or more than the largest object, is
/// refused before a byte of it is read.
#[tokio::test]
async fn the_form_length_is_judged_before_a_byte_is_read() {
    let rig = Rig::new().await;
    for (what, len, code) in [
        (
            "above max_object_size",
            Some(MAX_OBJECT as u64 + FORM_OVERHEAD + 1),
            "EntityTooLarge",
        ),
        ("no length", None, "MissingContentLength"),
    ] {
        let read = Arc::new(AtomicBool::new(false));
        let flag = read.clone();
        let stream = futures::stream::poll_fn(move |_| {
            flag.store(true, Ordering::SeqCst);
            std::task::Poll::Ready(None::<Result<Bytes, std::io::Error>>)
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}")
                .parse()
                .unwrap(),
        );
        if let Some(len) = len {
            headers.insert(axum::http::header::CONTENT_LENGTH, len.into());
        }
        let err = handle_form_post_upload(
            &rig.state,
            "bkt",
            Some(&rig.iam),
            &headers,
            &axum::http::Extensions::new(),
            stream,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), code, "{what}: {err:?}");
        assert!(!read.load(Ordering::SeqCst), "{what}: the form was read");
    }
}

// ── Source guards ───────────────────────────────────────────────────────

/// No request body under `src/api/` is collected whole with
/// `axum::body::to_bytes`, except the response rewrite, which is bounded
/// by `REWRITE_BODY_CAP`. (The form POST collected its whole body, up to
/// `max_object_size`, before it checked the signature.)
#[test]
fn request_bodies_are_never_collected_whole() {
    let mut offenders = Vec::new();
    for (file, text) in crate::source_scan::prod_sources("src/api") {
        for (n, line) in crate::source_scan::prod_lines(&text) {
            if line.contains("axum::body::to_bytes(") && !line.contains("REWRITE_BODY_CAP") {
                offenders.push(format!("{file}:{n}: {}", line.trim()));
            }
        }
    }
    assert!(offenders.is_empty(), "whole-body reads: {offenders:#?}");
}

/// The form POST reads no part whole (`Field::bytes` / `Field::text`):
/// small fields go through `read_small_field` under its limits, and the
/// file only through `ingest_form_file`.
#[test]
fn no_form_part_is_read_whole() {
    let text =
        crate::source_scan::prod_text(&crate::source_scan::read("src/api/handlers/form_post.rs"));
    for whole in [".bytes().await", ".text().await", "body::to_bytes("] {
        assert!(
            !text.contains(whole),
            "form_post.rs reads a part with {whole}"
        );
    }
    let head = crate::source_scan::prod_fn_text("src/api/handlers/form_post.rs", "read_form_head");
    assert!(
        !head.contains("chunk(") && !head.contains("ingest"),
        "read_form_head reads no byte of the file"
    );
}

/// Signature first, file last: the handler reads the form head, checks the
/// signature and the policy, runs the client-write gates, and only then
/// ingests the file, once.
#[test]
fn the_file_is_ingested_only_after_the_signature_and_the_policy() {
    let file = "src/api/handlers/form_post.rs";
    let body = crate::source_scan::prod_fn_text(file, "handle_form_post_upload");
    let at = |needle: &str| {
        assert_eq!(body.matches(needle).count(), 1, "{needle} once");
        body.find(needle).unwrap()
    };
    let order = [
        "form_body_len(",
        "read_form_head(",
        "authenticate_form_post(",
        "check_client_write_allowed(",
        "check_verified_request(",
        "ensure_bucket_exists(",
        "ingest_form_file(",
        "check_file_len(",
        "ignore_trailing_fields(",
        "store_client_write(",
    ];
    for pair in order.windows(2) {
        assert!(at(pair[0]) < at(pair[1]), "{} before {}", pair[0], pair[1]);
    }
    let text = crate::source_scan::prod_text(&crate::source_scan::read(file));
    assert_eq!(
        text.matches("ingest_form_file(").count(),
        2,
        "ingest_form_file: its definition and the handler's one call"
    );
}
