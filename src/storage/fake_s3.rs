// SPDX-License-Identifier: BUSL-1.1

//! An in-process S3 for unit tests that count requests: it keeps objects
//! with their `x-amz-meta-*` headers in memory and records every request as
//! `"METHOD /bucket/key"` (a bucket-level LIST as `"GET /bucket?..."`).
//! It answers PUT, GET, HEAD, DELETE and ListObjectsV2 (`prefix`,
//! `delimiter`, `start-after`, `max-keys` (up to 1000) and
//! `continuation-token`, paged like S3) and ListBuckets. Other
//! bucket-level requests get an empty listing. Tests can delay each
//! operation, read its peak concurrency, and inject faults
//! ([`FakeS3::set_delay_ms`], [`FakeS3::peak_in_flight`], [`FakeS3::fail`]).

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

/// A fault rule: requests of operation `op` whose path contains
/// `path_contains` get `status` with S3 error `code`, `remaining` more
/// times (`u32::MAX` = always).
struct Fault {
    op: String,
    path_contains: String,
    status: StatusCode,
    code: String,
    remaining: u32,
}

#[derive(Default)]
pub(crate) struct FakeS3 {
    objects: parking_lot::Mutex<HashMap<String, Stored>>,
    requests: parking_lot::Mutex<Vec<String>>,
    changed: tokio::sync::Notify,
    /// Buckets made with a bucket-level PUT (CreateBucket).
    buckets: parking_lot::Mutex<std::collections::BTreeSet<String>>,
    /// Delay per operation in ms (see [`FakeS3::set_delay_ms`]).
    delays: parking_lot::Mutex<HashMap<String, u64>>,
    /// Requests in flight per operation, and the peak: a test can see
    /// whether a client sends requests concurrently.
    in_flight: parking_lot::Mutex<HashMap<String, (usize, usize)>>,
    faults: parking_lot::Mutex<Vec<Fault>>,
}

impl FakeS3 {
    /// Every request so far, in order.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().clone()
    }

    /// Make every request of operation `op` take `ms` milliseconds.
    /// Operations: `PUT`, `GET`, `HEAD`, `DELETE` (object level), `LIST`
    /// (ListObjectsV2), `LIST_BUCKETS`, `HEAD_BUCKET`, `BUCKET` (other
    /// bucket-level requests).
    pub(crate) fn set_delay_ms(&self, op: &str, ms: u64) {
        self.delays.lock().insert(op.to_string(), ms);
    }

    /// The most requests of operation `op` that were in flight at once.
    pub(crate) fn peak_in_flight(&self, op: &str) -> usize {
        self.in_flight.lock().get(op).map_or(0, |(_, peak)| *peak)
    }

    /// Answer the next `times` requests of operation `op` whose path
    /// contains `path_contains` with `status` and S3 error `code`
    /// (`u32::MAX` = every request).
    pub(crate) fn fail(&self, op: &str, path_contains: &str, status: u16, code: &str, times: u32) {
        self.faults.lock().push(Fault {
            op: op.to_string(),
            path_contains: path_contains.to_string(),
            status: StatusCode::from_u16(status).unwrap(),
            code: code.to_string(),
            remaining: times,
        });
    }

    /// Remove every fault rule.
    pub(crate) fn clear_faults(&self) {
        self.faults.lock().clear();
    }

    /// Make every object PUT take `ms` milliseconds.
    pub(crate) fn set_put_delay_ms(&self, ms: u64) {
        self.set_delay_ms("PUT", ms);
    }

    /// The most object PUTs that were in flight at once.
    pub(crate) fn peak_puts_in_flight(&self) -> usize {
        self.peak_in_flight("PUT")
    }

    /// Make every object DELETE take `ms` milliseconds.
    pub(crate) fn set_delete_delay_ms(&self, ms: u64) {
        self.set_delay_ms("DELETE", ms);
    }

    /// The most object DELETEs that were in flight at once.
    pub(crate) fn peak_deletes_in_flight(&self) -> usize {
        self.peak_in_flight("DELETE")
    }

    /// Count the request as in flight for the operation's delay, then
    /// apply the first matching fault rule.
    async fn enter(&self, op: &str, path: &str) -> Option<(StatusCode, Vec<u8>)> {
        {
            let mut m = self.in_flight.lock();
            let e = m.entry(op.to_string()).or_default();
            e.0 += 1;
            e.1 = e.1.max(e.0);
        }
        let delay = self.delays.lock().get(op).copied().unwrap_or(0);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        if let Some(e) = self.in_flight.lock().get_mut(op) {
            e.0 -= 1;
        }
        let mut faults = self.faults.lock();
        let fault = faults
            .iter_mut()
            .find(|f| f.op == op && f.remaining > 0 && path.contains(&f.path_contains))?;
        if fault.remaining != u32::MAX {
            fault.remaining -= 1;
        }
        Some((
            fault.status,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{}</Code>\
                 <Message>fake fault</Message></Error>",
                fault.code
            )
            .into_bytes(),
        ))
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

