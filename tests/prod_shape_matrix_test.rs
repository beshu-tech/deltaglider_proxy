// SPDX-License-Identifier: BUSL-1.1

//! Production-shape request matrix: boot a proxy from
//! `tests/fixtures/prod_shape_config.yaml` (the sanitized, structure-true
//! copy of the production config) and drive the request paths production
//! drives. `prod_shape_tests` in `src/config/mod.rs` proves the document
//! PARSES; this file proves the proxy it describes SERVES.
//!
//! The fixture stays as it is except for what a test machine needs: the
//! listen address, the bootstrap hash, backend paths/endpoints, an OIDC
//! issuer that answers "connection refused" at once (discovery fails fast
//! and is logged, as on a box without network), and aliases that keep the
//! real bucket names unique (the S3 variant shares one MinIO).
//!
//! Two variants: the default backend `hetzner-fsn1` is a filesystem
//! backend (PR gate, no MinIO) or MinIO (skipped without it).

use crate::common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use common::{
    admin_http_client, generate_binary, get_bytes, mutate_binary, put_and_get_storage_type, S3Http,
    S3Requests, TestServer, LISTEN_ADDR_PLACEHOLDER,
};
use reqwest::Method;
use serde_yaml::Value;
use std::path::{Path, PathBuf};

const FIXTURE: &str = include_str!("fixtures/prod_shape_config.yaml");

/// The fixture's bootstrap S3 pair (also the `legacy-admin` IAM user).
const ADMIN: (&str, &str) = ("admin", "bootstrap-dummy-secret-000");

/// Where the default backend `hetzner-fsn1` lives.
enum DefaultBackend {
    Filesystem,
    Minio,
}

struct ProdShape {
    server: TestServer,
    admin_http: S3Http,
    /// Filesystem path of the encrypted `local-disk` backend.
    local_disk: PathBuf,
    /// Real (backend) name of the `scratch` bucket, which the test aliases.
    scratch_real: String,
}

fn map_get<'a>(v: &'a mut Value, key: &str) -> &'a mut Value {
    v.get_mut(key)
        .unwrap_or_else(|| panic!("fixture has no `{key}`"))
}

fn set(v: &mut Value, key: &str, value: impl Into<Value>) {
    v.as_mapping_mut()
        .expect("a mapping")
        .insert(Value::from(key), value.into());
}

/// The fixture, adapted to this machine. Pure: no I/O.
fn adapt_fixture(
    data_root: &Path,
    default_backend: &DefaultBackend,
    suffix: &str,
) -> (String, String) {
    let mut doc: Value = serde_yaml::from_str(FIXTURE).expect("fixture parses");

    let advanced = map_get(&mut doc, "advanced");
    set(advanced, "listen_addr", LISTEN_ADDR_PLACEHOLDER);
    set(
        advanced,
        "bootstrap_password_hash",
        common::TEST_BOOTSTRAP_PASSWORD_HASH,
    );

    let access = map_get(&mut doc, "access");
    for p in map_get(access, "auth_providers")
        .as_sequence_mut()
        .expect("auth_providers is a list")
    {
        set(p, "issuer_url", "http://127.0.0.1:9");
    }

    let storage = map_get(&mut doc, "storage");
    for b in map_get(storage, "backends")
        .as_sequence_mut()
        .expect("backends is a list")
    {
        let name = b["name"].as_str().expect("backend name").to_string();
        match (name.as_str(), default_backend) {
            ("hetzner-fsn1", DefaultBackend::Filesystem) => {
                let m = b.as_mapping_mut().unwrap();
                for k in [
                    "endpoint",
                    "region",
                    "force_path_style",
                    "access_key_id",
                    "secret_access_key",
                ] {
                    m.remove(k);
                }
                set(b, "type", "filesystem");
                set(b, "path", data_root.join(&name).display().to_string());
            }
            ("hetzner-fsn1", DefaultBackend::Minio) => {
                set(b, "endpoint", common::minio_endpoint_url());
                set(b, "region", "us-east-1");
                set(b, "access_key_id", common::MINIO_ACCESS_KEY);
                set(b, "secret_access_key", common::MINIO_SECRET_KEY);
            }
            (_, _) if b["type"].as_str() == Some("filesystem") => {
                set(b, "path", data_root.join(&name).display().to_string());
            }
            _ => panic!("fixture backend `{name}` has no test adaptation"),
        }
    }

    let buckets = map_get(storage, "buckets");
    // Unique real names: MinIO is shared by every test of the run. An alias
    // needs an explicit `backend` (the one the default routing picks).
    if matches!(default_backend, DefaultBackend::Minio) {
        let releases = map_get(buckets, "releases");
        set(releases, "backend", "hetzner-fsn1");
        set(releases, "alias", format!("releases-{suffix}"));
    }
    let scratch_real = format!("scratch-{suffix}");
    set(map_get(buckets, "scratch"), "alias", scratch_real.clone());

    (
        serde_yaml::to_string(&doc).expect("adapted fixture serialises"),
        scratch_real,
    )
}

