// SPDX-License-Identifier: BUSL-1.1

//! The conversion marker on a real CAS backend (MinIO): the encrypting
//! wrapper creates it once and reuses it, closes it only by identity, and
//! the proxy hides it from client listings and refuses client requests on
//! its key. Wrapper-level cases (format, cache, scripted races) are in
//! `src/storage/encrypting/conversion_tests.rs`.

use crate::common;

use common::TestServer;
use deltaglider_proxy::storage::encrypting::conversion::{ByIdentity, Window};
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
    let run = a.open_conversion(&bucket, 7).await.unwrap();
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
    assert_eq!(b.open_conversion(&bucket, 3).await.unwrap(), run);
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
    let run = wrapper.open_conversion("releases", 7).await.unwrap();
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
