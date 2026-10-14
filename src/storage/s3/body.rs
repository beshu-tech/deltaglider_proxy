// SPDX-License-Identifier: BUSL-1.1

//! GET bodies that survive a read that breaks off (issue #102).
//!
//! The SDK retries a GET until the response headers arrive. A body that
//! breaks off after that reaches the caller as an error: the backend stops
//! sending and stalled-stream protection ends the read after its grace
//! period, or the connection resets. Every GET body of this backend is read
//! through [`S3Backend::body_stream`], which resumes such a body instead: a
//! ranged GET from the next byte, pinned with `If-Match` to the ETag of the
//! first response, so the bytes after the break come from the same object
//! version.

use super::*;
use crate::storage::traits::object_changed_while_read;
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::operation::get_object::GetObjectOutput;

/// Resumes of one GET body before its read fails.
pub(super) const BODY_RESUMES: u32 = 3;

/// Wait before each resume, in ms (index = resumes done before it).
const RESUME_BACKOFF_MS: [u64; BODY_RESUMES as usize] = [200, 1_000, 3_000];

/// GET bodies that broke off and resumed with a ranged GET.
pub static BACKEND_GET_BODY_RESUMES: std::sync::LazyLock<prometheus::IntCounter> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounter::new(
            "deltaglider_backend_get_body_resumes_total",
            "S3 GET bodies that broke off mid-read and resumed with a ranged GET",
        )
        .expect("valid metric")
    });

/// What the headers of a GET response say about its body: the object bytes
/// `[start, end)` that it carries, and the object's size when they name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BodySpan {
    pub(super) start: u64,
    pub(super) end: u64,
    /// `None` for a `Content-Range` total of `*`.
    pub(super) total: Option<u64>,
}

impl BodySpan {
    /// The number of bytes in the body.
    pub(super) fn len(&self) -> u64 {
        self.end - self.start
    }
}

/// The span of a GET response body: from `Content-Range` on a partial
/// response, else the whole object from `Content-Length`. A store that
/// sends the body chunked has no `Content-Length`: the span of a partial
/// body then comes from `Content-Range` alone, and a whole body has no
/// known span. A missing `Content-Length` is never a length of 0. `None`
/// when the headers do not say, or disagree. Pure.
pub(super) fn body_span(
    content_range: Option<&str>,
    content_length: Option<i64>,
) -> Option<BodySpan> {
    let len = match content_length {
        Some(n) => Some(u64::try_from(n).ok()?),
        None => None,
    };
    let Some(range) = content_range else {
        let len = len?;
        return Some(BodySpan {
            start: 0,
            end: len,
            total: Some(len),
        });
    };
    // "bytes <first>-<last>/<total or *>"
    let (span, total) = range.strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse::<u64>().ok()?),
    };
    let span_len = last.checked_sub(first)?.checked_add(1)?;
    if len.is_some_and(|len| len != span_len) || total.is_some_and(|t| last >= t) {
        return None;
    }
    Some(BodySpan {
        start: first,
        end: last + 1,
        total,
    })
}

/// Where a body that broke off resumes from.
struct ResumePoint {
    client: Client,
    bucket: String,
    key: String,
    etag: String,
    /// The object offset of the next byte the reader has not received.
    next: u64,
    /// One past the object offset of the last byte the body carries.
    end: u64,
    resumes: u32,
}

struct BodyState {
    body: ByteStream,
    /// `None` when the first response had no ETag or no usable length:
    /// the body cannot resume, and a break fails the read as before.
    resume: Option<ResumePoint>,
    bucket: String,
    key: String,
    /// Bytes received so far (for the error text of a body that cannot resume).
    received: u64,
    done: bool,
}

impl BodyState {
    async fn next_chunk(&mut self) -> Option<Result<Bytes, StorageError>> {
        if self.done {
            return None;
        }
        loop {
            match self.body.try_next().await {
                Ok(Some(chunk)) => {
                    self.received += chunk.len() as u64;
                    if let Some(r) = self.resume.as_mut() {
                        r.next += chunk.len() as u64;
                    }
                    return Some(Ok(chunk));
                }
                Ok(None) => return None,
                Err(e) => {
                    // The Display of a body error is only "streaming error";
                    // the cause (a stall, a reset) is in its source chain.
                    let cause = DisplayErrorContext(&e).to_string();
                    match self.resume_after(&cause).await {
                        Ok(body) => self.body = body,
                        Err(err) => {
                            self.done = true;
                            return Some(Err(err));
                        }
                    }
                }
            }
        }
    }