async fn boot(default_backend: DefaultBackend) -> ProdShape {
    let data = tempfile::TempDir::new().expect("temp dir");
    let suffix = common::unique_bucket("ps")
        .trim_start_matches("ps-")
        .to_string();
    let (doc, scratch_real) = adapt_fixture(data.path(), &default_backend, &suffix);
    let local_disk = data.path().join("local-disk");
    let server = TestServer::from_config_document(&doc, data, ADMIN, "releases", Vec::new()).await;
    let s3 = server.s3_client().await;
    for b in ["db-archive", "pippo", "scratch", "downloads"] {
        s3.create_bucket()
            .bucket(b)
            .send()
            .await
            .unwrap_or_else(|e| panic!("create bucket {b}: {e:?}"));
    }
    ProdShape {
        admin_http: server.http(),
        server,
        local_disk,
        scratch_real,
    }
}

fn url(ps: &ProdShape, bucket: &str, key: &str) -> String {
    format!("{}/{}/{}", ps.server.endpoint(), bucket, key)
}

async fn status(client: &impl S3Requests, method: Method, url: &str, body: Option<&[u8]>) -> u16 {
    let mut req = client.s3_request(method, url);
    if let Some(b) = body {
        req = req.body(b.to_vec());
    }
    req.send().await.expect("request sent").status().as_u16()
}

/// `(key, size)` pairs of a ListObjectsV2 response.
fn listed(xml: &str) -> Vec<(String, u64)> {
    xml.split("<Contents>")
        .skip(1)
        .map(|c| {
            let field = |name: &str| {
                let open = format!("<{name}>");
                let start = c.find(&open).expect("field") + open.len();
                let end = c[start..].find('<').expect("field end") + start;
                c[start..end].to_string()
            };
            (field("Key"), field("Size").parse().expect("size"))
        })
        .collect()
}

async fn list(
    client: &impl S3Requests,
    ps: &ProdShape,
    bucket: &str,
    prefix: &str,
) -> (u16, String) {
    let u = format!(
        "{}/{}?list-type=2&prefix={}",
        ps.server.endpoint(),
        bucket,
        prefix
    );
    let resp = client.s3_request(Method::GET, &u).send().await.unwrap();
    let code = resp.status().as_u16();
    (code, resp.text().await.unwrap_or_default())
}

