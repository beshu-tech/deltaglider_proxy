// SPDX-License-Identifier: BUSL-1.1

//! The conversion marker on a real CAS backend (MinIO): the encrypting
//! wrapper creates it once and reuses it, closes it only by identity, and
//! the proxy hides it from client listings and refuses client requests on
//! its key. Wrapper-level cases (format, cache, scripted races) are in
//! `src/storage/encrypting/conversion_tests.rs`.

use crate::common;

use common::TestServer;
use deltaglider_proxy::storage::encrypting::conversion::{ByIdentity, MarkerState, Window};
use deltaglider_proxy::storage::{
    EncryptingBackend, EncryptionConfig, EncryptionKey, S3Backend, WitnessGate,
};
use std::sync::Arc;

const TEST_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const MARKER: &str = ".dg/facts/!!conversion";

/// An encrypting wrapper on MinIO, named like the proxy's singleton
/// backend (`default`), so it binds the same storage identity.
async fn wrapper() -> EncryptingBackend<S3Backend> {
    let cfg: deltaglider_proxy::config::BackendConfig = serde_yaml::from_str(&format!(
        "type: s3\nendpoint: \"{}\"\nregion: us-east-1\nforce_path_style: true\n\
         access_key_id: {}\nsecret_access_key: {}\nallow_local: true\n",
        common::minio_endpoint_url(),
        common::MINIO_ACCESS_KEY,
        common::MINIO_SECRET_KEY
    ))
    .unwrap();
    let s3 = S3Backend::new(
        &cfg,
        deltaglider_proxy::storage::NativeEncryptionConfig::None,
    )
    .await
    .unwrap();
    let mut enc = EncryptionConfig::default();
    enc.key = Some(EncryptionKey::from_hex(TEST_KEY).unwrap());
    enc.key_id = Some("test-key".into());
    EncryptingBackend::new(s3, Arc::new(arc_swap::ArcSwap::new(Arc::new(enc))))
        .with_witness("default", Arc::new(WitnessGate::default()))
}

async fn raw_marker(bucket: &str) -> Option<Vec<u8>> {
    let got = common::minio_client()
        .await
        .get_object()
        .bucket(bucket)
        .key(MARKER)
        .send()
        .await
        .ok()?;
    Some(got.body.collect().await.unwrap().into_bytes().to_vec())
}

#[tokio::test]
async fn the_conversion_marker_is_cas_hidden_and_closed_by_identity() {
    skip_unless_minio!();
    let bucket = common::unique_bucket("conversion");
    let server = TestServer::builder()
        .bucket(&bucket)
        .auth("CONVKEY", "CONVSECRET")
        .s3_endpoint(&common::minio_endpoint_url())
        .encryption_key(TEST_KEY)
        .build()
        .await;
    let client = server.s3_client().await;
    client
        .put_object()
        .bucket(&bucket)
        .key("history/a.txt")
        .body(b"a".to_vec().into())
        .send()
        .await
        .unwrap();

    // Create: the cutoff is the server's clock.
    let a = wrapper().await;
    let run = a.open_conversion_secs(&bucket, 7 * 86_400).await.unwrap();
    let Window::Open {
        cutoff, deadline, ..
    } = a.window_of(&bucket).await
    else {
        panic!("open after open_conversion")
    };
    assert!(
        (cutoff - chrono::Utc::now().timestamp()).abs() < 60,
        "{cutoff}"
    );
    assert_eq!(deadline - cutoff, 7 * 86_400);
    let stored = raw_marker(&bucket).await.expect("the marker object");

    // A second create (another node, fresh cache) reuses it: same run, same
    // bytes on MinIO.
    let b = wrapper().await;
    assert_eq!(
        b.open_conversion_secs(&bucket, 3 * 86_400).await.unwrap(),
        run
    );
    assert_eq!(raw_marker(&bucket).await.as_deref(), Some(&stored[..]));
    // MinIO enforces the create condition.
    let second = common::minio_client()
        .await
        .put_object()
        .bucket(&bucket)
        .key(MARKER)
        .if_none_match("*")
        .body(b"other".to_vec().into())
        .send()
        .await;
    assert!(second.is_err(), "If-None-Match: * on an existing marker");

    // Hidden from client listings, at the root and under `.dg/`.
    for prefix in [None, Some(".dg/"), Some(".dg/facts/")] {
        let listed = client
            .list_objects_v2()
            .bucket(&bucket)
            .set_prefix(prefix.map(str::to_string))
            .send()
            .await
            .unwrap();
        let keys: Vec<_> = listed.contents().iter().filter_map(|o| o.key()).collect();
        assert!(!keys.iter().any(|k| k.contains("conversion")), "{keys:?}");
        assert!(
            !listed
                .common_prefixes()
                .iter()
                .filter_map(|p| p.prefix())
                .any(|p| p.starts_with(".dg/f")),
            "{prefix:?}"
        );
    }
    // Refused for client GET, PUT and DELETE on its key.
    let url = format!("{}/{bucket}/{MARKER}", server.endpoint());
    let get = server.http().get(&url).send().await.unwrap();
    assert!(get.status().is_client_error(), "GET {}", get.status());
    let put = server
        .http()
        .put(&url)
        .body("dgc1.forged")
        .send()
        .await
        .unwrap();
    assert!(put.status().is_client_error(), "PUT {}", put.status());
    let del = server.http().delete(&url).send().await.unwrap();
    assert!(del.status().is_client_error(), "DELETE {}", del.status());
    assert_eq!(raw_marker(&bucket).await.as_deref(), Some(&stored[..]));

    // Close by identity: another run id is refused, the marker stays.
    assert_eq!(
        a.close_conversion(&bucket, &[0; 16]).await.unwrap(),
        ByIdentity::NotOurs
    );
    assert!(raw_marker(&bucket).await.is_some());
    // A backend writer re-PUTs a foreign body: the close is NotOurs and
    // the foreign body stays (a lost condition is never "gone").
    common::minio_client()
        .await
        .put_object()
        .bucket(&bucket)
        .key(MARKER)
        .body(b"planted".to_vec().into())
        .send()
        .await
        .unwrap();
    assert_eq!(
        a.close_conversion(&bucket, &run).await.unwrap(),
        ByIdentity::NotOurs
    );
    assert_eq!(raw_marker(&bucket).await.as_deref(), Some(&b"planted"[..]));
    // Our marker back, then the close by identity removes it.
    common::minio_client()
        .await
        .put_object()
        .bucket(&bucket)
        .key(MARKER)
        .body(stored.clone().into())
        .send()
        .await
        .unwrap();
    assert!(matches!(
        a.close_conversion(&bucket, &run).await.unwrap(),
        ByIdentity::Done(None, _)
    ));
    assert_eq!(raw_marker(&bucket).await, None);
    assert_eq!(a.window_of(&bucket).await, Window::Closed);
    // A fresh node reads it closed too.
    assert_eq!(wrapper().await.window_of(&bucket).await, Window::Closed);
}

