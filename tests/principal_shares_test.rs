// SPDX-License-Identifier: BUSL-1.1

//! Shares of the shared budgets, through the whole request pipeline: a
//! principal with stalled downloads or many open uploads leaves the other
//! principals their share, and one principal alone can still use most of a
//! budget. The rules themselves are unit-tested in `budget_share`,
//! `spool::read_share_tests` and `multipart::tests`; this file proves that
//! the pipeline wires them (principal of the request, stall guard, 503 text).

use crate::common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use common::{
    admin_http_client, generate_binary, get_iam_version, mutate_binary, sign_raw, sign_raw_with,
    wait_for_iam_rebuild, S3Http, TestServer,
};
use reqwest::Method;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

const MIB: usize = 1024 * 1024;

struct User {
    key: String,
    secret: String,
    http: S3Http,
}

/// A server with IAM users, a small spool at `spool`, and the extra `env`.
async fn server(spool: &tempfile::TempDir, spool_mib: usize, env: &[(&str, &str)]) -> TestServer {
    let mut builder = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .env("DGP_SPOOL_DIR", &spool.path().display().to_string())
        .env("DGP_SPOOL_MAX_BYTES", &(spool_mib * MIB).to_string())
        .env("DGP_SPOOL_THRESHOLD_BYTES", &MIB.to_string())
        .env("DGP_SPOOL_ACQUIRE_TIMEOUT_SECS", "60")
        // A finished or cut download leaves no cached file: the gauge counts what streams hold.
        .env("DGP_RANGE_SPOOL_TTL_SECS", "0")
        .env("DGP_DISK_MONITOR_INTERVAL_SECS", "1");
    for (k, v) in env {
        builder = builder.env(k, v);
    }
    builder.build().await
}