/// One ListObjectsV2 page over `objects` (keys `bucket/key`), at most
/// `max-keys` entries (objects plus common prefixes).
fn list_page(
    objects: &HashMap<String, Stored>,
    bucket: &str,
    query: &HashMap<String, String>,
) -> String {
    let prefix = query.get("prefix").map(String::as_str).unwrap_or("");
    let delimiter = query.get("delimiter").map(String::as_str).unwrap_or("");
    // A continuation token is the last key of the previous page.
    let after = query
        .get("continuation-token")
        .or_else(|| query.get("start-after"))
        .map(String::as_str)
        .unwrap_or("");
    let max_keys: usize = query
        .get("max-keys")
        .and_then(|m| m.parse().ok())
        .unwrap_or(1000)
        .min(1000);
    let mut keys: Vec<(&str, &Stored)> = objects
        .iter()
        .filter_map(|(path, o)| Some((path.strip_prefix(bucket)?.strip_prefix('/')?, o)))
        .filter(|(k, _)| k.starts_with(prefix) && *k > after)
        .collect();
    keys.sort_by_key(|(k, _)| *k);
    let mut contents = String::new();
    let mut common = std::collections::BTreeSet::new();
    let mut entries = 0usize;
    let mut next_token: Option<&str> = None;
    for (k, o) in keys {
        let rest = &k[prefix.len()..];
        if let Some(i) = (!delimiter.is_empty())
            .then(|| rest.find(delimiter))
            .flatten()
        {
            let p = &k[..prefix.len() + i + delimiter.len()];
            if !common.contains(p) {
                if entries == max_keys {
                    break;
                }
                entries += 1;
                common.insert(p);
            }
            next_token = Some(k);
            continue;
        }
        if entries == max_keys {
            break;
        }
        entries += 1;
        next_token = Some(k);
        contents.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>2025-01-01T00:00:00.000Z</LastModified>\
             <ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
            xml_escape(k),
            xml_escape(&o.etag),
            o.body.len()
        ));
    }
    // Truncated when an entry past the page exists.
    let truncated = entries == max_keys
        && objects.keys().any(|path| {
            path.strip_prefix(bucket)
                .and_then(|p| p.strip_prefix('/'))
                .is_some_and(|k| {
                    k.starts_with(prefix)
                        && next_token.is_some_and(|t| k > t)
                        && !common.iter().any(|c| k.starts_with(c))
                })
        });
    let token = match (truncated, next_token) {
        (true, Some(t)) => format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            xml_escape(t)
        ),
        _ => String::new(),
    };
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
         <Name>{bucket}</Name><Prefix>{}</Prefix><MaxKeys>{max_keys}</MaxKeys>\
         <IsTruncated>{truncated}</IsTruncated>{token}{contents}{common}</ListBucketResult>",
        xml_escape(prefix)
    )
}