/// Every regular file under `dir` (recursive).
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// PUT/GET/HEAD/LIST/DELETE of a delta pair and a passthrough object on
/// one bucket route. LIST must report LOGICAL sizes (not delta or
/// ciphertext sizes).
async fn object_matrix(ps: &ProdShape, bucket: &str) {
    let http = &ps.admin_http;
    let ep = ps.server.endpoint();
    let base = generate_binary(200_000, 7);
    let v2 = mutate_binary(&base, 0.01);
    let photo = [
        b"PNG-PLAINTEXT-MARKER-".as_slice(),
        &generate_binary(50_000, 8),
    ]
    .concat();
    let objects = [
        ("matrix/app-v1.zip", base.clone(), "application/zip"),
        ("matrix/app-v2.zip", v2.clone(), "application/zip"),
        ("matrix/photo.png", photo.clone(), "image/png"),
    ];
    let mut kinds = Vec::new();
    for (key, body, ct) in &objects {
        kinds.push(put_and_get_storage_type(http, &ep, bucket, key, body.clone(), ct).await);
    }
    assert_eq!(kinds[1], "delta", "{bucket}: the second zip is a delta");
    assert_eq!(kinds[2], "passthrough", "{bucket}: a png is passthrough");

    for (key, body, _) in &objects {
        assert_eq!(
            &get_bytes(http, &ep, bucket, key).await,
            body,
            "{bucket}/{key}: GET returns the PUT bytes"
        );
        let head = http
            .s3_request(Method::HEAD, &url(ps, bucket, key))
            .send()
            .await
            .unwrap();
        assert_eq!(head.status().as_u16(), 200, "{bucket}/{key}: HEAD");
        assert_eq!(
            head.headers()["content-length"].to_str().unwrap(),
            body.len().to_string(),
            "{bucket}/{key}: HEAD reports the logical size"
        );
    }

    let (code, xml) = list(http, ps, bucket, "matrix/").await;
    assert_eq!(code, 200, "{bucket}: LIST: {xml}");
    let mut got = listed(&xml);
    got.sort();
    let mut want: Vec<(String, u64)> = objects
        .iter()
        .map(|(k, b, _)| (k.to_string(), b.len() as u64))
        .collect();
    want.sort();
    assert_eq!(got, want, "{bucket}: LIST keys and logical sizes");

    for (key, _, _) in &objects {
        let del = status(http, Method::DELETE, &url(ps, bucket, key), None).await;
        assert_eq!(del, 204, "{bucket}/{key}: DELETE");
        let get = status(http, Method::GET, &url(ps, bucket, key), None).await;
        assert_eq!(get, 404, "{bucket}/{key}: GET after DELETE");
    }
    let (_, xml) = list(http, ps, bucket, "matrix/").await;
    assert!(
        listed(&xml).is_empty(),
        "{bucket}: LIST after DELETE: {xml}"
    );
}

/// Encrypted `local-disk`: no plaintext of an object reaches the disk.
async fn encrypted_at_rest(ps: &ProdShape) {
    let marker = b"PLAINTEXT-MUST-NOT-REACH-DISK-0123456789".to_vec();
    let body = [marker.as_slice(), &generate_binary(10_000, 9)].concat();
    for bucket in ["db-archive", "scratch"] {
        common::put_object(
            &ps.admin_http,
            &ps.server.endpoint(),
            bucket,
            "enc/secret.bin",
            body.clone(),
            "application/octet-stream",
        )
        .await;
    }
    let mut files = Vec::new();
    files_under(&ps.local_disk, &mut files);
    assert!(
        files
            .iter()
            .any(|f| f.to_string_lossy().contains(&ps.scratch_real)),
        "the aliased bucket is stored under its real name: {files:?}"
    );
    for f in &files {
        let bytes = std::fs::read(f).unwrap();
        assert!(
            !contains(&bytes, &marker),
            "{} holds plaintext on the encrypted backend",
            f.display()
        );
    }
}