/// On the filesystem backend the marker and the key claim are files in the
/// bucket's directory (`<bucket>/.dg-conversion`, `<bucket>/.dg-key-claim`),
/// while every object key lives under `<bucket>/deltaspaces/`. So no client
/// key names them: LIST does not show them, a GET of the same name finds
/// no object, a PUT or DELETE of that key acts on an ordinary object
/// elsewhere, and a `..` key is refused. The marker stays byte for byte.
#[tokio::test]
async fn on_the_filesystem_no_client_key_reaches_the_marker_or_the_claim() {
    use deltaglider_proxy::storage::{FilesystemBackend, StorageBackend};
    let server = TestServer::builder()
        .bucket("releases")
        .encryption_key(TEST_KEY)
        .build()
        .await;
    let root = server
        .data_dir()
        .expect("a filesystem server")
        .to_path_buf();
    let fs = FilesystemBackend::new(root.clone()).await.unwrap();
    let mut enc = EncryptionConfig::default();
    enc.key = Some(EncryptionKey::from_hex(TEST_KEY).unwrap());
    enc.key_id = Some("test-key".into());
    let wrapper = EncryptingBackend::new(fs, Arc::new(arc_swap::ArcSwap::new(Arc::new(enc))))
        .with_witness("default", Arc::new(WitnessGate::default()));
    let run = wrapper
        .open_conversion_secs("releases", 7 * 86_400)
        .await
        .unwrap();
    // A client write makes the proxy claim the bucket for its key.
    let put = server
        .http()
        .put(format!("{}/releases/history/a.txt", server.endpoint()))
        .body("a")
        .send()
        .await
        .unwrap();
    assert_eq!(put.status().as_u16(), 200);
    let raw = FilesystemBackend::new(root.clone()).await.unwrap();
    assert!(
        raw.read_key_claim("releases").await.unwrap().is_some(),
        "the proxy wrote its key claim"
    );
    let files = [".dg-conversion", ".dg-key-claim"];
    let snapshot = || {
        files.map(|f| std::fs::read(root.join("releases").join(f)).expect("the bucket-level file"))
    };
    let before = snapshot();

    let list = |s: &TestServer| {
        let client = s.http();
        let url = format!("{}/releases?list-type=2", s.endpoint());
        async move { client.get(&url).send().await.unwrap().text().await.unwrap() }
    };
    let listed = list(&server).await;
    for f in files {
        assert!(!listed.contains(f), "{f} listed: {listed}");
    }
    for f in files {
        let url = format!("{}/releases/{f}", server.endpoint());
        let get = server.http().get(&url).send().await.unwrap();
        assert_eq!(get.status().as_u16(), 404, "GET {f}");
        // A PUT of that key stores an ordinary object under deltaspaces/,
        // never the bucket-level file; a DELETE removes only that object.
        let put = server
            .http()
            .put(&url)
            .body("client bytes")
            .send()
            .await
            .unwrap();
        let status = put.status().as_u16();
        let text = put.text().await.unwrap();
        assert_eq!(status, 200, "PUT {f}: {text}");
        let del = server.http().delete(&url).send().await.unwrap();
        assert!(del.status().is_success(), "DELETE {f}: {}", del.status());
        // A key that climbs out of deltaspaces/ is refused.
        let climb = format!("{}/releases/x/..%2F..%2F{f}", server.endpoint());
        let put = server.http().put(&climb).body("x").send().await.unwrap();
        assert!(
            put.status().is_client_error(),
            "PUT ../../{f}: {}",
            put.status()
        );
    }
    assert_eq!(
        snapshot(),
        before,
        "a client request changed a bucket-level file"
    );
    let listed = list(&server).await;
    for f in files {
        assert!(!listed.contains(f), "{f} listed: {listed}");
    }
    // The window is still this run's.
    let fresh = FilesystemBackend::new(root).await.unwrap();
    let mut enc = EncryptionConfig::default();
    enc.key = Some(EncryptionKey::from_hex(TEST_KEY).unwrap());
    enc.key_id = Some("test-key".into());
    let reader = EncryptingBackend::new(fresh, Arc::new(arc_swap::ArcSwap::new(Arc::new(enc))))
        .with_witness("default", Arc::new(WitnessGate::default()));
    assert!(matches!(reader.window_of("releases").await, Window::Open { run: r, .. } if r == run));
}

