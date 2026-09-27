// SPDX-License-Identifier: BUSL-1.1

//! An in-process S3 for unit tests that count requests: it keeps objects
//! with their `x-amz-meta-*` headers in memory and records every request as
//! `"METHOD /bucket/key"` (a bucket-level LIST as `"GET /bucket?..."`).
//! It answers PUT, GET, HEAD, DELETE and an empty ListObjectsV2.

use axum::http::{HeaderMap, Method, StatusCode, Uri};
use md5::Digest;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
struct Stored {
    body: Vec<u8>,
    meta: Vec<(String, String)>,
    etag: String,
}

#[derive(Default)]
pub(crate) struct FakeS3 {
    objects: parking_lot::Mutex<HashMap<String, Stored>>,
    requests: parking_lot::Mutex<Vec<String>>,
    changed: tokio::sync::Notify,
}

impl FakeS3 {
    /// Every request so far, in order.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().clone()
    }

    /// Forget the requests so far.
    pub(crate) fn clear(&self) {
        self.requests.lock().clear();
    }

    /// Wait until a request matches `pred` (up to 10 s).
    pub(crate) async fn wait_for(&self, pred: impl Fn(&str) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let notified = self.changed.notified();
            if self.requests().iter().any(|r| pred(r)) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return false;
            }
        }
    }

    fn record(&self, method: &Method, uri: &Uri) {
        let path = uri
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default();
        self.requests.lock().push(format!("{method} {path}"));
        self.changed.notify_waiters();
    }
}

const EMPTY_LIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
<Name>b</Name><Prefix></Prefix><KeyCount>0</KeyCount><MaxKeys>1000</MaxKeys>\
<IsTruncated>false</IsTruncated></ListBucketResult>";

fn object_headers(o: &Stored) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in &o.meta {
        h.insert(
            axum::http::HeaderName::try_from(format!("x-amz-meta-{k}")).unwrap(),
            v.parse().unwrap(),
        );
    }
    h.insert("etag", o.etag.parse().unwrap());
    h.insert(
        "last-modified",
        "Wed, 01 Jan 2025 00:00:00 GMT".parse().unwrap(),
    );
    h.insert("content-length", o.body.len().to_string().parse().unwrap());
    h
}

/// Start the fake; returns its endpoint.
pub(crate) async fn start() -> (String, Arc<FakeS3>) {
    let fake = Arc::new(FakeS3::default());
    let on_object = fake.clone();
    let on_bucket = fake.clone();
    let on_bucket_handler = move |method: Method, uri: Uri| {
        let f = on_bucket.clone();
        async move {
            f.record(&method, &uri);
            (StatusCode::OK, EMPTY_LIST.to_string())
        }
    };
    let app = axum::Router::new()
        .route(
            "/:bucket/*key",
            axum::routing::any(
                move |method: Method,
                      uri: Uri,
                      axum::extract::Path((b, k)): axum::extract::Path<(String, String)>,
                      headers: HeaderMap,
                      body: axum::body::Bytes| {
                    let f = on_object.clone();
                    async move {
                        f.record(&method, &uri);
                        let path = format!("{b}/{k}");
                        let current = f.objects.lock().get(&path).cloned();
                        match method {
                            Method::PUT => {
                                let etag = format!("\"{}\"", hex::encode(md5::Md5::digest(&body)));
                                let meta = headers
                                    .iter()
                                    .filter_map(|(k, v)| {
                                        Some((
                                            k.as_str().strip_prefix("x-amz-meta-")?.to_string(),
                                            v.to_str().ok()?.to_string(),
                                        ))
                                    })
                                    .collect();
                                f.objects.lock().insert(
                                    path,
                                    Stored {
                                        body: body.to_vec(),
                                        meta,
                                        etag: etag.clone(),
                                    },
                                );
                                let mut h = HeaderMap::new();
                                h.insert("etag", etag.parse().unwrap());
                                (StatusCode::OK, h, Vec::new())
                            }
                            Method::GET | Method::HEAD => match current {
                                Some(o) => {
                                    let h = object_headers(&o);
                                    let body = if method == Method::GET {
                                        o.body
                                    } else {
                                        Vec::new()
                                    };
                                    (StatusCode::OK, h, body)
                                }
                                None => (
                                    StatusCode::NOT_FOUND,
                                    HeaderMap::new(),
                                    b"<Error><Code>NoSuchKey</Code></Error>".to_vec(),
                                ),
                            },
                            Method::DELETE => {
                                f.objects.lock().remove(&path);
                                (StatusCode::NO_CONTENT, HeaderMap::new(), Vec::new())
                            }
                            _ => (StatusCode::NOT_IMPLEMENTED, HeaderMap::new(), Vec::new()),
                        }
                    }
                },
            ),
        )
        .route("/:bucket", axum::routing::any(on_bucket_handler.clone()))
        .route("/:bucket/", axum::routing::any(on_bucket_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}