/// Declarative IAM users of the fixture: allowed and denied operations.
async fn iam_matrix(ps: &ProdShape) {
    let ep = ps.server.endpoint();
    let user = |k: &str, s: &str| S3Http::signed(k, s);
    let ci = user("ci-uploader", "DummySecretCiUploader00000000");
    let reader = user("api-reader", "DummySecretApiReader00000000");
    let bot = user("build-bot", "DummySecretBuildBot0000000000000000");
    let cust = user("customer-x", "DummySecretCustomerX00000000000000000000");
    let dana = user(
        "AKDUMMY00ADMIN000000",
        "DummySecret/Admin+0000000000000000000000",
    );
    let body: &[u8] = b"iam matrix body";
    let u = |b: &str, k: &str| url(ps, b, k);

    // (who, method, url, expected status)
    let cases: Vec<(&str, &S3Http, Method, String, u16)> = vec![
        (
            "ci-uploader",
            &ci,
            Method::PUT,
            u("releases", "firmware/ci.bin"),
            200,
        ),
        (
            "ci-uploader",
            &ci,
            Method::GET,
            u("releases", "firmware/ci.bin"),
            200,
        ),
        (
            "ci-uploader",
            &ci,
            Method::PUT,
            u("releases", "reports/r.bin"),
            200,
        ),
        (
            "ci-uploader",
            &ci,
            Method::PUT,
            u("releases", "lib/x.bin"),
            403,
        ),
        (
            "ci-uploader",
            &ci,
            Method::DELETE,
            u("releases", "firmware/ci.bin"),
            403,
        ),
        (
            "ci-uploader",
            &ci,
            Method::PUT,
            u("db-archive", "scrap/x.bin"),
            403,
        ),
        (
            "api-reader",
            &reader,
            Method::GET,
            u("releases", "firmware/ci.bin"),
            200,
        ),
        (
            "api-reader",
            &reader,
            Method::PUT,
            u("releases", "firmware/r.bin"),
            403,
        ),
        (
            "api-reader",
            &reader,
            Method::GET,
            u("db-archive", "scrap/x.bin"),
            403,
        ),
        (
            "build-bot",
            &bot,
            Method::GET,
            u("releases", "firmware/ci.bin"),
            200,
        ),
        (
            "build-bot",
            &bot,
            Method::PUT,
            u("releases", "firmware/b.bin"),
            403,
        ),
        (
            "build-bot",
            &bot,
            Method::PUT,
            u("db-archive", "scrap/bot.bin"),
            200,
        ),
        (
            "build-bot",
            &bot,
            Method::DELETE,
            u("db-archive", "scrap/bot.bin"),
            204,
        ),
        (
            "customer-x",
            &cust,
            Method::PUT,
            u("db-archive", "scrap/customers/customer-x/a.txt"),
            200,
        ),
        (
            "customer-x",
            &cust,
            Method::GET,
            u("db-archive", "scrap/customers/customer-x/a.txt"),
            200,
        ),
        (
            "customer-x",
            &cust,
            Method::PUT,
            u("db-archive", "scrap/customers/customer-y/a.txt"),
            403,
        ),
        (
            "customer-x",
            &cust,
            Method::PUT,
            u("db-archive", "scrap/elsewhere.txt"),
            403,
        ),
        (
            "customer-x",
            &cust,
            Method::GET,
            u("releases", "firmware/ci.bin"),
            403,
        ),
        (
            "customer-x",
            &cust,
            Method::DELETE,
            u("db-archive", "scrap/customers/customer-x/a.txt"),
            204,
        ),
        (
            "Dana Admin",
            &dana,
            Method::PUT,
            u("pippo", "admin/a.bin"),
            200,
        ),
        (
            "Dana Admin",
            &dana,
            Method::DELETE,
            u("pippo", "admin/a.bin"),
            204,
        ),
    ];
    let mut wrong = Vec::new();
    for (who, client, method, url, want) in cases {
        let b = (method == Method::PUT).then_some(body);
        let got = status(client, method.clone(), &url, b).await;
        if got != want {
            wrong.push(format!("{who} {method} {url}: want {want}, got {got}"));
        }
    }

    // LIST: prefix conditions (StringLike s3:prefix, `${iam:username}`).
    // A prefix outside the grants answers 403 or a FILTERED 200 (the
    // proxy's list scope); either way no key outside the grants shows.
    for (b, k) in [
        ("releases", "secret/s.bin"),
        ("db-archive", "nightly/n.zip"),
        ("db-archive", "scrap/customers/customer-y/y.txt"),
        ("db-archive", "scrap/customers/customer-x/x.txt"),
    ] {
        common::put_object(&ps.admin_http, &ep, b, k, body.to_vec(), "text/plain").await;
    }
    // (who, client, bucket, prefix, must answer 200, the keys it may show)
    let lists: Vec<(&str, &S3Http, &str, &str, bool, &str)> = vec![
        (
            "build-bot",
            &bot,
            "releases",
            "firmware/",
            true,
            "firmware/",
        ),
        ("build-bot", &bot, "releases", "secret/", false, "\0"),
        (
            "customer-x",
            &cust,
            "db-archive",
            "scrap/customers/customer-x/",
            true,
            "scrap/customers/customer-x/",
        ),
        (
            "customer-x",
            &cust,
            "db-archive",
            "scrap/customers/",
            true,
            "scrap/customers/customer-x/",
        ),
        (
            "customer-x",
            &cust,
            "db-archive",
            "",
            true,
            "scrap/customers/customer-x/",
        ),
        ("customer-x", &cust, "db-archive", "nightly/", false, "\0"),
        (
            "customer-x",
            &cust,
            "db-archive",
            "scrap/customers/customer-y/",
            false,
            "\0",
        ),
    ];
    for (who, client, bucket, prefix, must_200, visible) in lists {
        let (got, xml) = list(client, ps, bucket, prefix).await;
        if got != 200 && (must_200 || got != 403) {
            wrong.push(format!("{who} LIST {bucket}?prefix={prefix}: got {got}"));
        }
        if got == 200 {
            for (k, _) in listed(&xml) {
                if !k.starts_with(visible) {
                    wrong.push(format!("{who} LIST {bucket}?prefix={prefix} shows {k}"));
                }
            }
        }
    }
    let (_, xml) = list(&cust, ps, "db-archive", "scrap/customers/customer-x/").await;
    if !xml.contains("scrap/customers/customer-x/x.txt") {
        wrong.push(format!("customer-x does not see its own key: {xml}"));
    }
    assert!(wrong.is_empty(), "IAM matrix:\n{}", wrong.join("\n"));
}

