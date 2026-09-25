// SPDX-License-Identifier: BUSL-1.1

//! Admin request extractors with JSON errors.
//!
//! axum's `Json` / `Query` answer a bad input with a plain-text body such as
//! "Failed to deserialize the JSON body into the target type: dest_prefix:
//! invalid path ... at line 1 column 81". [`AdminJson`] and [`AdminQuery`]
//! run the same extraction and answer `400 {"error": <code>, "message": ...}`
//! instead: the message names the field and the rule, and the code tells
//! a path escape (`invalid_path`) and a bad bucket name (`invalid_bucket`)
//! from any other bad input (`invalid_request`). The status matches the
//! other admin validation errors (400). `admin_handlers_use_admin_extractors`
//! (below) keeps new handlers on these types.

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

/// JSON body extractor: `axum::Json` with a JSON error body.
pub struct AdminJson<T>(pub T);

/// Query-string extractor: `axum::extract::Query` with a JSON error body.
pub struct AdminQuery<T>(pub T);

/// The rejection of both extractors.
#[derive(Debug)]
pub struct AdminInputRejection {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl IntoResponse for AdminInputRejection {
    fn into_response(self) -> Response {
        (
            self.status,
            axum::Json(serde_json::json!({ "error": self.code, "message": self.message })),
        )
            .into_response()
    }
}

/// Pure: the error code for an input-rule message. The path-guard rules
/// (`path_guard::check_*`) start their messages with these phrases.
pub fn input_error_code(message: &str) -> &'static str {
    if message.contains("invalid path") {
        "invalid_path"
    } else if message.contains("invalid bucket name") {
        "invalid_bucket"
    } else {
        "invalid_request"
    }
}

/// Pure: `field: rule` when the field is known, else the rule alone.
pub fn input_error_message(field: &str, rule: &str) -> String {
    if field.is_empty() || field == "." {
        rule.to_string()
    } else {
        format!("{field}: {rule}")
    }
}

/// serde_json's Display appends " at line L column C"; drop it, the admin
/// caller wants the rule.
fn strip_position(e: &serde_json::Error) -> String {
    let full = e.to_string();
    let suffix = format!(" at line {} column {}", e.line(), e.column());
    full.strip_suffix(&suffix).unwrap_or(&full).to_string()
}

fn json_rejection(rej: JsonRejection) -> AdminInputRejection {
    // A data error carries the field path (axum deserializes through
    // serde_path_to_error); a syntax or content-type error does not.
    let inner = std::error::Error::source(&rej)
        .and_then(|e| e.source())
        .and_then(|e| e.downcast_ref::<serde_path_to_error::Error<serde_json::Error>>());
    let message = match inner {
        Some(e) => input_error_message(&e.path().to_string(), &strip_position(e.inner())),
        None => rej.body_text(),
    };
    AdminInputRejection {
        // 415 (not JSON) keeps its status; every other bad body is a 400.
        status: if rej.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE {
            rej.status()
        } else {
            StatusCode::BAD_REQUEST
        },
        code: input_error_code(&message),
        message,
    }
}

fn query_rejection(rej: QueryRejection) -> AdminInputRejection {
    let text = rej.body_text();
    let message = text
        .strip_prefix("Failed to deserialize query string: ")
        .unwrap_or(&text)
        .to_string();
    AdminInputRejection {
        status: StatusCode::BAD_REQUEST,
        code: input_error_code(&message),
        message: format!("query string: {message}"),
    }
}

#[axum::async_trait]
impl<T, S> FromRequest<S> for AdminJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AdminInputRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|axum::Json(v)| AdminJson(v))
            .map_err(json_rejection)
    }
}

#[axum::async_trait]
impl<T, S> FromRequestParts<S> for AdminQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AdminInputRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(v)| AdminQuery(v))
            .map_err(query_rejection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::admin::path_guard::{AdminBucket, AdminObjectPath};
    use axum::body::Body;
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct Body1 {
        bucket: AdminBucket,
        dest_prefix: AdminObjectPath,
        count: u32,
    }

    async fn call(req: axum::http::Request<Body>) -> (StatusCode, serde_json::Value) {
        let app = Router::new()
            .route("/j", post(|AdminJson(_b): AdminJson<Body1>| async { "ok" }))
            .route(
                "/q",
                get(|AdminQuery(_b): AdminQuery<Body1>| async { "ok" }),
            );
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn json_req(body: &str) -> axum::http::Request<Body> {
        axum::http::Request::post("/j")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn path_escape_in_a_json_body_is_a_clean_json_400() {
        let (status, body) = call(json_req(
            r#"{"bucket":"releases","dest_prefix":"../x/","count":1}"#,
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_path", "{body}");
        assert_eq!(
            body["message"],
            "dest_prefix: invalid path \"../x/\": '.' and '..' segments are not allowed",
            "{body}"
        );
    }

    #[tokio::test]
    async fn bad_bucket_and_other_data_errors_get_their_codes() {
        let (s, b) = call(json_req(r#"{"bucket":"/etc","dest_prefix":"","count":1}"#)).await;
        assert_eq!(
            (s, b["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_bucket"))
        );
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .starts_with("bucket: invalid bucket name"),
            "{b}"
        );
        let (s, b) = call(json_req(
            r#"{"bucket":"releases","dest_prefix":"","count":"x"}"#,
        ))
        .await;
        assert_eq!(
            (s, b["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_request"))
        );
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .starts_with("count: invalid type"),
            "{b}"
        );
        let (s, b) = call(json_req("{not json")).await;
        assert_eq!(
            (s, b["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_request"))
        );
    }

    #[tokio::test]
    async fn non_json_content_type_keeps_415_with_a_json_body() {
        let req = axum::http::Request::post("/j")
            .body(Body::from("{}"))
            .unwrap();
        let (s, b) = call(req).await;
        assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(b["error"], "invalid_request");
    }

    #[tokio::test]
    async fn path_escape_in_a_query_is_a_clean_json_400() {
        let req = axum::http::Request::get("/q?bucket=releases&dest_prefix=..%2Fx&count=1")
            .body(Body::empty())
            .unwrap();
        let (s, b) = call(req).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b["error"], "invalid_path", "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("'..' segments are not allowed"),
            "{b}"
        );
    }

    #[test]
    fn codes_and_messages() {
        assert_eq!(input_error_code("invalid path \"..\": x"), "invalid_path");
        assert_eq!(
            input_error_code("invalid path: contains NUL"),
            "invalid_path"
        );
        assert_eq!(input_error_code("invalid bucket name: x"), "invalid_bucket");
        assert_eq!(
            input_error_code("missing field `bucket`"),
            "invalid_request"
        );
        assert_eq!(input_error_message("a.b", "rule"), "a.b: rule");
        assert_eq!(input_error_message("", "rule"), "rule");
        assert_eq!(input_error_message(".", "rule"), "rule");
    }

    /// Source guard: admin handlers take bodies and queries through
    /// `AdminJson` / `AdminQuery`, never the plain-text-rejecting axum types.
    /// (`Option<Json<T>>` stays allowed: there a bad body means "no body".)
    #[test]
    fn admin_handlers_use_admin_extractors() {
        let re =
            regex_lite::Regex::new(r"(\w+\)|\bbody)\s*:\s*(axum::extract::|axum::)?(Json|Query)<")
                .unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/admin");
        let mut bad = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    for (i, line) in src.lines().enumerate() {
                        if re.is_match(line) {
                            bad.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                        }
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "use AdminJson / AdminQuery:\n{}",
            bad.join("\n")
        );
    }
}
