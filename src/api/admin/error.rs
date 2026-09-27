// SPDX-License-Identifier: BUSL-1.1

//! The admin API's error type.
//!
//! An [`AdminError`] is a status and a message. The body shape is a type
//! parameter, because the admin API answers errors in three shapes and a
//! client sees the shape of the handler it calls:
//!
//! - [`Text`] (default): `text/plain` body with the message — the shape of
//!   the former `(StatusCode, String)` handlers.
//! - [`Bare`]: status only, empty body — the former `StatusCode` handlers.
//!   The message goes to the log, not to the client.
//! - [`JsonError`]: `{"error": <message>}` — the former
//!   `(StatusCode, Json(json!({"error": ..})))` handlers.
//!
//! `From<ConfigDbError>` is the one mapping of a config-DB error to a
//! status (missing row and FOREIGN KEY failure 404, UNIQUE violation 409,
//! the rest 500), so a handler forwards a DB error with `?` and cannot pick
//! a status by hand.

use std::marker::PhantomData;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::config_db::ConfigDbError;

/// How an [`AdminError`] renders its body.
pub trait ErrorBody {
    fn render(status: StatusCode, message: String) -> Response;
}

/// `text/plain` body with the message.
pub struct Text;
/// Status only, empty body.
pub struct Bare;
/// `{"error": <message>}`.
pub struct JsonError;

impl ErrorBody for Text {
    fn render(status: StatusCode, message: String) -> Response {
        (status, message).into_response()
    }
}

impl ErrorBody for Bare {
    fn render(status: StatusCode, _message: String) -> Response {
        status.into_response()
    }
}

impl ErrorBody for JsonError {
    fn render(status: StatusCode, message: String) -> Response {
        (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
    }
}

/// An admin API error; see the module docs for the body shapes.
pub struct AdminError<B: ErrorBody = Text> {
    status: StatusCode,
    message: String,
    body: PhantomData<B>,
}

impl<B: ErrorBody> AdminError<B> {
    /// Any status. Prefer the named constructors below.
    pub(crate) fn status(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
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

    pub(crate) fn status_code(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl<B: ErrorBody> std::fmt::Debug for AdminError<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminError")
            .field("status", &self.status)
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
        B::render(self.status, self.message)
    }
}

impl<B: ErrorBody> From<ConfigDbError> for AdminError<B> {
    fn from(e: ConfigDbError) -> Self {
        Self::status(db_error_status(&e), e.to_string())
    }
}

/// A helper that still answers a bare status (e.g. a shared rebuild step)
/// feeds a [`Bare`] handler through `?`.
impl From<StatusCode> for AdminError<Bare> {
    fn from(status: StatusCode) -> Self {
        Self::status(status, String::new())
    }
}

/// A helper that still answers `(StatusCode, String)` feeds a [`Text`]
/// handler through `?`.
impl From<(StatusCode, String)> for AdminError<Text> {
    fn from((status, message): (StatusCode, String)) -> Self {
        Self::status(status, message)
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
            assert_eq!(
                parts(AdminError::<Bare>::status(status, msg.clone()).into_response()).await,
                parts(status.into_response()).await,
            );
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

    /// `?` on a DB error keeps the legacy `db_error_reply` answer: the
    /// mapped status and the error's Display text.
    #[test]
    fn db_error_converts_to_its_status_and_text() {
        let e: AdminError = ConfigDbError::NotFound("provider 9".into()).into();
        assert_eq!(e.status_code(), StatusCode::NOT_FOUND);
        assert_eq!(e.message(), "Not found: provider 9");
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