/// The `prefix` query parameter of a LIST request (`""` when absent).
pub(crate) fn list_prefix(uri_or_request: &str) -> String {
    let query = uri_or_request.split_once('?').map_or("", |(_, q)| q);
    serde_urlencoded::from_str::<Vec<(String, String)>>(query)
        .unwrap_or_default()
        .into_iter()
        .find(|(k, _)| k == "prefix")
        .map(|(_, v)| v)
        .unwrap_or_default()
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
    let on_root = fake.clone();
    let on_bucket_handler =
        move |method: Method,
              uri: Uri,
              axum::extract::Path(params): axum::extract::Path<HashMap<String, String>>| {
            let f = on_bucket.clone();
            async move {
                f.record(&method, &uri);
                let query: HashMap<String, String> =
                    serde_urlencoded::from_str(uri.query().unwrap_or("")).unwrap_or_default();
                let bucket = params.get("bucket").cloned().unwrap_or_default();
                let is_list = method == Method::GET
                    && query.get("list-type").map(String::as_str) == Some("2");
                let op = match method {
                    _ if is_list => "LIST",
                    Method::HEAD => "HEAD_BUCKET",
                    _ => "BUCKET",
                };
                let full = uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
                if let Some((status, body)) = f.enter(op, full).await {
                    return (status, String::from_utf8(body).unwrap());
                }
                if is_list {
                    return (
                        StatusCode::OK,
                        list_page(&f.objects.lock(), &bucket, &query),
                    );
                }
                if method == Method::PUT {
                    f.buckets.lock().insert(bucket);
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
                        if let Some((status, body)) = f.enter(method.as_str(), &path).await {
                            return (status, HeaderMap::new(), body);
                        }
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
        .route(
            "/",
            axum::routing::get(move |method: Method, uri: Uri| {
                let f = on_root.clone();
                async move {
                    f.record(&method, &uri);
                    if let Some((status, body)) = f.enter("LIST_BUCKETS", "/").await {
                        return (status, String::from_utf8(body).unwrap());
                    }
                    let mut names = f.buckets.lock().clone();
                    for path in f.objects.lock().keys() {
                        if let Some((b, _)) = path.split_once('/') {
                            names.insert(b.to_string());
                        }
                    }
                    let buckets: String = names
                        .iter()
                        .map(|n| {
                            format!(
                                "<Bucket><Name>{}</Name>\
                                 <CreationDate>2025-01-01T00:00:00.000Z</CreationDate></Bucket>",
                                xml_escape(n)
                            )
                        })
                        .collect();
                    (
                        StatusCode::OK,
                        format!(
                            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                             <ListAllMyBucketsResult><Owner><ID>fake</ID></Owner>\
                             <Buckets>{buckets}</Buckets></ListAllMyBucketsResult>"
                        ),
                    )
                }
            }),
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn put(endpoint: &str, key: &str) {
        reqwest::Client::new()
            .put(format!("{endpoint}/b/{key}"))
            .body("x")
            .send()
            .await
            .unwrap();
    }

    async fn list(endpoint: &str, query: &str) -> String {
        reqwest::Client::new()
            .get(format!("{endpoint}/b?list-type=2&{query}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    /// A listing pages at `max-keys`; the continuation token resumes it.
    #[tokio::test]
    async fn list_pages_at_max_keys() {
        let (endpoint, _fake) = start().await;
        for k in ["d/a", "d/b", "d/c", "d/sub/x", "d/sub/y", "d/z"] {
            put(&endpoint, k).await;
        }
        let page = list(&endpoint, "prefix=d%2F&delimiter=%2F&max-keys=2").await;
        assert!(page.contains("<IsTruncated>true</IsTruncated>"), "{page}");
        assert!(page.contains("<Key>d/a</Key>") && page.contains("<Key>d/b</Key>"));
        assert!(page.contains("<NextContinuationToken>d/b</NextContinuationToken>"));
        let page = list(
            &endpoint,
            "prefix=d%2F&delimiter=%2F&max-keys=2&continuation-token=d%2Fb",
        )
        .await;
        assert!(page.contains("<Key>d/c</Key>"), "{page}");
        assert!(page.contains("<Prefix>d/sub/</Prefix>"), "{page}");
        assert!(page.contains("<IsTruncated>true</IsTruncated>"), "{page}");
        let page = list(
            &endpoint,
            "prefix=d%2F&delimiter=%2F&max-keys=2&continuation-token=d%2Fsub%2Fy",
        )
        .await;
        assert!(page.contains("<Key>d/z</Key>"), "{page}");
        assert!(page.contains("<IsTruncated>false</IsTruncated>"), "{page}");
    }

    /// A fault rule answers the matching requests, then expires.
    #[tokio::test]
    async fn a_fault_answers_its_requests_then_expires() {
        let (endpoint, fake) = start().await;
        put(&endpoint, "k").await;
        fake.fail("HEAD", "b/k", 503, "SlowDown", 1);
        let c = reqwest::Client::new();
        let first = c.head(format!("{endpoint}/b/k")).send().await.unwrap();
        assert_eq!(first.status(), 503);
        let second = c.head(format!("{endpoint}/b/k")).send().await.unwrap();
        assert_eq!(second.status(), 200);
        fake.fail("HEAD", "b/k", 500, "InternalError", u32::MAX);
        for _ in 0..3 {
            let r = c.head(format!("{endpoint}/b/k")).send().await.unwrap();
            assert_eq!(r.status(), 500);
        }
        fake.clear_faults();
        let r = c.head(format!("{endpoint}/b/k")).send().await.unwrap();
        assert_eq!(r.status(), 200);
    }

    /// ListBuckets names the created buckets and the buckets of objects.
    #[tokio::test]
    async fn list_buckets_names_known_buckets() {
        let (endpoint, _fake) = start().await;
        reqwest::Client::new()
            .put(format!("{endpoint}/empty"))
            .send()
            .await
            .unwrap();
        put(&endpoint, "k").await;
        let body = reqwest::get(format!("{endpoint}/"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("<Name>empty</Name>") && body.contains("<Name>b</Name>"),
            "{body}"
        );
    }
}
