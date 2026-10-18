// SPDX-License-Identifier: BUSL-1.1

//! The admin API's error type.
//!
//! An [`AdminError`] is a status and a message. The body shape is a type
//! parameter, because the admin API answers errors in three shapes and a
//! client sees the shape of the handler it calls:
//!
//! - [`Text`] (default): `text/plain` body with the message — the shape of
//!   the former `(StatusCode, String)` handlers.
//! - [`Scrubbed`]: a client error (4xx) is `{"error": <message>}`, so the
//!   UI shows why the request was refused; a server error (5xx) is the
//!   status only, and the message goes to the log, not to the client.
//!   (It replaces the former `Bare`, which sent an empty body also for a
//!   4xx: the validation text never reached the UI.)
//!
//! No shape sends an empty body for a 4xx
//! (`no_shape_sends_an_empty_client_error`).
//! - [`JsonError`]: `{"error": <message>}` — the former
//!   `(StatusCode, Json(json!({"error": ..})))` handlers.
//! - [`CodedJson`]: `{"error": <code>, "message": <message>}` — a machine
//!   code plus the human text (CSRF refusal, declarative-IAM gate, bad
//!   request input). Built with [`AdminError::coded`].
//!
//! `From<ConfigDbError>` is the one mapping of a config-DB error to a
//! status (missing row and FOREIGN KEY failure 404, UNIQUE violation 409,
//! the rest 500), so a handler forwards a DB error with `?` and cannot pick
//! a status by hand.

use std::marker::PhantomData;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::config_db::ConfigDbError;
use crate::storage::StorageError;

/// How an [`AdminError`] renders its body.
pub trait ErrorBody {
    fn render(status: StatusCode, code: &'static str, message: String) -> Response;
}

/// `text/plain` body with the message.
pub struct Text;
/// 4xx: `{"error": <message>}`; 5xx: status only, message logged.
pub struct Scrubbed;
/// `{"error": <message>}`.
pub struct JsonError;
/// `{"error": <code>, "message": <message>}`.
pub struct CodedJson;

impl ErrorBody for Text {
    fn render(status: StatusCode, _code: &'static str, message: String) -> Response {
        (status, message).into_response()
    }
}

impl ErrorBody for Scrubbed {
    fn render(status: StatusCode, code: &'static str, message: String) -> Response {
        if status.is_server_error() {
            tracing::warn!("admin API {status}: {message}");
            return status.into_response();
        }
        JsonError::render(status, code, message)
    }
}

impl ErrorBody for JsonError {
    fn render(status: StatusCode, _code: &'static str, message: String) -> Response {
        (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
    }
}

impl ErrorBody for CodedJson {
    fn render(status: StatusCode, code: &'static str, message: String) -> Response {
        (
            status,
            axum::Json(serde_json::json!({ "error": code, "message": message })),
        )
            .into_response()
    }
}

/// An admin API error; see the module docs for the body shapes.
pub struct AdminError<B: ErrorBody = Text> {
    status: StatusCode,
    /// The machine code; only [`CodedJson`] renders it.
    code: &'static str,
    message: String,
    body: PhantomData<B>,
}

impl<B: ErrorBody> AdminError<B> {
    /// Any status. Prefer the named constructors below.
    pub(crate) fn status(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: GENERIC_CODE,
            message: message.into(),
            body: PhantomData,
        }
    }

    /// 404.
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::status(StatusCode::NOT_FOUND, message)
    }

    /// 409.
    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::status(StatusCode::CONFLICT, message)
    }

    /// 400.
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::status(StatusCode::BAD_REQUEST, message)
    }

    /// 403.
    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::status(StatusCode::FORBIDDEN, message)
    }

    /// 500.
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::status(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// 503.
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::status(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    /// 503 "config DB not available": the answer of a handler that needs
    /// the config DB on an instance without one.
    pub(crate) fn no_config_db() -> Self {
        Self::unavailable("config DB not available")
    }

    pub(crate) fn no_jobs_db() -> Self {
        Self::unavailable("jobs DB not available")
    }

    pub(crate) fn status_code(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

/// The code of an error built without one (a `?` on a DB or storage
/// error into a [`CodedJson`] handler).
const GENERIC_CODE: &str = "error";

impl AdminError<CodedJson> {
    /// `{"error": code, "message": message}` with `status`.
    pub(crate) fn coded(
        code: &'static str,
        status: StatusCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            ..Self::status(status, message)
        }
    }
}

impl<B: ErrorBody> std::fmt::Debug for AdminError<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminError")
            .field("status", &self.status)
            .field("code", &self.code)
            .field("message", &self.message)
            .finish()
    }
}