// ═══════════════════════════════════════════════════════════════════════
// P3 + P4: clients read through the window, the re-encrypt job converts
// and closes it. Filesystem unless the name ends in `_minio`.
// ═══════════════════════════════════════════════════════════════════════

const KEY_ID: &str = "conv-key-1";
const KEY_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const HISTORY: &[u8] = b"CONVERSION_HISTORY_0123456789";
/// Each object of a job waits this long, so a test acts while it runs.
const STALL: (&str, &str) = ("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "200");

struct Conv {
    server: TestServer,
    admin: reqwest::Client,
    bucket: String,
}

impl Conv {
    /// A plaintext proxy with `n` plaintext objects `h-NN.txt`, then a
    /// restart with the encryption in the config file: no apply, and the
    /// boot's conversion is off here (`DGP_BOOT_PLAINTEXT_CHECK=false`, which
    /// `env` may turn on), so no conversion window and no job (an apply
    /// opens both, see `tests/conversion_apply_test.rs`).
    async fn start(bucket: &str, n: usize, env: &[(&str, &str)], minio: bool) -> Self {
        let mut b = TestServer::builder()
            .bucket(bucket)
            .env("DGP_BOOT_PLAINTEXT_CHECK", "false");
        if minio {
            b = b.s3_endpoint(&common::minio_endpoint_url());
        }
        for (k, v) in env {
            b = b.env(k, v);
        }
        let server = b.build().await;
        let admin = common::admin_http_client(&server.endpoint()).await;
        let mut conv = Self {
            server,
            admin,
            bucket: bucket.to_string(),
        };
        for i in 0..n {
            let (status, _) = conv.put(&format!("h-{i:02}.txt"), &body_of(i)).await;
            assert_eq!(status, 200);
        }
        let path = conv.server.config_path().to_path_buf();
        let mut doc = std::fs::read_to_string(&path).unwrap();
        doc.push_str(&format!(
            "backend_encryption:\n  mode: aes256-gcm-proxy\n  key: \"{TEST_KEY}\"\n  \
             key_id: {KEY_ID}\n"
        ));
        std::fs::write(&path, doc).unwrap();
        conv.server.respawn_with_env(&[]).await;
        conv.admin = common::admin_http_client(&conv.server.endpoint()).await;
        conv
    }

    async fn encrypt(&self, body: serde_json::Value) {
        let resp = self
            .admin
            .put(format!(
                "{}/_/api/admin/config/section/storage",
                self.server.endpoint()
            ))
            .json(&serde_json::json!({ "backend_encryption": body }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        assert!(
            status.is_success(),
            "{status} {}",
            resp.text().await.unwrap()
        );
    }

    fn url(&self, key: &str) -> String {
        format!("{}/{}/{key}", self.server.endpoint(), self.bucket)
    }

    async fn put(&self, key: &str, body: &[u8]) -> (u16, String) {
        let r = self
            .server
            .http()
            .put(self.url(key))
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn get(&self, key: &str) -> (u16, Vec<u8>) {
        let r = self.server.http().get(self.url(key)).send().await.unwrap();
        (r.status().as_u16(), r.bytes().await.unwrap().to_vec())
    }

    async fn head(&self, key: &str) -> u16 {
        let r = self.server.http().head(self.url(key)).send().await.unwrap();
        r.status().as_u16()
    }

    async fn range(&self, key: &str, spec: &str) -> (u16, Vec<u8>) {
        let r = self
            .server
            .http()
            .get(self.url(key))
            .header("range", spec)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.bytes().await.unwrap().to_vec())
    }

    async fn reencrypt(&self, open_window: bool) -> i64 {
        let v: serde_json::Value = self
            .admin
            .post(format!(
                "{}/_/api/admin/jobs/reencrypt",
                self.server.endpoint()
            ))
            .json(&serde_json::json!({ "buckets": [self.bucket], "open_window": open_window }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["started"][0]["job_id"]
            .as_i64()
            .unwrap_or_else(|| panic!("no job: {v}"))
    }

    async fn job(&self, id: i64) -> serde_json::Value {
        let v: serde_json::Value = self
            .admin
            .get(format!("{}/_/api/admin/jobs", self.server.endpoint()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == format!("maintenance:{id}"))
            .cloned()
            .unwrap_or_else(|| panic!("job {id}: {v}"))
    }

    async fn wait_done(&self) {
        for _ in 0..1200 {
            let v: serde_json::Value = self
                .admin
                .get(format!(
                    "{}/_/api/admin/jobs/bucket/{}",
                    self.server.endpoint(),
                    self.bucket
                ))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if v["active"].is_null() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the job on {} did not finish", self.bucket);
    }

    async fn failures(&self, id: i64) -> Vec<(String, String)> {
        let v: serde_json::Value = self
            .admin
            .get(format!(
                "{}/_/api/admin/jobs/maintenance:{id}/failures?limit=500",
                self.server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["failures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["object_key"].as_str().unwrap().to_string(),
                    f["error"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    async fn audit(&self, action: &str) -> Vec<String> {
        let v: serde_json::Value = self
            .admin
            .get(format!(
                "{}/_/api/admin/audit?limit=500",
                self.server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v.get("entries")
            .and_then(|e| e.as_array())
            .or_else(|| v.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter(|e| e["action"] == action && e["bucket"] == self.bucket.as_str())
            .map(|e| e["target"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// An encrypting wrapper on the proxy's own storage (another node of
    /// the same backend), keyed like the proxy.
    async fn node(&self, key_hex: &str) -> Node {
        let mut enc = EncryptionConfig::default();
        enc.key = Some(EncryptionKey::from_hex(key_hex).unwrap());
        enc.key_id = Some(KEY_ID.into());
        let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(enc)));
        match self.server.data_dir() {
            Some(root) => Node::Fs(
                EncryptingBackend::new(
                    deltaglider_proxy::storage::FilesystemBackend::new(root.to_path_buf())
                        .await
                        .unwrap(),
                    cfg,
                )
                .with_witness("default", Arc::new(WitnessGate::default())),
            ),
            None => Node::S3(wrapper().await),
        }
    }

    fn root(&self) -> std::path::PathBuf {
        self.server.data_dir().expect("filesystem").to_path_buf()
    }

    fn marker_file(&self) -> std::path::PathBuf {
        self.root().join(&self.bucket).join(".dg-conversion")
    }

    /// How many of `h-00..h-{n}` are still plaintext on disk.
    fn plaintext_left(&self, n: usize) -> usize {
        (0..n)
            .filter(|i| {
                std::fs::read(self.blob(&format!("h-{i:02}.txt")))
                    .is_ok_and(|b| b.windows(HISTORY.len()).any(|w| w == HISTORY))
            })
            .count()
    }

    fn blob(&self, key: &str) -> std::path::PathBuf {
        self.root().join(&self.bucket).join("deltaspaces").join(key)
    }

    /// A plaintext file written on the backend, past the proxy, stored at
    /// `at`.
    fn plant(&self, key: &str, body: &[u8], at: std::time::SystemTime) {
        let path = self.blob(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }
}

enum Node {
    Fs(EncryptingBackend<deltaglider_proxy::storage::FilesystemBackend>),
    S3(EncryptingBackend<S3Backend>),
}

impl Node {
    async fn open(&self, bucket: &str) -> [u8; 16] {
        match self {
            Node::Fs(w) => w.open_conversion_secs(bucket, 7 * 86_400).await.unwrap(),
            Node::S3(w) => w.open_conversion_secs(bucket, 7 * 86_400).await.unwrap(),
        }
    }
    async fn status(&self, bucket: &str) -> MarkerState {
        match self {
            Node::Fs(w) => w.conversion_status(bucket).await.unwrap(),
            Node::S3(w) => w.conversion_status(bucket).await.unwrap(),
        }
    }
}

fn body_of(i: usize) -> Vec<u8> {
    [HISTORY, format!(" object {i}").as_bytes()].concat()
}

fn ours(state: &MarkerState) -> Option<deltaglider_proxy::storage::encrypting::conversion::Fields> {
    match state {
        MarkerState::Ours { fields, .. } => Some(*fields),
        _ => None,
    }
}

/// (a) A bucket with plaintext history turns on encryption without
/// `plaintext_reads: serve`: its history is refused until a conversion
/// opens; then GET, HEAD and range reads of it succeed, before and during
/// the job; the job converts everything and closes the window.
#[tokio::test]
async fn clients_read_the_history_through_the_window_while_the_job_converts() {
    const N: usize = 24;
    let c = Conv::start("convread", N, &[STALL], false).await;
    let (status, _) = c.get("h-00.txt").await;
    assert_eq!(status, 500, "no window: refused");

    c.node(TEST_KEY).await.open(&c.bucket).await;
    // The proxy cached the window as closed: the refusal reads it again.
    assert_eq!(c.get("h-00.txt").await, (200, body_of(0)));
    assert_eq!(c.head("h-01.txt").await, 200);
    assert_eq!(
        c.range("h-02.txt", "bytes=0-9").await,
        (206, body_of(2)[..10].to_vec())
    );
    assert_eq!(
        c.range("h-02.txt", "bytes=5-").await,
        (206, body_of(2)[5..].to_vec())
    );

    let id = c.reencrypt(true).await;
    // During the job: every object reads, plaintext or converted.
    let mut seen_mid = false;
    for round in 0..200 {
        let left = c.plaintext_left(N);
        if left == 0 {
            break;
        }
        seen_mid |= left < N;
        let i = round % N;
        assert_eq!(
            c.get(&format!("h-{i:02}.txt")).await,
            (200, body_of(i)),
            "{left} plaintext left"
        );
    }
    assert!(seen_mid, "the reads ran while the job converted");
    c.wait_done().await;
    let job = c.job(id).await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(c.plaintext_left(N), 0);
    assert_eq!(
        c.node(TEST_KEY).await.status(&c.bucket).await,
        MarkerState::Absent
    );
    assert!(!c.marker_file().exists());
    assert_eq!(c.audit("encryption_conversion_closed").await.len(), 1);
    for i in 0..N {
        assert_eq!(c.get(&format!("h-{i:02}.txt")).await, (200, body_of(i)));
    }
}

/// (b) A plaintext object planted on the backend after the cutoff plus the
/// grace (the real 300 s: its stored time is an hour ahead) is refused for
/// clients while the history reads; the job converts it, and lists it as
/// adopted after the cutoff (failure ring, run summary, audit), then
/// closes the window.
#[tokio::test]
async fn a_plant_after_the_cutoff_is_refused_for_clients_and_adopted_by_the_job() {
    const N: usize = 4;
    let c = Conv::start("convplant", N, &[], false).await;
    c.node(TEST_KEY).await.open(&c.bucket).await;
    let planted = b"PLANTED_AFTER_THE_CUTOFF";
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    c.plant("planted.txt", planted, later);
    // Inside the grace (stored 100 s after the cutoff): still history.
    let inside = std::time::SystemTime::now() + std::time::Duration::from_secs(100);
    c.plant("in-grace.txt", b"IN_GRACE", inside);

    assert_eq!(c.get("h-00.txt").await, (200, body_of(0)));
    assert_eq!(c.get("in-grace.txt").await, (200, b"IN_GRACE".to_vec()));
    let (status, body) = c.get("planted.txt").await;
    // A client sees a plain 500; the log has the refusal's text (its words
    // are checked in `read_rule_tests.rs`).
    assert_eq!(status, 500, "{}", String::from_utf8_lossy(&body));
    assert_eq!(c.head("planted.txt").await, 500);

    let id = c.reencrypt(true).await;
    c.wait_done().await;
    let job = c.job(id).await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["progress"]["failed"], 0, "{job}");
    let note = job["last_error"].as_str().unwrap_or_default();
    assert!(
        note.contains("1 object(s) adopted after the conversion's cutoff"),
        "{job}"
    );
    let failures = c.failures(id).await;
    assert_eq!(
        failures
            .iter()
            .filter(|(_, e)| e.starts_with("adopted_after_cutoff"))
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["planted.txt"],
        "{failures:?}"
    );
    assert_eq!(
        c.audit("encryption_conversion_adopted_after_cutoff")
            .await
            .len(),
        1
    );
    assert!(c.audit("encryption_conversion_closed").await[0].contains("1 object(s) adopted"));
    // Converted: now sealed, and it reads.
    assert_eq!(c.get("planted.txt").await, (200, planted.to_vec()));
    assert!(!std::fs::read(c.blob("planted.txt"))
        .unwrap()
        .windows(planted.len())
        .any(|w| w == planted));
    assert_eq!(
        c.node(TEST_KEY).await.status(&c.bucket).await,
        MarkerState::Absent
    );
}

/// (c) `open_window: true` opens the marker before the job; the job's
/// success closes it. Re-planting the old marker body afterwards (a
/// replay) opens the OLD window again, whose cutoff is in the past: a
/// plaintext object planted now is still refused.
#[tokio::test]
async fn a_replayed_marker_does_not_serve_a_new_plant() {
    const N: usize = 8;
    let c = Conv::start(
        "convreplay",
        N,
        &[STALL, ("DGP_TEST_CONVERSION_GRACE_SECS", "0")],
        false,
    )
    .await;
    let id = c.reencrypt(true).await;
    // The marker is written before the job is queued.
    let old_body = std::fs::read(c.marker_file()).expect("the marker of open_window");
    assert_eq!(c.audit("encryption_conversion_opened").await.len(), 1);
    c.wait_done().await;
    assert_eq!(c.job(id).await["status"], "succeeded");
    assert!(!c.marker_file().exists(), "closed on success");

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    c.plant("late.txt", b"LATE_PLANT", std::time::SystemTime::now());
    std::fs::write(c.marker_file(), &old_body).unwrap();
    assert!(
        ours(&c.node(TEST_KEY).await.status(&c.bucket).await).is_some(),
        "the replayed marker opens"
    );
    let (status, _) = c.get("late.txt").await;
    assert_eq!(status, 500, "stored after the replayed cutoff: refused");
    // The converted history reads (sealed now).
    assert_eq!(c.get("h-00.txt").await, (200, body_of(0)));
}

/// (d) A graceful stop mid-walk leaves the marker; clients still read the
/// rest of the history while the job is down; the resume extends the
/// deadline by identity (the cutoff stays) and finishes, then closes it.
#[tokio::test]
async fn a_stopped_job_keeps_the_window_and_the_resume_extends_it() {
    const N: usize = 60;
    let mut c = Conv::start("convresume", N, &[STALL], false).await;
    let id = c.reencrypt(true).await;
    let before = ours(&c.node(TEST_KEY).await.status(&c.bucket).await).expect("opened");
    for _ in 0..400 {
        if c.plaintext_left(N) < N {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    c.server.terminate();
    let left = c.plaintext_left(N);
    assert!(
        left > 0 && left < N,
        "stopped mid-walk ({left} of {N} left)"
    );
    let at_stop = ours(&c.node(TEST_KEY).await.status(&c.bucket).await).expect("kept");
    assert_eq!(at_stop.cutoff, before.cutoff);
    assert_eq!(at_stop.run, before.run);

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    // Boot without the stall: the job resumes and finishes. Clients read
    // a plaintext object at once.
    c.server
        .respawn_with_env(&[
            (STALL.0, "0"),
            (
                "DGP_BOOTSTRAP_PASSWORD_HASH",
                common::TEST_BOOTSTRAP_PASSWORD_HASH,
            ),
        ])
        .await;
    c.admin = common::admin_http_client(&c.server.endpoint()).await;
    c.wait_done().await;
    let job = c.job(id).await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(c.plaintext_left(N), 0);
    let extended = c.audit("encryption_conversion_extended").await;
    assert!(!extended.is_empty(), "the resume extended the deadline");
    let cutoff = format!("cutoff {} kept", before.cutoff);
    assert!(extended.iter().all(|t| t.contains(&cutoff)), "{extended:?}");
    let last = extended[0]
        .rsplit("-> ")
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    assert!(
        last > before.deadline,
        "{extended:?} vs {}",
        before.deadline
    );
    assert!(!c.marker_file().exists());
    for i in 0..N {
        assert_eq!(c.get(&format!("h-{i:02}.txt")).await, (200, body_of(i)));
    }
}

/// (d') A cancel keeps the window: clients still read the history.
#[tokio::test]
async fn a_cancelled_job_keeps_the_window() {
    const N: usize = 40;
    let c = Conv::start("convcancel", N, &[STALL], false).await;
    let id = c.reencrypt(true).await;
    let resp = c
        .admin
        .post(format!(
            "{}/_/api/admin/jobs/maintenance:{id}/cancel",
            c.server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    c.wait_done().await;
    assert_eq!(c.job(id).await["status"], "cancelled");
    assert!(c.plaintext_left(N) > 0);
    assert!(ours(&c.node(TEST_KEY).await.status(&c.bucket).await).is_some());
    for i in 0..N {
        assert_eq!(c.get(&format!("h-{i:02}.txt")).await, (200, body_of(i)));
    }
}

/// (e) A foreign body PUT over the marker while the job runs: the close
/// is blocked, audited, and the run does not report success.
#[tokio::test]
async fn a_foreign_re_put_blocks_the_close_and_the_run_reports_it() {
    const N: usize = 24;
    let c = Conv::start("convforeign", N, &[STALL], false).await;
    let id = c.reencrypt(true).await;
    for _ in 0..400 {
        if c.plaintext_left(N) < N {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    std::fs::write(c.marker_file(), b"planted").unwrap();
    c.wait_done().await;
    let job = c.job(id).await;
    assert_eq!(job["status"], "completed_with_errors", "{job}");
    let failures = c.failures(id).await;
    assert!(
        failures
            .iter()
            .any(|(k, e)| k == ".dg/facts/!!conversion" && e.contains("did not close")),
        "{failures:?}"
    );
    assert_eq!(
        c.audit("encryption_conversion_close_blocked").await.len(),
        1
    );
    assert_eq!(std::fs::read(c.marker_file()).unwrap(), b"planted");
    assert_eq!(c.plaintext_left(N), 0, "the objects are converted anyway");
}

/// (f) A key rotation during the window keeps it (the marker opens with
/// the keyring): history still reads, and the job closes it.
#[tokio::test]
async fn a_key_rotation_mid_window_keeps_it() {
    const N: usize = 6;
    let c = Conv::start("convrotate", N, &[], false).await;
    c.node(TEST_KEY).await.open(&c.bucket).await;
    assert_eq!(c.get("h-00.txt").await, (200, body_of(0)));
    c.encrypt(serde_json::json!({
        "mode": "aes256-gcm-proxy", "key": KEY_B, "key_id": "conv-key-2",
        "legacy_key": TEST_KEY, "legacy_key_id": KEY_ID,
    }))
    .await;
    // The apply rebuilt the engine (fresh cache): the window still opens.
    assert_eq!(c.get("h-01.txt").await, (200, body_of(1)));
    let id = c.reencrypt(true).await;
    c.wait_done().await;
    assert_eq!(c.job(id).await["status"], "succeeded");
    assert!(!c.marker_file().exists());
    for i in 0..N {
        assert_eq!(c.get(&format!("h-{i:02}.txt")).await, (200, body_of(i)));
    }
}

/// (g) A metadata backfill is refused while a marker exists (it would move
/// the stored time of the history past the cutoff), and accepted after.
#[tokio::test]
async fn backfill_is_refused_while_a_marker_exists() {
    let c = Conv::start("convbackfill", 2, &[], false).await;
    let backfill = || async {
        c.admin
            .post(format!(
                "{}/_/api/admin/jobs/backfill-metadata",
                c.server.endpoint()
            ))
            .json(&serde_json::json!({ "buckets": [c.bucket] }))
            .send()
            .await
            .unwrap()
    };
    c.node(TEST_KEY).await.open(&c.bucket).await;
    let resp = backfill().await;
    assert_eq!(resp.status().as_u16(), 409);
    let text = resp.text().await.unwrap();
    assert!(text.contains("being converted to encryption"), "{text}");
    let id = c.reencrypt(true).await;
    c.wait_done().await;
    assert_eq!(c.job(id).await["status"], "succeeded");
    let resp = backfill().await;
    assert!(resp.status().is_success(), "{}", resp.status());
}

/// A rewrite that fails keeps the window open (the run settles with
/// errors, clients still read the object); the next run converts it and
/// closes the window.
#[tokio::test]
async fn a_failed_rewrite_keeps_the_window() {
    use std::os::unix::fs::PermissionsExt;
    const N: usize = 4;
    let c = Conv::start("convfail", N, &[], false).await;
    let blocked = c.blob("h-01.txt");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&blocked).is_ok() {
        // Root reads any file: this check cannot fail a rewrite here.
        eprintln!("skipped: the test runs as root");
        return;
    }
    let id = c.reencrypt(true).await;
    c.wait_done().await;
    let job = c.job(id).await;
    assert_eq!(job["status"], "completed_with_errors", "{job}");
    let failures = c.failures(id).await;
    assert!(
        failures
            .iter()
            .any(|(k, e)| k == "h-01.txt" && e.starts_with("rewrite_failed")),
        "{failures:?}"
    );
    assert!(
        ours(&c.node(TEST_KEY).await.status(&c.bucket).await).is_some(),
        "kept open"
    );
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(c.get("h-01.txt").await, (200, body_of(1)), "history reads");
    let id = c.reencrypt(true).await;
    c.wait_done().await;
    assert_eq!(c.job(id).await["status"], "succeeded");
    assert!(!c.marker_file().exists());
}

/// (a) + (b) on MinIO: the cutoff is MinIO's clock and `Last-Modified` the
/// stored time (grace 0 here, so a plant one second later is past it).
#[tokio::test]
async fn history_reads_and_a_plant_is_adopted_minio() {
    skip_unless_minio!();
    let bucket = common::unique_bucket("convminio");
    let c = Conv::start(&bucket, 4, &[("DGP_TEST_CONVERSION_GRACE_SECS", "0")], true).await;
    assert_eq!(c.get("h-00.txt").await.0, 500, "no window");
    // Times are whole seconds and the grace is 0: the history must be
    // stored a second before the cutoff.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    c.node(TEST_KEY).await.open(&bucket).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    common::minio_client()
        .await
        .put_object()
        .bucket(&bucket)
        .key("planted.txt")
        .body(b"PLANTED".to_vec().into())
        .send()
        .await
        .unwrap();
    assert_eq!(c.get("h-00.txt").await, (200, body_of(0)));
    assert_eq!(c.head("h-01.txt").await, 200);
    assert_eq!(
        c.range("h-02.txt", "bytes=0-9").await,
        (206, body_of(2)[..10].to_vec())
    );
    assert_eq!(c.get("planted.txt").await.0, 500, "after the cutoff");

    let id = c.reencrypt(true).await;
    c.wait_done().await;
    let job = c.job(id).await;
    assert_eq!(job["status"], "succeeded", "{job}");
    let adopted: Vec<_> = c
        .failures(id)
        .await
        .into_iter()
        .filter(|(_, e)| e.starts_with("adopted_after_cutoff"))
        .map(|(k, _)| k)
        .collect();
    assert_eq!(adopted, ["planted.txt"]);
    assert_eq!(c.get("planted.txt").await, (200, b"PLANTED".to_vec()));
    assert_eq!(
        c.node(TEST_KEY).await.status(&bucket).await,
        MarkerState::Absent
    );
}

/// Review S1: a backend writer plants an object after the cutoff, then
/// deletes the marker before the job starts. The job still reports the
/// plant as adopted: in a bucket that encrypts, every unsealed object that
/// a client could not read is reported, with or without a marker.
#[tokio::test]
async fn a_deleted_marker_never_makes_the_job_adopt_silently() {
    const N: usize = 4;
    let c = Conv::start("convsilent", N, &[], false).await;
    c.node(TEST_KEY).await.open(&c.bucket).await;
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    c.plant("planted.txt", b"PLANTED_AFTER_THE_CUTOFF", later);
    assert_eq!(c.get("planted.txt").await.0, 500, "refused for clients");
    // The backend writer closes the window ("availability only").
    std::fs::remove_file(c.marker_file()).unwrap();
    let id = c.reencrypt(false).await;
    c.wait_done().await;
    let failures = c.failures(id).await;
    assert!(
        failures
            .iter()
            .any(|(k, e)| k == "planted.txt" && e.starts_with("adopted_after_cutoff")),
        "the plant was converted with no adoption report: {failures:?}"
    );
    assert!(
        !c.audit("encryption_conversion_adopted_after_cutoff")
            .await
            .is_empty(),
        "audited"
    );
}

/// Review S2: a replayed marker whose deadline passed is NOT extended by a
/// job that did not open it (a plain re-encrypt, a key rotation), so an
/// object with an old stored time (a version restore) is not served and
/// sealed as history without a report.
#[tokio::test]
async fn a_job_without_a_run_never_extends_a_replayed_marker() {
    const N: usize = 4;
    let c = Conv::start(
        "convreopen",
        N,
        &[
            ("DGP_TEST_CONVERSION_GRACE_SECS", "0"),
            ("DGP_TEST_CONVERSION_WINDOW_SECS", "3"),
        ],
        false,
    )
    .await;
    let id = c.reencrypt(true).await;
    let old_body = std::fs::read(c.marker_file()).expect("the marker of open_window");
    c.wait_done().await;
    assert_eq!(c.job(id).await["status"], "succeeded");
    assert!(!c.marker_file().exists(), "closed on success");
    let extended_before = c.audit("encryption_conversion_extended").await.len();
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    // A rollback to an old plaintext object (old stored time).
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    c.plant("rolled.txt", b"ROLLED_BACK", old);
    // The backend writer replays the expired marker.
    std::fs::write(c.marker_file(), &old_body).unwrap();
    assert_eq!(c.get("rolled.txt").await.0, 500, "expired replay: closed");
    // A routine re-encrypt job (no open_window).
    let id2 = c.reencrypt(false).await;
    c.wait_done().await;
    assert_eq!(
        c.audit("encryption_conversion_extended").await.len(),
        extended_before,
        "a replayed expired marker was extended by a job that did not open it"
    );
    assert!(
        c.marker_file().exists(),
        "a job without a run never closes a marker"
    );
    let failures = c.failures(id2).await;
    assert!(
        failures
            .iter()
            .any(|(k, e)| k == "rolled.txt" && e.starts_with("adopted_after_cutoff")),
        "the rolled-back object is reported: {failures:?}"
    );
}

/// Review X2: `POST /jobs/reencrypt {open_window: true}` on a bucket with
/// an active job opens no window: the request fails before the open, so
/// no marker is left for a job that does not exist.
#[tokio::test]
async fn open_window_with_an_active_job_leaves_no_marker() {
    const N: usize = 6;
    let c = Conv::start("convbusy", N, &[STALL], false).await;
    let first = c.reencrypt(false).await;
    let v: serde_json::Value = c
        .admin
        .post(format!(
            "{}/_/api/admin/jobs/reencrypt",
            c.server.endpoint()
        ))
        .json(&serde_json::json!({ "buckets": [c.bucket], "open_window": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["started"], serde_json::json!([]), "{v}");
    assert!(
        v["errors"][0]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("already active"),
        "{v}"
    );
    assert!(
        !c.marker_file().exists(),
        "no window for a job that is not queued"
    );
    c.wait_done().await;
    assert_eq!(c.job(first).await["status"], "succeeded");
}

/// Review X1: a restart-based deployment (operator, Helm, GitOps) adds the
/// encryption to the config file. The boot finds plaintext, no key claim
/// and no marker: it opens the window and queues the job of its run.
/// Clients read the history meanwhile, the job converts everything and
/// closes the window, and the next restart starts nothing.
#[tokio::test]
async fn a_restart_with_encryption_in_the_file_starts_the_conversion() {
    const N: usize = 6;
    let mut c = Conv::start(
        "convboot",
        N,
        &[
            ("DGP_BOOT_PLAINTEXT_CHECK", "true"),
            // Long enough that the reads below run while the job does.
            ("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "2000"),
        ],
        false,
    )
    .await;
    // The boot runs in the background: the window opens shortly (30 s at
    // most on a loaded host).
    let mut audits = Vec::new();
    for _ in 0..600 {
        audits = c.audit("encryption_conversion_opened").await;
        if !audits.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(audits.len(), 1, "the boot opened the window: {audits:?}");
    assert!(c.plaintext_left(N) > 0, "the job still runs");
    // History reads while the job converts.
    for i in 0..N {
        assert_eq!(c.get(&format!("h-{i:02}.txt")).await, (200, body_of(i)));
    }
    c.wait_done().await;
    assert_eq!(c.plaintext_left(N), 0, "the job converted everything");
    assert!(!c.marker_file().exists(), "the job closed the window");
    assert_eq!(c.audit("encryption_conversion_closed").await.len(), 1);
    for i in 0..N {
        assert_eq!(c.get(&format!("h-{i:02}.txt")).await, (200, body_of(i)));
    }
    // A backend writer plants plaintext and deletes the key claim (its
    // body is not sealed). The next restart still opens no window: the
    // bucket holds encrypted objects, so the plant is not history.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    c.plant("planted.txt", b"PLANTED", old);
    let claim = c.marker_file().with_file_name(".dg-key-claim");
    assert!(claim.exists(), "the job's writes claimed the bucket");
    std::fs::remove_file(&claim).unwrap();
    c.server.respawn_with_env(&[]).await;
    c.admin = common::admin_http_client(&c.server.endpoint()).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(!c.marker_file().exists(), "no window over a plant");
    assert_eq!(c.get("planted.txt").await.0, 500);
    assert_eq!(c.audit("encryption_conversion_opened").await.len(), 0);
}