    /// The body of a ranged GET for the bytes not yet received, or the
    /// error that ends the read.
    async fn resume_after(&mut self, cause: &str) -> Result<ByteStream, StorageError> {
        let fail = |at: u64, why: &str| {
            StorageError::Transient(format!(
                "Failed to read response body of {}/{} at byte {at}: {cause}{why}",
                self.bucket, self.key
            ))
        };
        let Some(r) = self.resume.as_mut() else {
            return Err(fail(self.received, ""));
        };
        if r.next >= r.end {
            // Every byte arrived; the error is about the body as a whole.
            return Err(fail(r.next, ""));
        }
        if r.resumes >= BODY_RESUMES {
            return Err(fail(
                r.next,
                &format!(" (gave up after {BODY_RESUMES} resumes)"),
            ));
        }
        warn!(
            "S3 GET {}/{}: the body broke off at byte {} of {} ({cause}); resuming ({}/{BODY_RESUMES})",
            r.bucket,
            r.key,
            r.next,
            r.end,
            r.resumes + 1
        );
        BACKEND_GET_BODY_RESUMES.inc();
        tokio::time::sleep(std::time::Duration::from_millis(
            RESUME_BACKOFF_MS[r.resumes as usize],
        ))
        .await;
        r.resumes += 1;
        let resp = r
            .client
            .get_object()
            .bucket(&r.bucket)
            .key(&r.key)
            .range(format!("bytes={}-{}", r.next, r.end - 1))
            .if_match(&r.etag)
            .send()
            .await;
        let at = r.next;
        let resp = match resp {
            Ok(resp) => resp,
            Err(e) if e.raw_response().map(|raw| raw.status().as_u16()) == Some(412) => {
                return Err(fail(at, " (the object changed while it was read)"));
            }
            Err(e) => {
                return Err(fail(
                    at,
                    &format!(" (the resume failed: {})", DisplayErrorContext(&e)),
                ));
            }
        };
        // A backend that ignores If-Match still names the version it sent.
        if resp.e_tag() != Some(r.etag.as_str()) {
            return Err(fail(at, " (the object changed while it was read)"));
        }
        if body_span(resp.content_range(), resp.content_length()).map(|s| (s.start, s.end))
            != Some((r.next, r.end))
        {
            return Err(fail(
                at,
                &format!(
                    " (the backend answered the resume with Content-Range {:?})",
                    resp.content_range()
                ),
            ));
        }
        Ok(resp.body)
    }
}