/// `releases` has `public_prefixes: [firmware/public/]`.
async fn public_prefix_matrix(ps: &ProdShape) {
    let ep = ps.server.endpoint();
    let http = &ps.admin_http;
    let body = b"public firmware".to_vec();
    for key in ["firmware/public/pub.bin", "firmware/private.bin"] {
        common::put_object(
            http,
            &ep,
            "releases",
            key,
            body.clone(),
            "application/octet-stream",
        )
        .await;
    }
    let anon = S3Http::unsigned();
    let resp = anon
        .get(url(ps, "releases", "firmware/public/pub.bin"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "anonymous GET of a public prefix"
    );
    assert_eq!(resp.bytes().await.unwrap().to_vec(), body);
    let mut wrong = Vec::new();
    for (method, u, want) in [
        (
            Method::HEAD,
            url(ps, "releases", "firmware/public/pub.bin"),
            200,
        ),
        (
            Method::GET,
            url(ps, "releases", "firmware/private.bin"),
            403,
        ),
        (
            Method::PUT,
            url(ps, "releases", "firmware/public/new.bin"),
            403,
        ),
        (
            Method::DELETE,
            url(ps, "releases", "firmware/public/pub.bin"),
            403,
        ),
        (Method::GET, url(ps, "db-archive", "enc/secret.bin"), 403),
    ] {
        let b = (method == Method::PUT).then_some(body.as_slice());
        let got = status(&anon, method.clone(), &u, b).await;
        if got != want {
            wrong.push(format!("anonymous {method} {u}: want {want}, got {got}"));
        }
    }
    let (code, xml) = list(&anon, ps, "releases", "firmware/public/").await;
    if code != 200 || !xml.contains("firmware/public/pub.bin") {
        wrong.push(format!("anonymous LIST of the public prefix: {code} {xml}"));
    }
    let (code, xml) = list(&anon, ps, "releases", "firmware/").await;
    if code == 200 && xml.contains("firmware/private.bin") {
        wrong.push(format!(
            "anonymous LIST outside the public prefix shows a key: {xml}"
        ));
    }
    assert!(
        wrong.is_empty(),
        "public prefix matrix:\n{}",
        wrong.join("\n")
    );
}

async fn multipart(ps: &ProdShape, bucket: &str) {
    let s3 = ps.server.s3_client().await;
    let key = "mp/archive.bin";
    let part1 = generate_binary(5 * 1024 * 1024, 11);
    let part2 = generate_binary(1024 * 1024, 12);
    let up = s3
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{bucket}: create MPU: {e:?}"));
    let id = up.upload_id().unwrap();
    let mut parts = Vec::new();
    for (n, body) in [(1, &part1), (2, &part2)] {
        let p = s3
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(id)
            .part_number(n)
            .body(ByteStream::from(body.clone()))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{bucket}: upload part {n}: {e:?}"));
        parts.push(
            CompletedPart::builder()
                .part_number(n)
                .e_tag(p.e_tag().unwrap())
                .build(),
        );
    }
    let done = s3
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build(),
        )
        .send()
        .await
        .unwrap_or_else(|e| panic!("{bucket}: complete MPU: {e:?}"));
    assert!(
        done.e_tag().unwrap_or("").trim_matches('"').ends_with("-2"),
        "{bucket}: multipart ETag: {:?}",
        done.e_tag()
    );
    let got = get_bytes(&ps.admin_http, &ps.server.endpoint(), bucket, key).await;
    assert_eq!(got.len(), part1.len() + part2.len(), "{bucket}: MPU size");
    assert!(got == [part1, part2].concat(), "{bucket}: MPU bytes");
}