impl<B: ErrorBody> std::fmt::Display for AdminError<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl<B: ErrorBody> IntoResponse for AdminError<B> {
    fn into_response(self) -> Response {
        B::render(self.status, self.code, self.message)
    }
}

impl<B: ErrorBody> From<ConfigDbError> for AdminError<B> {
    fn from(e: ConfigDbError) -> Self {
        let status = db_error_status(&e);
        Self::status(status, db_error_message(status, &e))
    }
}

/// Pure: the message of a config-DB error. A SQLite failure that is the
/// caller's (404, 409) is said in words, because the client shows it; the
/// rest keeps the error's own text.
fn db_error_message(status: StatusCode, e: &ConfigDbError) -> String {
    match (status, e) {
        (StatusCode::CONFLICT, ConfigDbError::Sqlite(rusqlite::Error::SqliteFailure(_, msg))) => {
            match msg {
                Some(m) => format!("the name is already in use ({m})"),
                None => "the name is already in use".to_string(),
            }
        }
        (StatusCode::NOT_FOUND, ConfigDbError::Sqlite(rusqlite::Error::SqliteFailure(..))) => {
            "a user or group that the request names does not exist".to_string()
        }
        (StatusCode::NOT_FOUND, ConfigDbError::Sqlite(_)) => "not found".to_string(),
        _ => e.to_string(),
    }
}

/// A storage failure is the server's 500, with the error text.
impl<B: ErrorBody> From<StorageError> for AdminError<B> {
    fn from(e: StorageError) -> Self {
        Self::internal(e.to_string())
    }
}

