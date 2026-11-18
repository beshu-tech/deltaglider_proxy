// SPDX-License-Identifier: BUSL-1.1

//! Read the body of a signed request that was refused before its body was read.
//!
//! hyper drains only what is already buffered; otherwise it closes the read
//! side after the response, and a reverse proxy reports a reset instead of
//! the 4xx/5xx (hyper-1 `proto/h1/conn.rs` `poll_drain_or_close_read`).

use crate::api::auth::AuthOutcome;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

/// Most body bytes read after a refusal.
pub const DRAIN_CAP_BYTES: u64 = 64 * 1024 * 1024;
/// Longest time spent reading them.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// Pure: whether a response is held back until the request body is read.
/// Only a verified, non-presigned request is drained: an anonymous caller
/// (or the holder of a shared link) must not make the proxy read 64 MiB.
pub fn should_drain(
    status: StatusCode,
    verified: bool,
    presigned: bool,
    declared_len: Option<u64>,
) -> bool {
    status.as_u16() >= 400
        && verified
        && !presigned
        && declared_len.is_none_or(|n| n <= DRAIN_CAP_BYTES)
}

/// How a drain ended.
#[derive(Debug, PartialEq, Eq)]
pub enum DrainEnd {
    Finished,
    CapReached,
    TimedOut,
    Failed,
}

/// Read and discard `body` up to `cap` bytes and `deadline`.
pub async fn drain_capped<B>(mut body: B, cap: u64, deadline: Duration) -> DrainEnd
where
    B: HttpBody + Unpin,
    B::Data: bytes::Buf,
{
    let read = async {
        let mut total = 0u64;
        loop {
            match futures::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
                None => return DrainEnd::Finished,
                Some(Err(_)) => return DrainEnd::Failed,
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        total += bytes::Buf::remaining(data) as u64;
                        if total >= cap {
                            return DrainEnd::CapReached;
                        }
                    }
                }
            }
        }
    };
    tokio::time::timeout(deadline, read)
        .await
        .unwrap_or(DrainEnd::TimedOut)
}

/// The request body, still reachable by the middleware after the handlers
/// took it.
struct SharedBody(Arc<Mutex<Option<Body>>>);

impl HttpBody for SharedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        match self.0.lock().expect("body lock").as_mut() {
            Some(body) => Pin::new(body).poll_frame(cx),
            None => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.0
            .lock()
            .expect("body lock")
            .as_ref()
            .is_none_or(|b| b.is_end_stream())
    }

    fn size_hint(&self) -> SizeHint {
        self.0
            .lock()
            .expect("body lock")
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), |b| b.size_hint())
    }
}

/// Middleware: after an error response to a verified request, read the rest
/// of the request body before the response goes out.
pub async fn drain_after_refusal(mut request: Request, next: Next) -> Response {
    let outcome = AuthOutcome::default();
    request.extensions_mut().insert(outcome.clone());
    let presigned =
        crate::api::auth::has_presigned_query_params(request.uri().query().unwrap_or(""));
    let declared_len = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let shared = Arc::new(Mutex::new(None));
    let request = request.map(|body| {
        *shared.lock().expect("body lock") = Some(body);
        Body::new(SharedBody(shared.clone()))
    });
    let response = next.run(request).await;
    if should_drain(
        response.status(),
        outcome.is_verified(),
        presigned,
        declared_len,
    ) {
        let rest = shared.lock().expect("body lock").take();
        if let Some(body) = rest {
            let end = drain_capped(body, DRAIN_CAP_BYTES, DRAIN_DEADLINE).await;
            tracing::debug!(?end, status = %response.status(), "drained refused request body");
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_drain_truth_table() {
        let big = Some(DRAIN_CAP_BYTES + 1);
        let ok = StatusCode::OK;
        let denied = StatusCode::FORBIDDEN;
        // (status, verified, presigned, declared, want)
        let rows = [
            (denied, true, false, Some(10), true),
            (denied, true, false, None, true),
            (denied, true, false, Some(DRAIN_CAP_BYTES), true),
            (denied, true, false, big, false),
            (denied, false, false, Some(10), false),
            (denied, true, true, Some(10), false),
            (ok, true, false, Some(10), false),
            (StatusCode::SERVICE_UNAVAILABLE, true, false, Some(10), true),
            (StatusCode::BAD_REQUEST, false, true, None, false),
        ];
        for (status, verified, presigned, declared, want) in rows {
            assert_eq!(
                should_drain(status, verified, presigned, declared),
                want,
                "{status} verified={verified} presigned={presigned} declared={declared:?}"
            );
        }
    }

    #[tokio::test]
    async fn drain_reads_a_short_body_to_the_end() {
        let body = Body::from(vec![0u8; 1000]);
        assert_eq!(
            drain_capped(body, 4096, Duration::from_secs(5)).await,
            DrainEnd::Finished
        );
    }

    #[tokio::test]
    async fn drain_stops_at_the_byte_cap() {
        let chunks = (0..100).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![0u8; 100])));
        let body = Body::from_stream(futures::stream::iter(chunks));
        assert_eq!(
            drain_capped(body, 1000, Duration::from_secs(5)).await,
            DrainEnd::CapReached
        );
    }

    #[tokio::test(start_paused = true)]
    async fn drain_stops_at_the_deadline() {
        let body = Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>());
        assert_eq!(
            drain_capped(body, 1 << 20, Duration::from_secs(30)).await,
            DrainEnd::TimedOut
        );
    }
}