async fn copy_across_backends(ps: &ProdShape) {
    let s3 = ps.server.s3_client().await;
    let ep = ps.server.endpoint();
    let body = generate_binary(120_000, 21);
    for (src, dst) in [
        ("releases", "db-archive"),
        ("db-archive", "releases"),
        ("scratch", "pippo"),
    ] {
        common::put_object(
            &ps.admin_http,
            &ep,
            src,
            "copy/src.zip",
            body.clone(),
            "application/zip",
        )
        .await;
        s3.copy_object()
            .bucket(dst)
            .key("copied/from.zip")
            .copy_source(format!("{src}/copy/src.zip"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("copy {src} -> {dst}: {e:?}"));
        assert_eq!(
            get_bytes(&ps.admin_http, &ep, dst, "copied/from.zip").await,
            body,
            "copy {src} -> {dst}: bytes"
        );
    }
}

async fn wait_bucket_job_done(admin: &reqwest::Client, endpoint: &str, bucket: &str) {
    for _ in 0..600 {
        let v: serde_json::Value = admin
            .get(format!("{endpoint}/_/api/admin/jobs/bucket/{bucket}"))
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
    panic!("the job on '{bucket}' did not finish within 60s");
}

async fn newest_job(admin: &reqwest::Client, endpoint: &str, kind: &str) -> serde_json::Value {
    let v: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/jobs"))
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
        .find(|j| j["kind"] == kind)
        .cloned()
        .unwrap_or_else(|| panic!("no {kind} job in {v}"))
}

/// Replication run-now, lifecycle preview, backfill and migrate jobs on
/// the fixture's own rules and buckets.
async fn jobs_matrix(ps: &ProdShape) {
    let ep = ps.server.endpoint();
    let http = &ps.admin_http;
    let admin = admin_http_client(&ep).await;

    // Replication: releases/firmware/ -> pippo/mirror/ (rule is disabled in
    // prod; a run-now is a deliberate one-off).
    let rep_body = generate_binary(30_000, 31);
    common::put_object(
        http,
        &ep,
        "releases",
        "firmware/rep-1.bin",
        rep_body.clone(),
        "application/octet-stream",
    )
    .await;
    let resp = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:mirror-releases-to-dr/run-now"
        ))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "replication run-now: {}",
        resp.status()
    );
    let run = common::wait_for_run(&admin, &ep, "mirror-releases-to-dr").await;
    assert_eq!(
        run["status"].as_str(),
        Some("succeeded"),
        "replication run: {run}"
    );
    assert_eq!(
        get_bytes(http, &ep, "pippo", "mirror/rep-1.bin").await,
        rep_body,
        "the replica holds the source bytes"
    );

    // Lifecycle preview: a fresh nightly build is not 30 days old.
    common::put_object(
        http,
        &ep,
        "db-archive",
        "nightly/build-1.zip",
        generate_binary(10_000, 32),
        "application/zip",
    )
    .await;
    let preview: serde_json::Value = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/lifecycle:expire-old-builds/preview"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preview["status"].as_str(), Some("preview"), "{preview}");
    assert_eq!(preview["objects_affected"].as_i64(), Some(0), "{preview}");

    // Backfill-metadata on an encrypted-backend bucket.
    common::put_object(
        http,
        &ep,
        "downloads",
        "lib/libfoo.so",
        generate_binary(10_000, 33),
        "application/octet-stream",
    )
    .await;
    let resp = admin
        .post(format!("{ep}/_/api/admin/jobs/backfill-metadata"))
        .json(&serde_json::json!({ "buckets": ["downloads"] }))
        .send()
        .await
        .unwrap();
    let code = resp.status();
    let v: serde_json::Value = resp.json().await.unwrap_or_default();
    assert!(code.is_success(), "backfill start: {code} {v}");
    wait_bucket_job_done(&admin, &ep, "downloads").await;
    let job = newest_job(&admin, &ep, "backfill-metadata").await;
    assert_eq!(job["status_raw"], "completed", "backfill job: {job}");
    assert_eq!(job["progress"]["failed"], 0, "backfill job: {job}");

    // Migrate the ALIASED bucket off the encrypted backend to the default.
    let body = generate_binary(40_000, 34);
    common::put_object(
        http,
        &ep,
        "scratch",
        "mig/one.bin",
        body.clone(),
        "application/octet-stream",
    )
    .await;
    let resp = admin
        .post(format!("{ep}/_/api/admin/buckets/scratch/migrate"))
        .json(&serde_json::json!({ "target_backend": "hetzner-fsn1", "delete_source": false }))
        .send()
        .await
        .unwrap();
    let code = resp.status();
    let v: serde_json::Value = resp.json().await.unwrap_or_default();
    assert!(code.is_success(), "migrate start: {code} {v}");
    wait_bucket_job_done(&admin, &ep, "scratch").await;
    let job = newest_job(&admin, &ep, "migrate").await;
    assert_eq!(job["status_raw"], "completed", "migrate job: {job}");
    assert_eq!(
        get_bytes(http, &ep, "scratch", "mig/one.bin").await,
        body,
        "the migrated bucket serves its objects from the new backend"
    );
    let (code, xml) = list(http, ps, "scratch", "mig/").await;
    assert_eq!(code, 200);
    assert_eq!(
        listed(&xml),
        vec![("mig/one.bin".to_string(), body.len() as u64)]
    );
}

