// SPDX-License-Identifier: BUSL-1.1

//! Raw HTTP for S3 requests that a test builds by hand, SigV4-signed.
//!
//! The AWS SDK cannot send every shape a test needs (a bad XML body, an odd
//! header, a hand-built aws-chunked body), so tests use reqwest for those.
//! [`S3Http`] keeps the reqwest builder API and signs each request with the
//! server's credentials at `send()`, so such a test runs with auth ON.
//! An unsigned [`S3Http`] (open-access server) sends requests as they are.
//!
//! Signing follows the S3 rules of the AWS SDK: single percent-encoding, no
//! path normalisation, `x-amz-content-sha256` signed. A request that already
//! carries `authorization`, or a presigned query, is sent unchanged. A body
//! hash the test set in `x-amz-content-sha256` (aws-chunked streaming
//! values, `UNSIGNED-PAYLOAD`) is signed as given.

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    sign, PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest, SigningSettings,
    UriPathNormalizationMode,
};
use aws_sigv4::sign::v4;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use sha2::{Digest, Sha256};

/// A reqwest client that SigV4-signs S3 requests (see the module docs).
#[derive(Clone)]
pub struct S3Http {
    client: reqwest::Client,
    creds: Option<(String, String)>,
}

impl S3Http {
    /// Signs every request with `access_key` / `secret_key`.
    pub fn signed(access_key: &str, secret_key: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            creds: Some((access_key.to_string(), secret_key.to_string())),
        }
    }

    /// Sends requests unsigned (for an open-access server).
    pub fn unsigned() -> Self {
        Self {
            client: reqwest::Client::new(),
            creds: None,
        }
    }

    /// Same credentials, another reqwest client (timeouts, redirects, ...).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    pub fn request(&self, method: Method, url: impl reqwest::IntoUrl) -> S3RequestBuilder {
        S3RequestBuilder {
            client: self.client.clone(),
            creds: self.creds.clone(),
            inner: self.client.request(method, url),
        }
    }

    pub fn get(&self, url: impl reqwest::IntoUrl) -> S3RequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn put(&self, url: impl reqwest::IntoUrl) -> S3RequestBuilder {
        self.request(Method::PUT, url)
    }

    pub fn post(&self, url: impl reqwest::IntoUrl) -> S3RequestBuilder {
        self.request(Method::POST, url)
    }

    pub fn head(&self, url: impl reqwest::IntoUrl) -> S3RequestBuilder {
        self.request(Method::HEAD, url)
    }

    pub fn delete(&self, url: impl reqwest::IntoUrl) -> S3RequestBuilder {
        self.request(Method::DELETE, url)
    }
}

/// A reqwest `RequestBuilder` that signs at [`send`](Self::send).
pub struct S3RequestBuilder {
    client: reqwest::Client,
    creds: Option<(String, String)>,
    inner: reqwest::RequestBuilder,
}

impl S3RequestBuilder {
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        self.inner = self.inner.header(key, value);
        self
    }

    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.inner = self.inner.headers(headers);
        self
    }

    pub fn body<T: Into<reqwest::Body>>(mut self, body: T) -> Self {
        self.inner = self.inner.body(body);
        self
    }

    pub fn query<T: serde::Serialize + ?Sized>(mut self, query: &T) -> Self {
        self.inner = self.inner.query(query);
        self
    }

    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.inner = self.inner.timeout(timeout);
        self
    }

    pub async fn send(self) -> reqwest::Result<reqwest::Response> {
        let mut request = self.inner.build()?;
        if let Some((key, secret)) = &self.creds {
            sign_in_place(&mut request, key, secret);
        }
        self.client.execute(request).await
    }
}

/// Anything the S3 raw-HTTP helpers (`put_object`, `get_bytes`, ...) can
/// send through: a signing [`S3Http`], or a plain reqwest client (unsigned).
pub trait S3Requests {
    fn s3_request(&self, method: Method, url: &str) -> S3RequestBuilder;
}

impl S3Requests for S3Http {
    fn s3_request(&self, method: Method, url: &str) -> S3RequestBuilder {
        self.request(method, url)
    }
}

impl S3Requests for reqwest::Client {
    fn s3_request(&self, method: Method, url: &str) -> S3RequestBuilder {
        S3RequestBuilder {
            client: self.clone(),
            creds: None,
            inner: self.request(method, url),
        }
    }
}

fn sign_in_place(request: &mut reqwest::Request, key: &str, secret: &str) {
    let presigned = request
        .url()
        .query_pairs()
        .any(|(k, _)| k == "X-Amz-Signature");
    if presigned || request.headers().contains_key("authorization") {
        return;
    }
    // The signed body hash: the test's own value, else SHA-256 of the bytes
    // (a streamed body cannot be hashed up front: UNSIGNED-PAYLOAD).
    let hash = match request.headers().get("x-amz-content-sha256") {
        Some(v) => v.to_str().expect("x-amz-content-sha256").to_string(),
        None => match request.body() {
            None => hex::encode(Sha256::digest(b"")),
            Some(b) => match b.as_bytes() {
                Some(bytes) => hex::encode(Sha256::digest(bytes)),
                None => "UNSIGNED-PAYLOAD".to_string(),
            },
        },
    };
    let mut settings = SigningSettings::default();
    settings.percent_encoding_mode = PercentEncodingMode::Single;
    settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    let identity = Credentials::new(key, secret, None, None, "test").into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region("us-east-1")
        .name("s3")
        .time(std::time::SystemTime::now())
        .settings(settings)
        .build()
        .expect("signing params")
        .into();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let signable = SignableRequest::new(
        request.method().as_str(),
        request.url().as_str(),
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        SignableBody::Precomputed(hash),
    )
    .expect("signable request");
    let (instructions, _) = sign(signable, &params).expect("sign").into_parts();
    for (name, value) in instructions.headers() {
        request.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
}
