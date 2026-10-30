// SPDX-License-Identifier: BUSL-1.1

//! The admin API's error type.
//!
//! An [`AdminError`] is a status, a stable machine code and a message. It
//! has one body, `{"error": <code>, "message": <text>}`, with
//! `Content-Type: application/json`.
//!
//! `error` carries the code, not the text: the codes that clients match
//! on (`iam_declarative`, `admin_session_required`, `csrf_rejected`,
//! `invalid_path`, ...) were already in `error`, and a client that shows
//! only `error` still shows a stable word. The text is in `message`.
//!
//! A server error (5xx) never sends its text: the detail goes to the log,
//! and the body is `{"error": "internal", "message": <generic>}`. Two
//! constructors take a message written for the client instead:
//! [`AdminError::unavailable`] (a backend that the health check closed, no
//! config DB) and [`AdminError::internal_explained`] (what failed and what
//! state it left; the detail still goes to the log only).
//!
//! `From<ConfigDbError>` is the one mapping of a config-DB error to a
//! status (missing row and FOREIGN KEY failure 404, UNIQUE violation 409,
//! the rest 500), so a handler forwards a DB error with `?` and cannot pick
//! a status by hand.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::config_db::ConfigDbError;
use crate::storage::StorageError;

/// The code of every scrubbed server error.
pub(crate) const INTERNAL_CODE: &str = "internal";

/// The message of every scrubbed server error.
pub(crate) const INTERNAL_MESSAGE: &str = "internal server error; the server log has the details";

/// Pure: the code of an error built without one.
pub(crate) fn code_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "invalid_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::CONFLICT => "conflict",
        StatusCode::GONE => "gone",
        StatusCode::PRECONDITION_FAILED => "precondition_failed",
        StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        StatusCode::UNPROCESSABLE_ENTITY => "unprocessable",
        StatusCode::PRECONDITION_REQUIRED => "precondition_required",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        s if s.is_server_error() => INTERNAL_CODE,
        _ => "invalid_request",
    }
}

/// An admin API error; see the module docs for the body.
pub struct AdminError {
    status: StatusCode,
    code: &'static str,
    message: String,
    /// A 5xx whose message is written for the client
    /// ([`AdminError::unavailable`]). Every other 5xx is scrubbed.
    public: bool,
}

impl AdminError {
    /// Any status, with the code of the status. Prefer the named
    /// constructors below. The message of a 5xx goes to the log only.
    pub(crate) fn status(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code_for_status(status),
            message: message.into(),
            public: false,
        }
    }

    /// A specific machine code, for a refusal that a client tells apart
    /// from the other errors of its status.
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

    /// 404 (public: the binary answers an unmatched admin path with it).
    pub fn not_found(message: impl Into<String>) -> Self {
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

    /// 500. The detail goes to the log; the client gets [`INTERNAL_MESSAGE`].
    pub(crate) fn internal(detail: impl Into<String>) -> Self {
        Self::status(StatusCode::INTERNAL_SERVER_ERROR, detail)
    }

    /// 500 with a message written for the client (what failed and what
    /// state it left); `detail` goes to the log only.
    pub(crate) fn internal_explained(
        message: impl Into<String>,
        detail: impl std::fmt::Display,
    ) -> Self {
        let message = message.into();
        tracing::warn!("admin API 500: {message}: {detail}");
        Self {
            public: true,
            ..Self::internal(message)
        }
    }

    /// 503 with a message that the client reads. Write it for the
    /// operator; never pass the text of a dependency's error (log it and
    /// use [`AdminError::internal`]).
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            public: true,
            ..Self::status(StatusCode::SERVICE_UNAVAILABLE, message)
        }
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

    /// The message, also of a scrubbed 5xx (for the log, and for callers
    /// that wrap the error in their own body).
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// The machine code that the client receives.
    #[cfg(test)]
    pub(crate) fn code(&self) -> &'static str {
        self.wire().0
    }

    fn scrubbed(&self) -> bool {
        self.status.is_server_error() && !self.public
    }

    /// Pure: the `(code, message)` that the client receives.
    fn wire(&self) -> (&'static str, &str) {
        if self.scrubbed() {
            (INTERNAL_CODE, INTERNAL_MESSAGE)
        } else {
            (self.code, &self.message)
        }
    }
}

impl std::fmt::Debug for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminError")
            .field("status", &self.status)
            .field("code", &self.code)
            .field("message", &self.message)
            .finish()
    }
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        if self.scrubbed() {
            tracing::warn!("admin API {}: {}", self.status, self.message);
        }
        let (code, message) = self.wire();
        (
            self.status,
            axum::Json(serde_json::json!({ "error": code, "message": message })),
        )
            .into_response()
    }
}