async fn run_matrix(ps: &ProdShape) {
    for bucket in ["releases", "db-archive", "scratch"] {
        object_matrix(ps, bucket).await;
    }
    encrypted_at_rest(ps).await;
    iam_matrix(ps).await;
    public_prefix_matrix(ps).await;
    for bucket in ["releases", "db-archive"] {
        multipart(ps, bucket).await;
    }
    copy_across_backends(ps).await;
    jobs_matrix(ps).await;
}

#[tokio::test]
async fn prod_shape_filesystem_serves_the_request_matrix() {
    let ps = boot(DefaultBackend::Filesystem).await;
    run_matrix(&ps).await;
}

#[tokio::test]
async fn prod_shape_minio_serves_the_request_matrix() {
    skip_unless_minio!();
    let ps = boot(DefaultBackend::Minio).await;
    run_matrix(&ps).await;
}

/// The adaptation changes only machine-specific values: every user,
/// group, provider, bucket and rule of the fixture is still there.
#[test]
fn adaptation_keeps_the_fixture_shape() {
    let (doc, _) = adapt_fixture(Path::new("/data"), &DefaultBackend::Filesystem, "x");
    let orig: Value = serde_yaml::from_str(FIXTURE).unwrap();
    let got: Value = serde_yaml::from_str(&doc).unwrap();
    for path in [
        &["access", "iam_users"][..],
        &["access", "iam_groups"],
        &["access", "group_mapping_rules"],
        &["storage", "replication"],
        &["storage", "lifecycle"],
    ] {
        let pick = |v: &Value| path.iter().fold(v.clone(), |v, k| v[*k].clone());
        assert_eq!(pick(&orig), pick(&got), "{path:?} is unchanged");
    }
    let names = |v: &Value| {
        v["storage"]["buckets"]
            .as_mapping()
            .unwrap()
            .keys()
            .map(|k| k.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&orig), names(&got));
    assert_eq!(
        got["storage"]["default_backend"], orig["storage"]["default_backend"],
        "routing is unchanged"
    );
}