async fn user(server: &TestServer, name: &str) -> User {
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let before = get_iam_version(&admin, &ep).await;
    let resp = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&json!({
            "name": name,
            "permissions": [{ "effect": "Allow", "actions": ["*"], "resources": ["*"] }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create user {name}");
    let body: serde_json::Value = resp.json().await.unwrap();
    wait_for_iam_rebuild(&admin, &ep, before).await;
    let key = body["access_key_id"].as_str().unwrap().to_string();
    let secret = body["secret_access_key"].as_str().unwrap().to_string();
    User {
        http: S3Http::signed(&key, &secret),
        key,
        secret,
    }
}

fn url(server: &TestServer, key: &str) -> String {
    format!("{}/{}/{}", server.endpoint(), server.bucket(), key)
}

async fn put(server: &TestServer, who: &User, key: &str, body: Vec<u8>) -> u16 {
    who.http
        .request(Method::PUT, url(server, key))
        .body(body)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// A GET of `key` over raw TCP whose body is never read: the proxy writes
/// until the socket buffers are full, then its body stream waits. The status
/// of the answer is read from its first line.
async fn stalled_get(server: &TestServer, who: &User, key: &str) -> (tokio::net::TcpStream, u16) {
    let ep = server.endpoint();
    let host = ep.trim_start_matches("http://").to_string();
    let target = url(server, key);
    let mut req = format!(
        "GET /{}/{} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n",
        server.bucket(),
        key
    );
    for (k, v) in sign_raw(Method::GET, &target, &who.key, &who.secret) {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut stream = socket.connect(host.parse().unwrap()).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let status = read_status(&mut stream).await;
    (stream, status)
}

/// The status code of the response on `stream`: its first 12 bytes.
async fn read_status(stream: &mut tokio::net::TcpStream) -> u16 {
    use tokio::io::AsyncReadExt;
    let mut line = [0u8; 12];
    tokio::time::timeout(Duration::from_secs(60), stream.read_exact(&mut line))
        .await
        .expect("an answer within a minute")
        .expect("a status line");
    String::from_utf8_lossy(&line[9..12])
        .parse()
        .expect("a status code")
}

/// `deltaglider_spool_used_bytes`: the spool budget in use (the monitor
/// sets it every second here).
async fn spool_used(server: &TestServer) -> u64 {
    let text = reqwest::Client::new()
        .get(format!("{}/_/metrics", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix("deltaglider_spool_used_bytes "))
        .and_then(|v| v.trim().parse().ok())
        .expect("the spool gauge")
}

async fn wait_for_spool(server: &TestServer, what: &str, ok: impl Fn(u64) -> bool) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let used = spool_used(server).await;
        if ok(used) {
            return used;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: the spool gauge stays at {} MiB",
            used / MIB as u64
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// An 8 MiB baseline and a delta of it, written by `who`: GETs of the delta
/// stream from a spool file of 8 MiB.
async fn seed_delta(server: &TestServer, who: &User, folder: &str, mib: usize) -> Vec<u8> {
    let base = generate_binary(mib * MIB, 11);
    assert_eq!(
        put(server, who, &format!("{folder}/base.zip"), base.clone()).await,
        200
    );
    let v1 = mutate_binary(&base, 0.001);
    assert_eq!(
        put(server, who, &format!("{folder}/v1.zip"), v1.clone()).await,
        200
    );
    v1
}

/// M opens downloads of large delta objects and reads nothing. D's large PUT
/// needs more spool than M leaves free, so it waits; M's bodies are cut after
/// the stall window and the spool is whole again, while M's sockets are
/// still open.
#[tokio::test]
async fn stalled_downloads_end_at_the_stall_window_and_free_the_spool_for_others() {
    common::expect_throttling_only(
        "a download over the share, or a fill without budget, is 503 SlowDown",
    );
    let spool = tempfile::tempdir().unwrap();
    let server = server(&spool, 64, &[("DGP_RESPONSE_STALL_SECS", "2")]).await;
    let (m, d) = (user(&server, "mallory").await, user(&server, "dana").await);
    seed_delta(&server, &d, "m", 8).await;

    // Up to six downloads of one principal: it reaches 36 MiB of the 48 MiB of
    // reads alone. A stream holds 9 MiB, and a new fill needs 17 MiB while it
    // runs (output, delta and its own copy of the 8 MiB reference), so three fit.
    let mut sockets = Vec::new();
    let mut statuses = Vec::new();
    for _ in 0..6 {
        let (socket, status) = stalled_get(&server, &m, "m/v1.zip").await;
        sockets.push(socket);
        statuses.push(status);
    }
    assert_eq!(
        statuses,
        [200, 200, 200, 503, 503, 503],
        "three streams of 9 MiB and a fill of 17 MiB fit the 36 MiB of one principal; the rest are SlowDown"
    );
    let held = wait_for_spool(&server, "M's stalled bodies hold spool", |u| {
        u >= 25 * MIB as u64
    })
    .await;
    eprintln!("stalled downloads hold {} MiB", held / MIB as u64);

    // D's PUT of 8 MiB needs about 24 MiB; 37 are free. It must succeed.
    let base = generate_binary(8 * MIB, 11);
    let started = Instant::now();
    let code = put(&server, &d, "m/dana.zip", mutate_binary(&base, 0.002)).await;
    assert_eq!(code, 200, "D's large PUT while M stalls");
    eprintln!("D's PUT took {:?}", started.elapsed());

    // The stall window cut M's bodies: the spool use returns to zero with
    // M's sockets still open.
    wait_for_spool(&server, "after the stall window", |u| u < MIB as u64).await;
    drop(sockets);
}

/// The count of delta decodes that the proxy ran (`/_/metrics`).
async fn decodes(server: &TestServer) -> u64 {
    let text = reqwest::Client::new()
        .get(format!("{}/_/metrics", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix("deltaglider_delta_decode_duration_seconds_count "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// The files in the spool directory now.
fn spool_files(spool: &tempfile::TempDir) -> usize {
    std::fs::read_dir(spool.path()).unwrap().count()
}

/// A principal over its reach is refused BEFORE its GET of a new delta
/// reserves, downloads or decodes anything: 503 SlowDown, no decode, and no
/// new spool file (spool files stay cached here, so a fill that ran would
/// leave one).
#[tokio::test]
async fn a_flood_of_gets_for_new_deltas_over_the_reach_makes_no_spool_file() {
    common::expect_throttling_only("a fill over the reach of a principal is 503 SlowDown");
    let spool = tempfile::tempdir().unwrap();
    let server = server(
        &spool,
        64,
        &[
            ("DGP_RESPONSE_STALL_SECS", "3600"),
            ("DGP_RANGE_SPOOL_TTL_SECS", "600"),
        ],
    )
    .await;
    let (m, d) = (user(&server, "mallory").await, user(&server, "dana").await);
    // One baseline and nine versions: nine distinct deltas, nine distinct fills.
    let base = generate_binary(4 * MIB, 31);
    assert_eq!(put(&server, &d, "m/base.zip", base.clone()).await, 200);
    for i in 1..=9 {
        let body = mutate_binary(&base, 0.001 * i as f64);
        assert_eq!(put(&server, &d, &format!("m/v{i}.zip"), body).await, 200);
    }

    // M reaches 36 of the 48 MiB of reads (75 % of them): seven fills of 5 MiB.
    let mut sockets = Vec::new();
    for i in 1..=7 {
        let (socket, status) = stalled_get(&server, &m, &format!("m/v{i}.zip")).await;
        assert_eq!(status, 200, "fill {i} is within the reach");
        sockets.push(socket);
    }
    wait_for_spool(&server, "M's streams hold spool", |u| u >= 30 * MIB as u64).await;

    // Every further GET of a new delta is refused at once, with no work done.
    // The gauge is sampled every second: let it see the last stream.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (files, runs, used) = (
        spool_files(&spool),
        decodes(&server).await,
        spool_used(&server).await,
    );
    for i in 8..=9 {
        let refused = m
            .http
            .request(Method::GET, url(&server, &format!("m/v{i}.zip")))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status().as_u16(), 503, "GET of v{i}");
        let text = refused.text().await.unwrap();
        assert!(text.contains("SlowDown"), "{text}");
    }
    // A server-side copy of a new delta to a name that is stored whole reads
    // (decodes) its source as the same principal: it is refused the same way,
    // with no work done. (A copy to a delta name ships the stored delta.)
    let refused = m
        .http
        .request(Method::PUT, url(&server, "other/copy.jpg"))
        .header(
            "x-amz-copy-source",
            format!("/{}/m/v8.zip", server.bucket()),
        )
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 503, "CopyObject of a new delta");
    assert_eq!(decodes(&server).await, runs, "a refused GET decoded");
    assert_eq!(
        spool_files(&spool),
        files,
        "a refused GET made a spool file"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        spool_used(&server).await,
        used,
        "a refused GET reserved spool"
    );

    // Dana has her own share.
    let got = d
        .http
        .request(Method::GET, url(&server, "m/v9.zip"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    assert_eq!(got.status().as_u16(), 200);
    drop(sockets);
}

/// A read-only IAM user with a browser session (`browser-connect`): the
/// session cookie and the user.
async fn browser_user(server: &TestServer, name: &str) -> (String, User) {
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let before = get_iam_version(&admin, &ep).await;
    let resp = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&json!({
            "name": name,
            "permissions": [{ "effect": "Allow", "actions": ["read", "list"], "resources": ["*"] }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create user {name}");
    let body: serde_json::Value = resp.json().await.unwrap();
    wait_for_iam_rebuild(&admin, &ep, before).await;
    let (key, secret) = (
        body["access_key_id"].as_str().unwrap().to_string(),
        body["secret_access_key"].as_str().unwrap().to_string(),
    );
    let resp = reqwest::Client::new()
        .post(format!("{ep}/_/api/admin/session/browser-connect"))
        .json(&json!({
            "access_key_id": key,
            "secret_access_key": secret,
            "endpoint": ep,
            "bucket": "",
            "region": "us-east-1",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "browser-connect of {name}");
    let cookie = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            v.split(';')
                .next()
                .filter(|c| c.starts_with("dgp_session="))
        })
        .expect("a session cookie")
        .to_string();
    let user = User {
        http: S3Http::signed(&key, &secret),
        key,
        secret,
    };
    (cookie, user)
}

fn zip_path(server: &TestServer, key: &str) -> String {
    format!("/_/api/admin/objects/zip?keys={}/{}", server.bucket(), key)
}

/// A ZIP download over raw TCP whose body is never read.
async fn stalled_zip(server: &TestServer, cookie: &str, key: &str) -> (tokio::net::TcpStream, u16) {
    let host = server.endpoint().trim_start_matches("http://").to_string();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n",
        zip_path(server, key)
    );
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut stream = socket.connect(host.parse().unwrap()).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let status = read_status(&mut stream).await;
    (stream, status)
}

/// The parallel ZIP downloads of a read-only user with a browser session are
/// charged to that user like its S3 GETs: the same share, the same 503, and
/// one ledger for both doors.
#[tokio::test]
async fn zip_downloads_of_a_browser_session_are_charged_like_its_s3_gets() {
    common::expect_throttling_only("a download over the reach of one principal is 503 SlowDown");
    let spool = tempfile::tempdir().unwrap();
    let server = server(&spool, 64, &[("DGP_RESPONSE_STALL_SECS", "3600")]).await;
    let d = user(&server, "dana").await;
    let (cookie, erin) = browser_user(&server, "erin").await;
    let base = generate_binary(4 * MIB, 41);
    assert_eq!(put(&server, &d, "z/base.zip", base.clone()).await, 200);
    for i in 1..=9 {
        let body = mutate_binary(&base, 0.001 * i as f64);
        assert_eq!(put(&server, &d, &format!("z/v{i}.zip"), body).await, 200);
    }

    // Six ZIPs of large deltas (5 MiB each) reach 30 of the 36 MiB that one
    // principal holds alone, and the seventh would need 9 MiB while it runs
    // (5 MiB and its own copy of the 4 MiB reference); none of their bodies is read.
    let mut sockets = Vec::new();
    for i in 1..=6 {
        let (socket, status) = stalled_zip(&server, &cookie, &format!("z/v{i}.zip")).await;
        assert_eq!(status, 200, "ZIP {i} is within the reach");
        sockets.push(socket);
    }
    wait_for_spool(&server, "the ZIPs hold spool", |u| u >= 25 * MIB as u64).await;

    // Another ZIP is refused, and so is an S3 GET of the same user.
    let refused = reqwest::Client::new()
        .get(format!(
            "{}{}",
            server.endpoint(),
            zip_path(&server, "z/v8.zip")
        ))
        .header("cookie", &cookie)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 503, "the seventh ZIP");
    let refused = erin
        .http
        .request(Method::GET, url(&server, "z/v9.zip"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 503, "an S3 GET of the same user");
    drop(sockets);
}

/// With a long stall window M's bodies stay: its share and the rest cap what
/// it holds, so D still gets its share, and M is told that the limit is its
/// own.
#[tokio::test]
async fn a_principal_with_stalled_downloads_keeps_at_most_its_reach() {
    common::expect_throttling_only("a download over the reach of one principal is 503 SlowDown");
    let spool = tempfile::tempdir().unwrap();
    let server = server(&spool, 128, &[("DGP_RESPONSE_STALL_SECS", "3600")]).await;
    let (m, d) = (user(&server, "mallory").await, user(&server, "dana").await);
    let v1 = seed_delta(&server, &d, "m", 8).await;

    // More downloads than the 72 MiB that one principal reaches alone (75 % of
    // the 96 MiB of reads): a stream holds 9 MiB, a new fill needs 17 MiB.
    let mut sockets = Vec::new();
    let mut statuses = Vec::new();
    for _ in 0..12 {
        let (socket, status) = stalled_get(&server, &m, "m/v1.zip").await;
        sockets.push(socket);
        statuses.push(status);
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        7,
        "seven streams of 9 MiB and a fill of 17 MiB fit the 72 MiB of one principal: {statuses:?}"
    );
    assert!(
        statuses.iter().all(|s| *s == 200 || *s == 503),
        "{statuses:?}"
    );
    wait_for_spool(&server, "M holds its reach", |u| u >= 60 * MIB as u64).await;

    // One more download of M is refused at once, naming the variable.
    let refused = m
        .http
        .request(Method::GET, url(&server, "m/v1.zip"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 503);
    let text = refused.text().await.unwrap();
    assert!(text.contains("DGP_SPOOL_READ_SHARE_PERCENT"), "{text}");

    // D reads and writes: its share is whole.
    let got = d
        .http
        .request(Method::GET, url(&server, "m/v1.zip"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    assert_eq!(got.status().as_u16(), 200);
    assert!(got.bytes().await.unwrap().to_vec() == v1);
    let base = generate_binary(8 * MIB, 11);
    assert_eq!(
        put(&server, &d, "m/dana.zip", mutate_binary(&base, 0.002)).await,
        200
    );
    drop(sockets);
}

async fn create_upload(server: &TestServer, who: &User, key: &str) -> (u16, String) {
    let resp = who
        .http
        .request(Method::POST, format!("{}?uploads", url(server, key)))
        .send()
        .await
        .unwrap();
    let code = resp.status().as_u16();
    (code, resp.text().await.unwrap())
}

fn upload_id(xml: &str) -> String {
    let start = xml.find("<UploadId>").expect("an UploadId") + "<UploadId>".len();
    xml[start..start + xml[start..].find("</UploadId>").unwrap()].to_string()
}

async fn upload_part(
    server: &TestServer,
    who: &User,
    key: &str,
    id: &str,
    n: u32,
) -> (u16, String) {
    let resp = who
        .http
        .request(
            Method::PUT,
            format!("{}?partNumber={n}&uploadId={id}", url(server, key)),
        )
        .body(vec![b'x'])
        .send()
        .await
        .unwrap();
    let code = resp.status().as_u16();
    (code, resp.text().await.unwrap())
}

/// M opens uploads up to its reach and sends parts into D's upload id. D can
/// still create an upload and send parts, and M is told that the limit is its
/// own. The parts are M's: they count for M, not for D.
#[tokio::test]
async fn parts_sent_into_anothers_upload_count_for_their_sender() {
    common::expect_throttling_only("a request over the own limit of a principal is 503 SlowDown");
    let spool = tempfile::tempdir().unwrap();
    let server = server(
        &spool,
        256,
        &[
            ("DGP_MAX_MULTIPART_UPLOADS", "8"),
            ("DGP_MAX_TOTAL_MULTIPART_BYTES", &(8 * MIB).to_string()),
        ],
    )
    .await;
    let (m, d) = (user(&server, "mallory").await, user(&server, "dana").await);

    let (code, xml) = create_upload(&server, &d, "d/first.bin").await;
    assert_eq!(code, 200, "{xml}");
    let dana_upload = upload_id(&xml);

    // M opens uploads until it is refused: 5 (D has 1, the rest of 8 is M's reach of 6).
    let mut opened = 0;
    let refusal = loop {
        let (code, text) = create_upload(&server, &m, &format!("m/u{opened}.bin")).await;
        if code != 200 {
            assert_eq!(code, 503);
            break text;
        }
        opened += 1;
        assert!(opened <= 8, "M was never refused");
    };
    assert_eq!(opened, 5, "M reaches 75 % of 8 uploads, D holds one");
    assert!(refusal.contains("your own limit"), "{refusal}");

    // M sends 1-byte parts into D's upload id: each holds 1 MiB of spool
    // budget. M reaches 6 of the 8 MiB.
    for n in 1..=6 {
        let (code, text) = upload_part(&server, &m, "d/first.bin", &dana_upload, n).await;
        assert_eq!(code, 200, "part {n}: {text}");
    }
    let (code, text) = upload_part(&server, &m, "d/first.bin", &dana_upload, 7).await;
    assert_eq!(code, 503, "{text}");
    assert!(text.contains("your own limit"), "{text}");

    // D can still create an upload and send parts: its share is whole.
    let (code, xml) = create_upload(&server, &d, "d/second.bin").await;
    assert_eq!(code, 200, "D's second upload: {xml}");
    let (code, text) = upload_part(&server, &d, "d/first.bin", &dana_upload, 7).await;
    assert_eq!(code, 200, "D's part: {text}");
    let (code, text) = upload_part(&server, &d, "d/first.bin", &dana_upload, 8).await;
    assert_eq!(code, 200, "D's second part: {text}");
}

/// The single CI uploader: with the default shares ONE principal uploads a
/// multipart object above its share of the multipart cap and runs three
/// large delta reads at once.
#[tokio::test]
async fn one_principal_alone_uses_most_of_the_budgets() {
    let spool = tempfile::tempdir().unwrap();
    // Spool 256 MiB: the multipart cap is half of it, 128 MiB; one principal reaches 96 MiB
    // of it (the auto maximum), and the 38.5 MiB object is below that.
    let server = server(&spool, 256, &[]).await;
    let ci = user(&server, "ci-uploader").await;
    let s3 = server.s3_client_with_creds(&ci.key, &ci.secret).await;

    // 7 parts of 5 MiB and one of 3.5 MiB: 38.5 MiB, above the 25 % share (32 MiB).
    let bucket = server.bucket().to_string();
    let created = s3
        .create_multipart_upload()
        .bucket(&bucket)
        .key("ci/big.bin")
        .send()
        .await
        .unwrap();
    let id = created.upload_id().unwrap().to_string();
    let mut total = 0usize;
    let mut done = Vec::new();
    for n in 1..=8i32 {
        let len = if n == 8 { 3 * MIB + MIB / 2 } else { 5 * MIB };
        total += len;
        let part = s3
            .upload_part()
            .bucket(&bucket)
            .key("ci/big.bin")
            .upload_id(&id)
            .part_number(n)
            .body(ByteStream::from(generate_binary(len, n as u64)))
            .send()
            .await
            .unwrap_or_else(|e| panic!("part {n} of a lone principal: {e:?}"));
        done.push(
            CompletedPart::builder()
                .part_number(n)
                .e_tag(part.e_tag().unwrap())
                .build(),
        );
    }
    s3.complete_multipart_upload()
        .bucket(&bucket)
        .key("ci/big.bin")
        .upload_id(&id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(done))
                .build(),
        )
        .send()
        .await
        .expect("a lone principal completes a multipart object above its share");
    let head = s3
        .head_object()
        .bucket(&bucket)
        .key("ci/big.bin")
        .send()
        .await
        .unwrap();
    assert_eq!(head.content_length(), Some(total as i64));

    // Three large delta reads at once: 3 x 25 MiB is above the share of 48 MiB
    // (25 % of the 192 MiB of reads), and within the 144 MiB that one principal
    // reaches alone.
    let base = generate_binary(24 * MIB, 21);
    assert_eq!(put(&server, &ci, "r/base.zip", base.clone()).await, 200);
    let mut bodies = Vec::new();
    for i in 1..=3 {
        let body = mutate_binary(&base, 0.001 * i as f64);
        assert_eq!(
            put(&server, &ci, &format!("r/v{i}.zip"), body.clone()).await,
            200
        );
        bodies.push(body);
    }
    let mut open = Vec::new();
    for i in 1..=3 {
        let mut resp = ci
            .http
            .request(Method::GET, url(&server, &format!("r/v{i}.zip")))
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "read {i} of a lone principal");
        // Take the first chunk only: the stream stays open, holding its spool file.
        let first = resp.chunk().await.unwrap().expect("a first chunk");
        open.push((resp, first.to_vec()));
    }
    for ((mut resp, mut got), want) in open.into_iter().zip(&bodies) {
        while let Some(chunk) = resp.chunk().await.unwrap() {
            got.extend_from_slice(&chunk);
        }
        assert!(&got == want, "a concurrent read returns its object");
    }
}

/// A request over raw TCP with `extra` signed headers, whose response is
/// never read.
async fn unread_request(
    server: &TestServer,
    who: &User,
    method: Method,
    path: &str,
    extra: &[(&str, &str)],
) -> tokio::net::TcpStream {
    let host = server.endpoint().trim_start_matches("http://").to_string();
    let target = format!("{}{}", server.endpoint(), path);
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n"
    );
    for (k, v) in sign_raw_with(method.clone(), &target, &who.key, &who.secret, extra) {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut stream = socket.connect(host.parse().unwrap()).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    stream
}

/// A server-side copy is paced by the proxy, not by its client: the source
/// is read by the proxy's own ingest, so a client that never reads its
/// response cannot hold the source file. Both copies end with the response
/// unread and a stall window of an hour, and the spool is whole again.
#[tokio::test]
async fn a_client_cannot_slow_a_server_side_copy_by_stalling_its_connection() {
    let spool = tempfile::tempdir().unwrap();
    let server = server(&spool, 64, &[("DGP_RESPONSE_STALL_SECS", "3600")]).await;
    let d = user(&server, "dana").await;
    seed_delta(&server, &d, "c", 4).await;
    let bucket = server.bucket().to_string();
    let source = format!("/{bucket}/c/v1.zip");

    // CopyObject.
    let copy = unread_request(
        &server,
        &d,
        Method::PUT,
        &format!("/{bucket}/other/copy.jpg"),
        &[("x-amz-copy-source", &source)],
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let head = d
            .http
            .request(Method::HEAD, url(&server, "other/copy.jpg"))
            .send()
            .await
            .unwrap();
        if head.status().as_u16() == 200 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the copy did not finish: {}",
            head.status()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // UploadPartCopy of a range of the same source.
    let (code, xml) = create_upload(&server, &d, "c/parts.bin").await;
    assert_eq!(code, 200, "{xml}");
    let id = upload_id(&xml);
    let range = format!("bytes=0-{}", 2 * MIB - 1);
    let part = unread_request(
        &server,
        &d,
        Method::PUT,
        &format!("/{bucket}/c/parts.bin?partNumber=1&uploadId={id}"),
        &[
            ("x-amz-copy-source", &source),
            ("x-amz-copy-source-range", &range),
        ],
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let parts = d
            .http
            .request(
                Method::GET,
                format!("{}?uploadId={id}", url(&server, "c/parts.bin")),
            )
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if parts.contains("<PartNumber>1</PartNumber>") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the part copy did not finish: {parts}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // The source file is gone with both sockets still open: only the relay
    // file of the part (2 MiB) stays until the upload ends.
    wait_for_spool(&server, "after both copies", |u| u <= 3 * MIB as u64).await;
    let aborted = d
        .http
        .request(
            Method::DELETE,
            format!("{}?uploadId={id}", url(&server, "c/parts.bin")),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status().as_u16(), 204);
    wait_for_spool(&server, "after the abort", |u| u < MIB as u64).await;
    drop((copy, part));
}

/// Read `bytes` from `stream` now (what is there, up to `bytes`).
async fn take(stream: &mut tokio::net::TcpStream, bytes: usize) -> usize {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; bytes];
    let mut got = 0;
    while got < bytes {
        match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf[got..])).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => got += n,
            Ok(Err(_)) => break,
        }
    }
    got
}

/// The rate floor on a real connection, with a window of 3 seconds: a client
/// that takes 8 KiB per second is cut, and a control client that takes 1 MiB
/// per second, far above the floor of the window, is never cut. The spool
/// gauge tells which body lived on. The operating system buffers a few MiB
/// of a response before the proxy sees that its client is slow, and it gives
/// the proxy more data to send in bursts as the client reads, so a client is
/// cut at the earliest when the buffered data is gone, and the exact floor
/// is proven on the pure clock (`stall_guard` unit tests), not here.
#[tokio::test]
async fn a_trickle_client_is_cut_and_a_client_above_the_floor_is_not() {
    let spool = tempfile::tempdir().unwrap();
    let server = server(&spool, 192, &[("DGP_RESPONSE_STALL_SECS", "3")]).await;
    let d = user(&server, "dana").await;
    let (trickle, control) = (
        user(&server, "trickle").await,
        user(&server, "control").await,
    );
    seed_delta(&server, &d, "t", 28).await;

    // Each client reads the object of 28 MiB (29 MiB of spool each).
    let (mut a, status) = stalled_get(&server, &trickle, "t/v1.zip").await;
    assert_eq!(status, 200);
    let (mut b, status) = stalled_get(&server, &control, "t/v1.zip").await;
    assert_eq!(status, 200);
    wait_for_spool(&server, "both bodies hold spool", |u| u >= 55 * MIB as u64).await;

    // Every second the trickle takes 8 KiB and the control 1 MiB.
    let started = Instant::now();
    let mut cut_at = None;
    while cut_at.is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(40),
            "the trickle client was never cut"
        );
        take(&mut a, 8 * 1024).await;
        take(&mut b, MIB).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        if spool_used(&server).await < 45 * MIB as u64 {
            cut_at = Some(started.elapsed());
        }
    }
    eprintln!("the trickle client was cut after {:?}", cut_at.unwrap());
    // The control lives on through three more windows.
    for _ in 0..9 {
        take(&mut b, MIB).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let used = spool_used(&server).await;
    assert!(
        used >= 25 * MIB as u64,
        "the control client was cut too: the spool holds {} MiB",
        used / MIB as u64
    );
    drop((a, b));
}

/// Two principals download one large delta object at the same time. The
/// reconstruction is one cached spool file: the read cap counts it once, and
/// each principal's own count has it (before, the total counted it once for
/// each principal, so two principals could not read one large file together).
#[tokio::test]
async fn two_principals_download_one_large_delta_object_at_the_same_time() {
    let spool = tempfile::tempdir().unwrap();
    let server = server(
        &spool,
        128,
        &[
            ("DGP_RESPONSE_STALL_SECS", "3600"),
            ("DGP_RANGE_SPOOL_TTL_SECS", "600"),
        ],
    )
    .await;
    let (m, d) = (user(&server, "mallory").await, user(&server, "dana").await);
    seed_delta(&server, &d, "m", 8).await;
    let (first, first_status) = stalled_get(&server, &m, "m/v1.zip").await;
    let (second, second_status) = stalled_get(&server, &d, "m/v1.zip").await;
    assert_eq!((first_status, second_status), (200, 200));
    drop((first, second));
}

/// A principal that holds its reach with downloads of deltas in several
/// folders leaves the other principals their share: a second user's PUT of
/// 20 MiB succeeds. (The reference copy that each fill downloads is part of
/// what the fill is charged for, so the fills cannot hold more than the read
/// cap allows.)
#[tokio::test]
async fn a_large_put_succeeds_while_a_principal_holds_its_reach_in_several_folders() {
    common::expect_throttling_only("a fill over the reach of a principal is 503 SlowDown");
    let spool = tempfile::tempdir().unwrap();
    let server = server(&spool, 96, &[("DGP_RESPONSE_STALL_SECS", "3600")]).await;
    let (m, d) = (user(&server, "mallory").await, user(&server, "dana").await);
    for i in 0..8 {
        seed_delta(&server, &d, &format!("f{i}"), 4).await;
    }
    // Mallory opens a download in each folder and reads none of them: the
    // ones within its reach are served, the rest are refused at once.
    let mut open = Vec::new();
    let mut statuses = Vec::new();
    for i in 0..8 {
        let (socket, status) = stalled_get(&server, &m, &format!("f{i}/v1.zip")).await;
        open.push(socket);
        statuses.push(status);
    }
    assert!(statuses.contains(&200), "{statuses:?}");
    let code = put(&server, &d, "other/dana.jpg", generate_binary(20 * MIB, 5)).await;
    assert_eq!(
        code, 200,
        "Dana's PUT of 20 MiB while Mallory holds her reach"
    );
    drop(open);
}