impl From<ConfigDbError> for AdminError {
    fn from(e: ConfigDbError) -> Self {
        let status = db_error_status(&e);
        Self::status(status, db_error_message(status, &e))
    }
}

/// Pure: the message of a config-DB error. A SQLite failure that is the
/// caller's (404, 409) is said in words, because the client shows it; the
/// rest keeps the error's own text (a 500, so only the log sees it).
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

/// A storage failure is the server's 500; its text goes to the log.
impl From<StorageError> for AdminError {
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

    async fn parts(r: Response) -> (StatusCode, Option<String>, serde_json::Value) {
        let status = r.status();
        let ct = r
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, ct, serde_json::from_slice(&body).unwrap())
    }

    const CLIENT_ERRORS: [StatusCode; 9] = [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::CONFLICT,
        StatusCode::PRECONDITION_FAILED,
        StatusCode::PAYLOAD_TOO_LARGE,
        StatusCode::UNPROCESSABLE_ENTITY,
        StatusCode::TOO_MANY_REQUESTS,
    ];

    /// Guard: every client error (4xx) is the one JSON body with a code
    /// and its message, so the UI can show why the request was refused.
    #[tokio::test]
    async fn every_client_error_carries_its_code_and_message() {
        for status in CLIENT_ERRORS {
            let r = AdminError::status(status, "invalid permissions: x").into_response();
            let (st, ct, body) = parts(r).await;
            assert_eq!(st, status);
            assert_eq!(ct.as_deref(), Some("application/json"));
            assert_eq!(
                body,
                serde_json::json!({
                    "error": code_for_status(status),
                    "message": "invalid permissions: x",
                }),
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn a_coded_error_sends_its_code() {
        let e = AdminError::coded("iam_declarative", StatusCode::FORBIDDEN, "edit the YAML");
        assert_eq!(
            parts(e.into_response()).await.2,
            serde_json::json!({ "error": "iam_declarative", "message": "edit the YAML" })
        );
    }

    /// A server error keeps its detail off the wire, whatever built it.
    #[tokio::test]
    async fn server_errors_send_no_detail() {
        let detail = "sqlite: disk I/O at /var/x";
        let scrubbed = serde_json::json!({ "error": "internal", "message": INTERNAL_MESSAGE });
        for e in [
            AdminError::internal(detail),
            AdminError::status(StatusCode::SERVICE_UNAVAILABLE, detail),
            AdminError::status(StatusCode::BAD_GATEWAY, detail),
            AdminError::coded("x", StatusCode::INTERNAL_SERVER_ERROR, detail),
            ConfigDbError::Other(detail.into()).into(),
            StorageError::Other(detail.into()).into(),
        ] {
            let status = e.status_code();
            let (st, _, body) = parts(e.into_response()).await;
            assert_eq!(st, status);
            assert_eq!(body, scrubbed);
        }
    }

    /// The two 5xx constructors whose message the client reads.
    #[tokio::test]
    async fn written_server_messages_reach_the_client() {
        let (st, _, body) = parts(AdminError::no_jobs_db().into_response()).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            serde_json::json!({ "error": "unavailable", "message": "jobs DB not available" })
        );
        let e = AdminError::internal_explained("not saved (no change)", "EACCES /etc/x.yaml");
        let (st, _, body) = parts(e.into_response()).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            serde_json::json!({ "error": "internal", "message": "not saved (no change)" })
        );
    }

    #[test]
    fn every_status_has_a_snake_case_code() {
        for n in 400u16..600 {
            let Ok(status) = StatusCode::from_u16(n) else {
                continue;
            };
            let code = code_for_status(status);
            assert!(
                !code.is_empty() && code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{status}: {code:?}"
            );
            if status.is_server_error() && status != StatusCode::SERVICE_UNAVAILABLE {
                assert_eq!(code, INTERNAL_CODE);
            }
        }
    }

    #[test]
    fn named_constructors_pick_their_status() {
        type E = AdminError;
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

    /// `?` on a DB error keeps the mapped status and the error's text (a
    /// 500 sends it to the log only).
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
            (e.status_code(), e.code(), e.message()),
            (
                StatusCode::CONFLICT,
                "conflict",
                "the name is already in use (UNIQUE constraint failed: groups.name)"
            )
        );
    }

    #[test]
    fn storage_error_converts_to_a_500_with_its_text() {
        let e: AdminError = StorageError::NotFound("k".into()).into();
        assert_eq!(e.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.code(), INTERNAL_CODE);
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
