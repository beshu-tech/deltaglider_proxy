// SPDX-License-Identifier: BUSL-1.1

//! Lock and lease expiry by the S3 SERVER's clock.
//!
//! A lock body carries `expires_at` on the WRITER's wall clock, so a peer
//! whose clock differs from the writer's misjudges it: a clock ahead steals a
//! live lock, a clock behind waits on a dead one. The server's own clock is
//! one clock for every node: the object's `Last-Modified` (the time of the
//! last acquire or renew) and the `Date` of the GET response (the server's
//! now) give the lock's age with no node clock in it. A body that carries
//! `ttl_secs` is expired when that age is over the TTL.
//!
//! Compatibility: writers still put `expires_at` in the body, so a node on
//! the previous release (which ignores `ttl_secs`) reads a new lock as
//! before; a body from such a node (no `ttl_secs`) is judged by its
//! `expires_at`, as before.

use aws_sdk_s3::config::ConfigBag;
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::interceptors::context::BeforeDeserializationInterceptorContextRef;
use aws_smithy_runtime_api::client::interceptors::Intercept;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use std::sync::{Arc, Mutex};

/// Captures the `Date` header of the one response of the request that it is
/// attached to (`.customize().interceptor(..)`).
#[derive(Debug, Clone, Default)]
pub struct ServerDate(Arc<Mutex<Option<i64>>>);

impl ServerDate {
    pub fn get(&self) -> Option<i64> {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Intercept for ServerDate {
    fn name(&self) -> &'static str {
        "ServerDate"
    }

    fn read_before_deserialization(
        &self,
        context: &BeforeDeserializationInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        _: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let date = context
            .response()
            .headers()
            .get("date")
            .and_then(parse_http_date);
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = date;
        Ok(())
    }
}

/// Parse an HTTP date (IMF-fixdate, `Sat, 26 Sep 2026 01:02:03 GMT`).
pub fn parse_http_date(v: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(v.trim())
        .ok()
        .map(|d| d.timestamp())
}

/// Pure: the lock's age on the server clock, from the response `Date` and
/// the object's `Last-Modified`. Both have 1 s resolution; a negative age
/// (rounding) reads as 0.
pub fn server_age(date: Option<i64>, last_modified: Option<i64>) -> Option<i64> {
    Some((date? - last_modified?).max(0))
}

/// Pure: the `expires_at` to judge a lock by, on the reader's clock `now`.
/// With `ttl_secs` in the body and a server age, the lock has
/// `ttl - age` seconds left, so it expires at `now + ttl - age`: the
/// planners' `expires_at < now` then means exactly `age > ttl`, on the
/// server's clock. Without either, the writer's `expires_at` stays.
pub fn effective_expires_at(
    written_expires_at: i64,
    ttl_secs: Option<i64>,
    age: Option<i64>,
    now: i64,
) -> i64 {
    match (ttl_secs.filter(|t| *t > 0), age) {
        (Some(ttl), Some(age)) => now.saturating_add(ttl).saturating_sub(age),
        _ => written_expires_at,
    }
}

/// GET `key` and return the output with the lock's server-clock age.
pub async fn get_with_server_age(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> Result<
    (
        aws_sdk_s3::operation::get_object::GetObjectOutput,
        Option<i64>,
    ),
    aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
> {
    let date = ServerDate::default();
    let out = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .customize()
        .interceptor(date.clone())
        .send()
        .await?;
    let age = server_age(date.get(), out.last_modified().map(|t| t.secs()));
    Ok((out, age))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_date_parses() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:01:40 GMT"), Some(100));
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn server_age_truth_table() {
        assert_eq!(server_age(Some(110), Some(100)), Some(10));
        assert_eq!(server_age(Some(99), Some(100)), Some(0));
        assert_eq!(server_age(None, Some(100)), None);
        assert_eq!(server_age(Some(100), None), None);
    }

    #[test]
    fn effective_expiry_is_ttl_minus_server_age() {
        // Server says 10 s old, TTL 120: 110 s left on any reader clock.
        assert_eq!(effective_expires_at(-5, Some(120), Some(10), 1000), 1110);
        // Exactly at the TTL: expires_at == now → still live (strict <).
        assert_eq!(effective_expires_at(0, Some(120), Some(120), 1000), 1000);
        assert_eq!(effective_expires_at(0, Some(120), Some(121), 1000), 999);
        // An old body (no ttl) or no server age: the writer's clock.
        assert_eq!(effective_expires_at(77, None, Some(10), 1000), 77);
        assert_eq!(effective_expires_at(77, Some(120), None, 1000), 77);
        assert_eq!(effective_expires_at(77, Some(0), Some(1), 1000), 77);
    }
}