impl S3Backend {
    /// The body of a GET response as a stream of chunks. A read that breaks
    /// off resumes from the next byte (up to [`BODY_RESUMES`] times); the
    /// stream ends after the first error it yields.
    pub(super) fn body_stream(
        &self,
        bucket: &str,
        key: &str,
        response: GetObjectOutput,
    ) -> BoxStream<'static, Result<Bytes, StorageError>> {
        let span = body_span(response.content_range(), response.content_length());
        let resume = match (response.e_tag(), span) {
            (Some(etag), Some(BodySpan { start, end, .. })) => Some(ResumePoint {
                client: self.client.clone(),
                bucket: bucket.to_string(),
                key: key.to_string(),
                etag: etag.to_string(),
                next: start,
                end,
                resumes: 0,
            }),
            _ => None,
        };
        let state = BodyState {
            body: response.body,
            resume,
            bucket: bucket.to_string(),
            key: key.to_string(),
            received: 0,
            done: false,
        };
        Box::pin(futures::stream::unfold(state, |mut state| async move {
            let item = state.next_chunk().await?;
            Some((item, state))
        }))
    }

    /// The size of the object `key` that the GET `response` read: the
    /// total of its Content-Range, else the Content-Length of a whole body.
    /// A response that names neither (a chunked whole body) takes the size
    /// from a HEAD, which must name the same ETag: else the object changed
    /// between the two requests, and the read is retryable (503 SlowDown).
    pub(super) async fn object_size_of(
        &self,
        bucket: &str,
        key: &str,
        response: &GetObjectOutput,
    ) -> Result<u64, StorageError> {
        if let Some(total) =
            body_span(response.content_range(), response.content_length()).and_then(|s| s.total)
        {
            return Ok(total);
        }
        let (_, head) = self.head_object_response(bucket, key).await?;
        if head.e_tag() != response.e_tag() {
            return Err(object_changed_while_read(bucket, key));
        }
        head.content_length()
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| {
                StorageError::S3(format!(
                    "{bucket}/{key}: neither the GET nor the HEAD response names the size"
                ))
            })
    }

    /// One ranged GET of `key` for the bytes `start..=end` (`end` at
    /// `u64::MAX - 1` or past the object: to its end). Returns the
    /// response, the number of bytes its body carries, and the object's
    /// size. The length comes from Content-Range (a chunked body has no
    /// Content-Length). A backend that ignored the range (no
    /// Content-Range) answers only when the range covers the whole object.
    pub(super) async fn ranged_get(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: u64,
    ) -> Result<(GetObjectOutput, u64, u64), StorageError> {
        let range_header = if end >= u64::MAX - 1 {
            format!("bytes={start}-")
        } else {
            format!("bytes={start}-{end}")
        };
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .range(&range_header)
            .send()
            .await
            .map_err(|e| self.classify_get(bucket, key, &e))?;
        let ignored = || {
            StorageError::S3(format!(
                "backend ignored {range_header} for {bucket}/{key} (Content-Range {:?})",
                response.content_range()
            ))
        };
        let span = body_span(response.content_range(), response.content_length());
        let (len, total) = match span {
            Some(span) if response.content_range().is_some() => {
                if span.start != start {
                    return Err(ignored());
                }
                let total = match span.total {
                    Some(total) => total,
                    None => self.object_size_of(bucket, key, &response).await?,
                };
                (span.len(), total)
            }
            // No Content-Range: the backend sent the whole object.
            _ => {
                let total = self.object_size_of(bucket, key, &response).await?;
                if start != 0 || end.saturating_add(1) < total {
                    return Err(ignored());
                }
                (total, total)
            }
        };
        debug!(
            "S3 GET range {}/{} ({}, {} of {} bytes)",
            bucket, key, range_header, len, total
        );
        Ok((response, len, total))
    }

    /// The whole body of a GET response, resumed as [`Self::body_stream`].
    pub(super) async fn collect_body(
        &self,
        bucket: &str,
        key: &str,
        response: GetObjectOutput,
    ) -> Result<Vec<u8>, StorageError> {
        // The backend's Content-Length sizes the first allocation, capped: a
        // wrong or huge value must not abort the process on a failed
        // reservation. Past the cap the buffer grows as the bytes arrive.
        const PREALLOC_CAP: usize = 64 * 1024 * 1024;
        let capacity = response
            .content_length()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0)
            .min(PREALLOC_CAP);
        let mut out = Vec::with_capacity(capacity);
        let mut stream = self.body_stream(bucket, key, response);
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{body_span, BodySpan};

    fn span(start: u64, end: u64, total: Option<u64>) -> Option<BodySpan> {
        Some(BodySpan { start, end, total })
    }

    #[test]
    fn body_span_truth_table() {
        // A whole-object GET: Content-Length only.
        assert_eq!(body_span(None, Some(10)), span(0, 10, Some(10)));
        assert_eq!(body_span(None, Some(0)), span(0, 0, Some(0)));
        // A partial GET: Content-Range names the span.
        assert_eq!(
            body_span(Some("bytes 5-9/10"), Some(5)),
            span(5, 10, Some(10))
        );
        assert_eq!(body_span(Some("bytes 0-0/*"), Some(1)), span(0, 1, None));
        // A chunked partial body (no Content-Length): Content-Range alone.
        assert_eq!(body_span(Some("bytes 5-9/10"), None), span(5, 10, Some(10)));
        assert_eq!(body_span(Some("bytes 0-99/*"), None), span(0, 100, None));
        // A chunked whole body: no known span, never a length of 0.
        assert_eq!(body_span(None, None), None);
        // Headers that disagree, or are missing or malformed.
        assert_eq!(body_span(Some("bytes 5-9/10"), Some(4)), None);
        assert_eq!(body_span(Some("bytes 5-10/10"), Some(6)), None);
        assert_eq!(body_span(Some("bytes 9-5/10"), Some(5)), None);
        assert_eq!(body_span(Some("items 5-9/10"), Some(5)), None);
        assert_eq!(body_span(Some("bytes 5-9"), Some(5)), None);
        assert_eq!(body_span(Some("bytes 5-9/x"), None), None);
        assert_eq!(body_span(None, Some(-1)), None);
    }

    /// THE rule: a GET response's missing Content-Length is unknown, never
    /// 0 (stores that send chunked bodies leave it out). A length or size
    /// comes from `body_span` (Content-Range first) or from a HEAD.
    #[test]
    fn a_missing_content_length_is_never_read_as_zero() {
        let mut sites = Vec::new();
        for entry in std::fs::read_dir("src/storage/s3").unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == "tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (n, line) in crate::source_scan::prod_lines(&text) {
                let squashed: String = line.split_whitespace().collect();
                // The size of an outgoing request body, for a metric.
                if squashed.contains("request.body().content_length()") {
                    continue;
                }
                if squashed.contains("content_length().unwrap_or(")
                    || squashed.contains("content_length.unwrap_or(")
                    || squashed.contains("content_length().unwrap_or_default(")
                    || squashed.contains("content_length.unwrap_or_default(")
                {
                    sites.push(format!("{}:{n}", path.display()));
                }
            }
        }
        assert!(
            sites.is_empty(),
            "read the length through body_span: {sites:?}"
        );
    }
}