/// Pure: the HTTP status of a config-DB error. A missing row is the
/// caller's 404 (also a FOREIGN KEY failure: the request names a user or
/// group that does not exist), a UNIQUE violation its 409; only the rest
/// is a 500.
fn db_error_status(e: &ConfigDbError) -> StatusCode {
    use crate::config_db::{classify_sqlite_error, SqliteErrorClass};
    match e {
        ConfigDbError::NotFound(_) => StatusCode::NOT_FOUND,
        ConfigDbError::Refused(_) => StatusCode::CONFLICT,
        ConfigDbError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
        {
            StatusCode::NOT_FOUND
        }
        ConfigDbError::Sqlite(se) => match classify_sqlite_error(se) {
            SqliteErrorClass::NotFound => StatusCode::NOT_FOUND,
            SqliteErrorClass::Conflict => StatusCode::CONFLICT,
            SqliteErrorClass::Other => StatusCode::INTERNAL_SERVER_ERROR,
        },
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parts(r: Response) -> (StatusCode, Option<String>, Vec<u8>) {
        let status = r.status();
        let ct = r
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, ct, body)
    }

    /// Each shape answers byte for byte what the legacy return type did:
    /// status, content type and body.
    #[tokio::test]
    async fn shapes_render_like_the_legacy_return_types() {
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let msg = "rule \"x\" not found".to_string();
            assert_eq!(
                parts(AdminError::<Text>::status(status, msg.clone()).into_response()).await,
                parts((status, msg.clone()).into_response()).await,
            );
            if status.is_client_error() {
                assert_eq!(
                    parts(AdminError::<Scrubbed>::status(status, msg.clone()).into_response())
                        .await,
                    parts(AdminError::<JsonError>::status(status, msg.clone()).into_response())
                        .await,
                    "a Scrubbed client error carries its message as JSON"
                );
            }
            assert_eq!(
                parts(AdminError::<JsonError>::status(status, msg.clone()).into_response()).await,
                parts(
                    (
                        status,
                        axum::Json(serde_json::json!({ "error": msg.clone() }))
                    )
                        .into_response()
                )
                .await,
            );
        }
    }

    /// A Scrubbed server error keeps its detail off the wire.
    #[tokio::test]
    async fn scrubbed_server_errors_send_no_detail() {
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let r = AdminError::<Scrubbed>::status(status, "sqlite: disk I/O at /var/x")
                .into_response();
            assert_eq!(parts(r).await, parts(status.into_response()).await);
        }
    }

    /// Guard: every body shape carries the message of a client error
    /// (4xx), so the UI can show why the request was refused. A shape that
    /// sends an empty 4xx body fails here.
    #[tokio::test]
    async fn no_shape_sends_an_empty_client_error() {
        async fn body_of<B: ErrorBody>(status: StatusCode) -> String {
            let r = AdminError::<B>::status(status, "invalid permissions: x").into_response();
            String::from_utf8(parts(r).await.2).unwrap()
        }
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            for body in [
                body_of::<Text>(status).await,
                body_of::<Scrubbed>(status).await,
                body_of::<JsonError>(status).await,
                body_of::<CodedJson>(status).await,
            ] {
                assert!(
                    body.contains("invalid permissions: x"),
                    "{status}: body without the message: {body:?}"
                );
            }
        }
        // A new shape must join the list above.
        let shapes = crate::source_scan::prod_sources("src")
            .into_iter()
            .map(|(_, text)| {
                text.lines()
                    .filter(|l| l.starts_with("impl ErrorBody for "))
                    .count()
            })
            .sum::<usize>();
        assert_eq!(shapes, 4, "add the new ErrorBody shape to this guard");
    }

    /// `CodedJson` answers what the hand-built `{error, message}` bodies
    /// (CSRF, declarative gate, extractor rejection) did.
    #[tokio::test]
    async fn coded_json_renders_like_the_hand_built_bodies() {
        for status in [StatusCode::BAD_REQUEST, StatusCode::FORBIDDEN] {
            let e = AdminError::coded("iam_declarative", status, "edit the YAML");
            assert_eq!(
                parts(e.into_response()).await,
                parts(
                    (
                        status,
                        axum::Json(serde_json::json!({
                            "error": "iam_declarative",
                            "message": "edit the YAML",
                        }))
                    )
                        .into_response()
                )
                .await,
            );
        }
    }

    #[test]
    fn named_constructors_pick_their_status() {
        type E = AdminError<Text>;
        assert_eq!(E::not_found("").status_code(), StatusCode::NOT_FOUND);
        assert_eq!(E::conflict("").status_code(), StatusCode::CONFLICT);
        assert_eq!(E::invalid("").status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(E::forbidden("").status_code(), StatusCode::FORBIDDEN);
        assert_eq!(
            E::internal("").status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let no_db = E::no_config_db();
        assert_eq!(no_db.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(no_db.message(), "config DB not available");
    }

    /// Every handler that finds no config DB answers the one
    /// [`AdminError::no_config_db`] (503): a hand-written message drifted to
    /// 404 in some handlers.
    #[test]
    fn a_missing_config_db_is_always_no_config_db() {
        let mut bad = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src") {
            if file == "src/api/admin/error.rs" {
                continue;
            }
            for (n, line) in text.lines().enumerate() {
                let l = line.to_ascii_lowercase();
                let names_it = l.contains("\"config db") || l.contains(" config db");
                let missing = ["not available", "unavailable", "not initialised"]
                    .iter()
                    .any(|m| l.contains(m));
                if line.contains('"') && names_it && missing {
                    bad.push(format!("{file}:{}: {}", n + 1, line.trim()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "use AdminError::no_config_db():\n{}",
            bad.join("\n")
        );
    }

    /// `?` on a DB error keeps the legacy `db_error_reply` answer: the
    /// mapped status and the error's Display text.
    #[test]
    fn db_error_converts_to_its_status_and_text() {
        let e: AdminError = ConfigDbError::NotFound("provider 9".into()).into();
        assert_eq!(e.status_code(), StatusCode::NOT_FOUND);
        assert_eq!(e.message(), "Not found: provider 9");
        let e: AdminError = ConfigDbError::Sqlite(rusqlite::Error::QueryReturnedNoRows).into();
        assert_eq!(
            (e.status_code(), e.message()),
            (StatusCode::NOT_FOUND, "not found")
        );
        let unique = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
            },
            Some("UNIQUE constraint failed: groups.name".into()),
        );
        let e: AdminError = ConfigDbError::Sqlite(unique).into();
        assert_eq!(
            (e.status_code(), e.message()),
            (
                StatusCode::CONFLICT,
                "the name is already in use (UNIQUE constraint failed: groups.name)"
            )
        );
    }

    #[test]
    fn storage_error_converts_to_a_500_with_its_text() {
        let e: AdminError<JsonError> = StorageError::NotFound("k".into()).into();
        assert_eq!(e.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.message(), StorageError::NotFound("k".into()).to_string());
    }

    #[test]
    fn db_error_status_truth_table() {
        assert_eq!(
            db_error_status(&ConfigDbError::NotFound("provider 9".into())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            db_error_status(&ConfigDbError::Sqlite(rusqlite::Error::QueryReturnedNoRows)),
            StatusCode::NOT_FOUND
        );
        let unique = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
            },
            Some("UNIQUE constraint failed: auth_providers.name".into()),
        );
        assert_eq!(
            db_error_status(&ConfigDbError::Sqlite(unique)),
            StatusCode::CONFLICT
        );
        let fk = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
            },
            Some("FOREIGN KEY constraint failed".into()),
        );
        assert_eq!(
            db_error_status(&ConfigDbError::Sqlite(fk)),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            db_error_status(&ConfigDbError::Other("broken".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
