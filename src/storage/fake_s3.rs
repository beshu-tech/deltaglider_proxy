// SPDX-License-Identifier: BUSL-1.1

//! An in-process S3 for unit tests that count requests: it keeps objects
//! with their `x-amz-meta-*` headers in memory and records every request as
//! `"METHOD /bucket/key"` (a bucket-level LIST as `"GET /bucket?..."`).
//! It answers PUT, GET, HEAD, DELETE and ListObjectsV2 (`prefix`,
//! `delimiter`, `start-after`; every match on one page). Other
//! bucket-level requests get an empty listing.

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
    /// Delay of every object PUT, in ms (0 = none), and the in-flight
    /// PUT count and its peak: a test can see whether a client writes
    /// concurrently.
    put_delay_ms: std::sync::atomic::AtomicU64,
    puts_in_flight: std::sync::atomic::AtomicUsize,
    puts_peak: std::sync::atomic::AtomicUsize,
    /// The same for object DELETEs.
    delete_delay_ms: std::sync::atomic::AtomicU64,
    deletes_in_flight: std::sync::atomic::AtomicUsize,
    deletes_peak: std::sync::atomic::AtomicUsize,
}

impl FakeS3 {
    /// Every request so far, in order.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().clone()
    }

    /// Make every object PUT take `ms` milliseconds.
    pub(crate) fn set_put_delay_ms(&self, ms: u64) {
        self.put_delay_ms
            .store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    /// The most object PUTs that were in flight at once.
    pub(crate) fn peak_puts_in_flight(&self) -> usize {
        self.puts_peak.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Make every object DELETE take `ms` milliseconds.
    pub(crate) fn set_delete_delay_ms(&self, ms: u64) {
        self.delete_delay_ms
            .store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    /// The most object DELETEs that were in flight at once.
    pub(crate) fn peak_deletes_in_flight(&self) -> usize {
        self.deletes_peak.load(std::sync::atomic::Ordering::SeqCst)
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

/// One ListObjectsV2 page over `objects` (keys `bucket/key`): every match,
/// never truncated.
fn list_page(
    objects: &HashMap<String, Stored>,
    bucket: &str,
    query: &HashMap<String, String>,
) -> String {
    let prefix = query.get("prefix").map(String::as_str).unwrap_or("");
    let delimiter = query.get("delimiter").map(String::as_str).unwrap_or("");
    let after = query.get("start-after").map(String::as_str).unwrap_or("");
    let mut keys: Vec<(&str, &Stored)> = objects
        .iter()
        .filter_map(|(path, o)| Some((path.strip_prefix(bucket)?.strip_prefix('/')?, o)))
        .filter(|(k, _)| k.starts_with(prefix) && *k > after)
        .collect();
    keys.sort_by_key(|(k, _)| *k);
    let mut contents = String::new();
    let mut common = std::collections::BTreeSet::new();
    for (k, o) in keys {
        let rest = &k[prefix.len()..];
        if let Some(i) = (!delimiter.is_empty())
            .then(|| rest.find(delimiter))
            .flatten()
        {
            common.insert(&k[..prefix.len() + i + delimiter.len()]);
            continue;
        }
        contents.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>2025-01-01T00:00:00.000Z</LastModified>\
             <ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
            xml_escape(k),
            xml_escape(&o.etag),
            o.body.len()
        ));
    }
    let common: String = common
        .into_iter()
        .map(|p| {
            format!(
                "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                xml_escape(p)
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Name>{bucket}</Name><Prefix>{}</Prefix><MaxKeys>1000</MaxKeys>\
         <IsTruncated>false</IsTruncated>{contents}{common}</ListBucketResult>",
        xml_escape(prefix)
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
    let on_bucket_handler = move |method: Method,
                                  uri: Uri,
                                  axum::extract::Path(params): axum::extract::Path<
        HashMap<String, String>,
    >| {
        let f = on_bucket.clone();
        async move {
            f.record(&method, &uri);
            let query: HashMap<String, String> =
                serde_urlencoded::from_str(uri.query().unwrap_or("")).unwrap_or_default();
            let bucket = params.get("bucket").cloned().unwrap_or_default();
            if method == Method::GET && query.get("list-type").map(String::as_str) == Some("2") {
                return (
                    StatusCode::OK,
                    list_page(&f.objects.lock(), &bucket, &query),
                );
            }
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
                                use std::sync::atomic::Ordering::SeqCst;
                                let now = f.puts_in_flight.fetch_add(1, SeqCst) + 1;
                                f.puts_peak.fetch_max(now, SeqCst);
                                let delay = f.put_delay_ms.load(SeqCst);
                                if delay > 0 {
                                    tokio::time::sleep(std::time::Duration::from_millis(delay))
                                        .await;
                                }
                                f.puts_in_flight.fetch_sub(1, SeqCst);
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
                                use std::sync::atomic::Ordering::SeqCst;
                                let now = f.deletes_in_flight.fetch_add(1, SeqCst) + 1;
                                f.deletes_peak.fetch_max(now, SeqCst);
                                let delay = f.delete_delay_ms.load(SeqCst);
                                if delay > 0 {
                                    tokio::time::sleep(std::time::Duration::from_millis(delay))
                                        .await;
                                }
                                f.deletes_in_flight.fetch_sub(1, SeqCst);
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

/// A fake S3 and an S3 backend on it (no retries, no SSRF guard).
pub(crate) async fn backend() -> (crate::storage::S3Backend, Arc<FakeS3>) {
    let (endpoint, fake) = start().await;
    let s3 = crate::storage::s3::test_support::for_test_endpoint(&endpoint);
    (s3, fake)
}
