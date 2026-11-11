// SPDX-License-Identifier: BUSL-1.1

//! AWS chunked transfer-encoding decoder.
//!
//! AWS SDKs upload object bodies using a framed on-the-wire format when
//! they advertise a `STREAMING-*` value in the `x-amz-content-sha256`
//! header. There are several closely-related variants; all of them must
//! be decoded by the proxy before the payload is handed to storage,
//! otherwise the framing bytes end up concatenated into the object and
//! every subsequent GET serves corrupt content.
//!
//! # Variants recognised
//!
//! | `x-amz-content-sha256` value                     | Per-chunk sig | Trailer | Source |
//! |--------------------------------------------------|:-------------:|:-------:|--------|
//! | `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`             | yes           | no      | Legacy SigV4 streaming |
//! | `STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER`     | yes           | yes     | SigV4 streaming + flexible checksum |
//! | `STREAMING-UNSIGNED-PAYLOAD-TRAILER`             | no            | yes     | **Default in modern AWS SDK v3+ with flexible checksums** |
//! | `STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD`       | yes           | no      | SigV4a (regional) streaming |
//! | `STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD-TRAILER` | yes         | yes     | SigV4a streaming + flexible checksum |
//!
//! The detection predicate uses the `STREAMING-` prefix so new variants
//! (future regional-sig schemes, etc.) are recognised without a code
//! change. The decoder itself is variant-agnostic: it parses the chunk
//! size before any optional `;chunk-signature=...` extension, and after
//! the zero-terminator it consumes any trailer lines up to the final
//! empty line.
//!
//! # Wire format (all variants)
//!
//! ```text
//! <hex-size>[;chunk-signature=<sig>]\r\n
//! <chunk-bytes>\r\n
//! ...
//! 0[;chunk-signature=<sig>]\r\n
//! [<trailer-name>:<trailer-value>\r\n]...
//! \r\n
//! ```
//!
//! The decoder rejects any input that doesn't match this grammar; it does
//! not fall back to "pass the body through unchanged" — that is the bug
//! this module exists to prevent.
//!
//! [`ChunkedDecoder`] is the one implementation of the grammar: an
//! incremental state machine that decodes the body as it arrives (any
//! split of the input gives the same result), with limits on header and
//! trailer sizes. [`AwsChunkedStream`] runs it over a byte stream;
//! [`decode_aws_chunked_with_trailers`] runs it over a buffered body.
//!
//! # Integrity (trailer checksums)
//!
//! When a trailer carries `x-amz-checksum-{crc32,crc32c,sha1,sha256,crc64nvme}`,
//! the client expects the server to verify the checksum against the
//! decoded body. [`decode_aws_chunked_with_trailers`] returns the trailers;
//! the caller verifies them (`api::checksum::verify_body_checksum`).

use axum::body::Bytes;
use axum::http::HeaderMap;
use tracing::{debug, warn};

/// Prefix that identifies every AWS streaming-payload content-sha256
/// value. Any header value starting with this string indicates the
/// request body is chunk-framed and must be decoded before storage.
pub const STREAMING_PREFIX: &str = "STREAMING-";

/// Return `true` when the request's `x-amz-content-sha256` marks the
/// body as AWS chunk-framed.
///
/// The predicate uses the `STREAMING-` prefix so we automatically
/// recognise new AWS variants without a code change — the alternative
/// (an allow-list of exact strings) is what silently corrupted
/// production before: uploads with `STREAMING-UNSIGNED-PAYLOAD-TRAILER`
/// slipped past an allow-list that only knew about the legacy
/// `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` value.
pub fn is_aws_chunked(headers: &HeaderMap) -> bool {
    headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with(STREAMING_PREFIX))
        .unwrap_or(false)
}

/// Read the decoded content length hint the SDK provides alongside the
/// streaming body. Used to pre-size the decode buffer and to reject
/// truncated inputs whose decoded length doesn't match what was
/// advertised.
pub fn get_decoded_content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get("x-amz-decoded-content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// Decode an AWS streaming-chunked body into the underlying payload.
///
/// Returns `None` on any structural inconsistency — truncated chunks,
/// missing CRLFs, bad hex sizes, missing terminator, missing final
/// empty line, or a decoded length that disagrees with
/// `expected_length`. **The decoder never falls back to passing the
/// raw body through** — that behaviour is what produced the production
/// corruption this module guards against.
///
/// # Format
///
/// ```text
/// <hex-size>[;chunk-signature=<sig>]\r\n
/// <chunk-bytes>\r\n            (repeat zero or more times)
/// 0[;chunk-signature=<sig>]\r\n
/// [<trailer-name>:<trailer-value>\r\n]...
/// \r\n
/// ```
///
/// The optional `;chunk-signature=...` extension is ignored — we trust
/// SigV4 verification on the headers to decide whether the caller is
/// authorised. Chunk-level signature verification is a separate piece
/// of work and is not implemented here (nor in the pre-fix code).
pub fn decode_aws_chunked(body: &Bytes, expected_length: Option<usize>) -> Option<Bytes> {
    decode_aws_chunked_with_trailers(body, expected_length).map(|(decoded, _)| decoded)
}

/// [`decode_aws_chunked`], plus the trailer lines that parse as headers.
/// One feed of the [`ChunkedDecoder`]: the one implementation of the
/// grammar.
pub fn decode_aws_chunked_with_trailers(
    body: &Bytes,
    expected_length: Option<usize>,
) -> Option<(Bytes, HeaderMap)> {
    let mut decoder = ChunkedDecoder::new(expected_length.map(|n| n as u64));
    let mut pieces = Vec::new();
    if let Err(e) = decoder.feed(body.clone(), &mut pieces) {
        warn!("aws_chunked: {e}");
        return None;
    }
    let trailers = decoder
        .finish()
        .map_err(|e| warn!("aws_chunked: {e}"))
        .ok()?;
    debug!(
        "aws_chunked: decoded {} pieces from {} byte payload",
        pieces.len(),
        body.len()
    );
    Some((concat(pieces), trailers))
}

/// The pieces as one buffer; one piece is returned as is (no copy).
fn concat(mut pieces: Vec<Bytes>) -> Bytes {
    if pieces.len() == 1 {
        return pieces.pop().unwrap_or_default();
    }
    let len = pieces.iter().map(Bytes::len).sum();
    let mut out = bytes::BytesMut::with_capacity(len);
    for p in pieces {
        out.extend_from_slice(&p);
    }
    out.freeze()
}

/// The longest chunk header line (`<hex-size>[;chunk-signature=...]`).
pub const MAX_HEADER_LINE: usize = 4 * 1024;
/// The most bytes of the trailer section (its lines with their CRLFs).
pub const MAX_TRAILER_BYTES: usize = 16 * 1024;
/// The most trailer lines.
pub const MAX_TRAILER_LINES: usize = 64;

/// Why a framed body is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkedError {
    /// The bytes do not follow the grammar.
    Malformed(&'static str),
    /// A chunk header line is longer than [`MAX_HEADER_LINE`].
    HeaderLineTooLong,
    /// The trailers are larger than [`MAX_TRAILER_BYTES`] or
    /// [`MAX_TRAILER_LINES`].
    TrailersTooLarge,
    /// The chunks hold more than the expected decoded length.
    TooLong { expected: u64 },
    /// The body ended with fewer bytes than the expected decoded length.
    TooShort { expected: u64, decoded: u64 },
    /// The body ended before the final empty line.
    Truncated,
}

impl std::fmt::Display for ChunkedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed aws-chunked body: {what}"),
            Self::HeaderLineTooLong => write!(
                f,
                "an aws-chunked chunk header is longer than {MAX_HEADER_LINE} bytes"
            ),
            Self::TrailersTooLarge => write!(
                f,
                "the aws-chunked trailers are larger than {MAX_TRAILER_BYTES} bytes or \
                 {MAX_TRAILER_LINES} lines"
            ),
            Self::TooLong { expected } => write!(
                f,
                "the aws-chunked body holds more than its x-amz-decoded-content-length {expected}"
            ),
            Self::TooShort { expected, decoded } => write!(
                f,
                "the aws-chunked body holds {decoded} bytes, its \
                 x-amz-decoded-content-length is {expected}"
            ),
            Self::Truncated => write!(f, "the aws-chunked body ended before its last chunk"),
        }
    }
}

impl std::error::Error for ChunkedError {}

/// Where the decoder is in the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Reading a chunk header line.
    Header,
    /// Reading `remaining` bytes of chunk data.
    Data { remaining: u64 },
    /// Reading the CRLF after a chunk; `seen` of its two bytes read.
    DataEnd { seen: u8 },
    /// Reading trailer lines, up to the empty line.
    Trailers,
    /// The final empty line was read; later bytes are ignored.
    Done,
    /// An error was returned; the decoder takes no more input.
    Failed,
}

/// An incremental decoder of the aws-chunked grammar (see the module
/// docs). Pure: it holds no I/O. [`Self::feed`] hands out the payload as
/// zero-copy slices of the input; [`Self::finish`] checks the end and
/// returns the trailers. Whatever way the input is split, the result is
/// the same.
///
/// Limits: a chunk header line of at most [`MAX_HEADER_LINE`] bytes, a
/// trailer section of at most [`MAX_TRAILER_BYTES`] / [`MAX_TRAILER_LINES`],
/// and, when the expected decoded length is known, an overrun fails at the
/// chunk header that announces it, before any of its bytes is handed out.
#[derive(Debug)]
pub struct ChunkedDecoder {
    state: State,
    /// The line being read (header or trailer), with its CRLF so far.
    line: Vec<u8>,
    expected: Option<u64>,
    decoded: u64,
    trailers: HeaderMap,
    trailer_bytes: usize,
    trailer_lines: usize,
}

impl ChunkedDecoder {
    /// `expected`: the decoded length the client announced
    /// (`x-amz-decoded-content-length`), if any.
    pub fn new(expected: Option<u64>) -> Self {
        Self {
            state: State::Header,
            line: Vec::new(),
            expected,
            decoded: 0,
            trailers: HeaderMap::new(),
            trailer_bytes: 0,
            trailer_lines: 0,
        }
    }

    /// Decode `input`; the payload pieces are appended to `out`.
    pub fn feed(&mut self, mut input: Bytes, out: &mut Vec<Bytes>) -> Result<(), ChunkedError> {
        let result = self.feed_inner(&mut input, out);
        if result.is_err() {
            self.state = State::Failed;
        }
        result
    }

    fn feed_inner(&mut self, input: &mut Bytes, out: &mut Vec<Bytes>) -> Result<(), ChunkedError> {
        while !input.is_empty() {
            match self.state {
                State::Failed => return Err(ChunkedError::Malformed("input after an error")),
                State::Done => {
                    // As before: bytes after the final empty line are ignored.
                    input.clear();
                }
                State::Header => {
                    let Some(line) = self.take_line(input, MAX_HEADER_LINE)? else {
                        continue;
                    };
                    let size = parse_chunk_size(&line)?;
                    debug!("aws_chunked: chunk of {size} bytes");
                    if size == 0 {
                        self.state = State::Trailers;
                        continue;
                    }
                    let end = self
                        .decoded
                        .checked_add(size)
                        .ok_or(ChunkedError::Malformed("chunk size overflow"))?;
                    if let Some(expected) = self.expected.filter(|&e| end > e) {
                        return Err(ChunkedError::TooLong { expected });
                    }
                    self.state = State::Data { remaining: size };
                }
                State::Data { remaining } => {
                    let n = remaining.min(input.len() as u64);
                    // `n` is at most `input.len()`, a usize.
                    out.push(input.split_to(n as usize));
                    self.decoded += n;
                    self.state = match remaining - n {
                        0 => State::DataEnd { seen: 0 },
                        rest => State::Data { remaining: rest },
                    };
                }
                State::DataEnd { seen } => {
                    // Every data chunk ends with CRLF; a missing one is the
                    // laxity that once stored framing bytes in objects.
                    let want = if seen == 0 { b'\r' } else { b'\n' };
                    if input[0] != want {
                        return Err(ChunkedError::Malformed("no CRLF after a chunk"));
                    }
                    bytes::Buf::advance(input, 1);
                    self.state = if seen == 0 {
                        State::DataEnd { seen: 1 }
                    } else {
                        State::Header
                    };
                }
                State::Trailers => {
                    let room = MAX_TRAILER_BYTES.saturating_sub(self.trailer_bytes);
                    let Some(line) = self.take_line(input, room)? else {
                        continue;
                    };
                    self.trailer_bytes += line.len() + 2;
                    if line.is_empty() {
                        self.state = State::Done;
                        continue;
                    }
                    self.trailer_lines += 1;
                    if self.trailer_lines > MAX_TRAILER_LINES
                        || self.trailer_bytes > MAX_TRAILER_BYTES
                    {
                        return Err(ChunkedError::TrailersTooLarge);
                    }
                    add_trailer(&line, &mut self.trailers);
                }
            }
        }
        Ok(())
    }

    /// Move bytes of `input` into the current line up to its first CRLF.
    /// `Some(line)` without the CRLF once it is complete. A line longer
    /// than `max` (without its CRLF) fails: a header line with
    /// [`ChunkedError::HeaderLineTooLong`], a trailer line with
    /// [`ChunkedError::TrailersTooLarge`].
    fn take_line(
        &mut self,
        input: &mut Bytes,
        max: usize,
    ) -> Result<Option<Vec<u8>>, ChunkedError> {
        let mut consumed = 0;
        let mut complete = false;
        for &b in input.iter() {
            consumed += 1;
            self.line.push(b);
            if self.line.ends_with(b"\r\n") {
                complete = true;
                break;
            }
            // `max` + the CR of a CRLF still to come.
            if self.line.len() > max + 1 {
                return Err(match self.state {
                    State::Trailers => ChunkedError::TrailersTooLarge,
                    _ => ChunkedError::HeaderLineTooLong,
                });
            }
        }
        bytes::Buf::advance(input, consumed);
        if !complete {
            return Ok(None);
        }
        let mut line = std::mem::take(&mut self.line);
        line.truncate(line.len() - 2);
        Ok(Some(line))
    }

    /// The end of the body: the trailers, once the final empty line was
    /// read and the decoded length matches the expected one.
    pub fn finish(self) -> Result<HeaderMap, ChunkedError> {
        match self.state {
            State::Done => {}
            State::Failed => return Err(ChunkedError::Malformed("finish after an error")),
            _ => return Err(ChunkedError::Truncated),
        }
        if let Some(expected) = self.expected.filter(|&e| e != self.decoded) {
            return Err(ChunkedError::TooShort {
                expected,
                decoded: self.decoded,
            });
        }
        Ok(self.trailers)
    }
}

/// Pure: the size of a chunk header line: the hex token before the first
/// `;` (the rest, `;chunk-signature=<sig>` or a future extension, does
/// not frame anything).
fn parse_chunk_size(line: &[u8]) -> Result<u64, ChunkedError> {
    let text =
        std::str::from_utf8(line).map_err(|_| ChunkedError::Malformed("chunk header not UTF-8"))?;
    let token = text.split(';').next().unwrap_or_default().trim();
    u64::from_str_radix(token, 16).map_err(|_| ChunkedError::Malformed("bad chunk size"))
}

/// A trailer line that parses as `name:value` goes into `trailers`;
/// other lines are skipped, as they always were.
fn add_trailer(line: &[u8], trailers: &mut HeaderMap) {
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    debug!("aws_chunked: received trailer line '{text}'");
    let parsed = text.split_once(':').and_then(|(name, value)| {
        Some((
            axum::http::HeaderName::from_bytes(name.trim().as_bytes()).ok()?,
            axum::http::HeaderValue::from_str(value.trim()).ok()?,
        ))
    });
    if let Some((name, value)) = parsed {
        trailers.append(name, value);
    }
}

/// The trailers of an [`AwsChunkedStream`], set once the stream ended
/// well. A handle the caller keeps while the stream is consumed elsewhere.
#[derive(Debug, Clone, Default)]
pub struct TrailerSlot(std::sync::Arc<std::sync::OnceLock<HeaderMap>>);

impl TrailerSlot {
    /// The trailers; `None` before the stream ended, or when it failed.
    pub fn get(&self) -> Option<HeaderMap> {
        self.0.get().cloned()
    }
}

/// Why an [`AwsChunkedStream`] failed.
#[derive(Debug)]
pub enum AwsChunkedStreamError<E> {
    /// The framed source failed.
    Source(E),
    /// The framing is not valid.
    Framing(ChunkedError),
}

impl<E: std::fmt::Display> std::fmt::Display for AwsChunkedStreamError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(e) => e.fmt(f),
            Self::Framing(e) => e.fmt(f),
        }
    }
}

/// The payload of a framed byte stream, decoded as it arrives (see
/// [`ChunkedDecoder`]). A framing error is the stream's last item; the
/// end of the stream is checked ([`ChunkedDecoder::finish`]) before the
/// stream reports its end, and the trailers are then in [`Self::trailers`].
pub struct AwsChunkedStream<S> {
    inner: S,
    decoder: Option<ChunkedDecoder>,
    ready: std::collections::VecDeque<Bytes>,
    trailers: TrailerSlot,
    ended: bool,
}

impl<S> AwsChunkedStream<S> {
    pub fn new(inner: S, expected: Option<u64>) -> Self {
        Self {
            inner,
            decoder: Some(ChunkedDecoder::new(expected)),
            ready: Default::default(),
            trailers: TrailerSlot::default(),
            ended: false,
        }
    }

    /// The slot that holds the trailers after the stream ended well.
    pub fn trailers(&self) -> TrailerSlot {
        self.trailers.clone()
    }
}

impl<S, E> futures::Stream for AwsChunkedStream<S>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, AwsChunkedStreamError<E>>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            if let Some(piece) = this.ready.pop_front() {
                return Poll::Ready(Some(Ok(piece)));
            }
            let Some(decoder) = this.decoder.as_mut().filter(|_| !this.ended) else {
                return Poll::Ready(None);
            };
            match futures::ready!(std::pin::Pin::new(&mut this.inner).poll_next(cx)) {
                Some(Ok(input)) => {
                    let mut out = Vec::new();
                    if let Err(e) = decoder.feed(input, &mut out) {
                        this.ended = true;
                        return Poll::Ready(Some(Err(AwsChunkedStreamError::Framing(e))));
                    }
                    this.ready.extend(out.into_iter().filter(|p| !p.is_empty()));
                }
                Some(Err(e)) => {
                    this.ended = true;
                    return Poll::Ready(Some(Err(AwsChunkedStreamError::Source(e))));
                }
                None => {
                    this.ended = true;
                    let decoder = this.decoder.take().expect("checked above");
                    return Poll::Ready(match decoder.finish() {
                        Ok(trailers) => {
                            let _ = this.trailers.0.set(trailers);
                            None
                        }
                        Err(e) => Some(Err(AwsChunkedStreamError::Framing(e))),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buffered decoder this module had before [`ChunkedDecoder`],
    /// kept ONLY as the oracle of the differential proptest below.
    fn oracle_decode(body: &Bytes, expected_length: Option<usize>) -> Option<(Bytes, HeaderMap)> {
        let mut trailers = HeaderMap::new();
        let mut result = Vec::with_capacity(expected_length.unwrap_or(body.len()).min(body.len()));
        let mut pos = 0;
        loop {
            let header_end = find_crlf(&body[pos..])?;
            let header_line = &body[pos..pos + header_end];
            pos += header_end + 2;
            let header_str = std::str::from_utf8(header_line).ok()?;
            let size_token = header_str.split(';').next()?.trim();
            let chunk_size = usize::from_str_radix(size_token, 16).ok()?;
            if chunk_size == 0 {
                consume_trailers(body, &mut pos, &mut trailers)?;
                break;
            }
            let chunk_end = pos.checked_add(chunk_size).filter(|&e| e <= body.len())?;
            result.extend_from_slice(&body[pos..chunk_end]);
            pos = chunk_end;
            if pos + 2 > body.len() || &body[pos..pos + 2] != b"\r\n" {
                return None;
            }
            pos += 2;
        }
        if expected_length.is_some_and(|e| result.len() != e) {
            return None;
        }
        Some((Bytes::from(result), trailers))
    }

    fn consume_trailers(body: &Bytes, pos: &mut usize, trailers: &mut HeaderMap) -> Option<()> {
        loop {
            let line_end = find_crlf(&body[*pos..])?;
            let line = &body[*pos..*pos + line_end];
            *pos += line_end + 2;
            if line.is_empty() {
                return Some(());
            }
            if let Ok(line_str) = std::str::from_utf8(line) {
                let parsed = line_str.split_once(':').and_then(|(name, value)| {
                    Some((
                        axum::http::HeaderName::from_bytes(name.trim().as_bytes()).ok()?,
                        axum::http::HeaderValue::from_str(value.trim()).ok()?,
                    ))
                });
                if let Some((name, value)) = parsed {
                    trailers.append(name, value);
                }
            }
        }
    }

    fn find_crlf(data: &[u8]) -> Option<usize> {
        (0..data.len().saturating_sub(1)).find(|&i| data[i] == b'\r' && data[i + 1] == b'\n')
    }

    /// A chunk size near `usize::MAX` overflowed `pos + chunk_size`: a panic
    /// with overflow checks, a slice-index panic without them. The body is
    /// client input (the adapter decodes chunked bodies s3s did not).
    /// Found by the `aws_chunked` fuzz target.
    #[test]
    fn huge_chunk_size_is_refused_not_a_panic() {
        for body in [
            &b"ffffffffffffffff\r\nx"[..],
            b"fffffffffffffffe\r\nhello\r\n0\r\n\r\n",
            b"5\r\nhello\r\nffffffffffffffff\r\n",
        ] {
            let body = Bytes::copy_from_slice(body);
            assert_eq!(decode_aws_chunked(&body, None), None);
            assert_eq!(decode_aws_chunked(&body, Some(5)), None);
        }
    }

    /// The advertised decoded length is client text too: it must not size an
    /// allocation beyond the body it describes.
    #[test]
    fn advertised_length_does_not_size_the_buffer() {
        let body = Bytes::from_static(b"5\r\nhello\r\n0\r\n\r\n");
        assert_eq!(decode_aws_chunked(&body, Some(usize::MAX)), None);
    }

    // ── is_aws_chunked ────────────────────────────────────────────────

    #[test]
    fn is_aws_chunked_accepts_all_known_streaming_variants() {
        for value in [
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD",
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER",
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
            "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD",
            "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD-TRAILER",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-amz-content-sha256", value.parse().unwrap());
            assert!(
                is_aws_chunked(&headers),
                "expected {value} to be recognised as STREAMING-*"
            );
        }
    }

    #[test]
    fn is_aws_chunked_rejects_non_streaming_values() {
        for value in [
            "UNSIGNED-PAYLOAD",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-amz-content-sha256", value.parse().unwrap());
            assert!(
                !is_aws_chunked(&headers),
                "expected {value:?} to NOT match STREAMING-*"
            );
        }

        // Missing header altogether → not chunked.
        assert!(!is_aws_chunked(&HeaderMap::new()));
    }

    // ── Legacy signed variant (STREAMING-AWS4-HMAC-SHA256-PAYLOAD) ────

    #[test]
    fn decode_legacy_signed_single_chunk() {
        let body = Bytes::from(
            "2a;chunk-signature=abc123\r\ntest content Wed Dec 17 16:48:05 UTC 2025\n\r\n0;chunk-signature=def456\r\n\r\n"
        );
        let result = decode_aws_chunked(&body, Some(42)).unwrap();
        assert_eq!(result.len(), 42);
        assert!(result.starts_with(b"test content"));
    }

    #[test]
    fn decode_legacy_signed_multi_chunk() {
        let body = Bytes::from(
            "5;chunk-signature=aaa\r\nhello\r\n6;chunk-signature=bbb\r\n world\r\n0;chunk-signature=ccc\r\n\r\n"
        );
        let result = decode_aws_chunked(&body, Some(11)).unwrap();
        assert_eq!(result.as_ref(), b"hello world");
    }

    #[test]
    fn decode_legacy_signed_empty_payload() {
        let body = Bytes::from("0;chunk-signature=abc\r\n\r\n");
        let result = decode_aws_chunked(&body, Some(0)).unwrap();
        assert!(result.is_empty());
    }

    // ── Unsigned trailer variant (STREAMING-UNSIGNED-PAYLOAD-TRAILER) ─
    // This is what the AWS SDK v3 defaults to when flexible checksums
    // are enabled — the variant that triggered the production bug.

    #[test]
    fn decode_unsigned_trailer_single_chunk() {
        let body = Bytes::from("b\r\nhello world\r\n0\r\n\r\n");
        let result = decode_aws_chunked(&body, Some(11)).unwrap();
        assert_eq!(result.as_ref(), b"hello world");
    }

    #[test]
    fn decode_unsigned_trailer_with_checksum() {
        // Two chunks, followed by zero-terminator and a trailer line
        // carrying the CRC64NVME checksum AWS v3 SDKs emit by default.
        let body = Bytes::from(
            "5\r\nhello\r\n6\r\n world\r\n0\r\nx-amz-checksum-crc64nvme:xEkkN635Gbg=\r\n\r\n",
        );
        let result = decode_aws_chunked(&body, Some(11)).unwrap();
        assert_eq!(result.as_ref(), b"hello world");
    }

    #[test]
    fn decode_returns_the_trailers() {
        let body = Bytes::from(
            "5\r\nhello\r\n0\r\nx-amz-checksum-crc32: NhCmhg==\r\nx-amz-trailer-signature:abc\r\nnot a header\r\n\r\n",
        );
        let (decoded, trailers) = decode_aws_chunked_with_trailers(&body, Some(5)).unwrap();
        assert_eq!(decoded.as_ref(), b"hello");
        assert_eq!(trailers["x-amz-checksum-crc32"], "NhCmhg==");
        assert_eq!(trailers["x-amz-trailer-signature"], "abc");
        assert_eq!(trailers.len(), 2);
    }

    #[test]
    fn decode_unsigned_trailer_empty_payload_with_trailer() {
        // Empty upload with only a trailer checksum — pathological but
        // valid per spec.
        let body = Bytes::from("0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n");
        let result = decode_aws_chunked(&body, Some(0)).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn decode_unsigned_trailer_empty_payload_no_trailer() {
        let body = Bytes::from("0\r\n\r\n");
        let result = decode_aws_chunked(&body, Some(0)).unwrap();
        assert!(result.is_empty());
    }

    // ── Signed trailer variant ────────────────────────────────────────

    #[test]
    fn decode_signed_trailer_with_checksum() {
        let body = Bytes::from(
            "5;chunk-signature=aaa\r\nhello\r\n0;chunk-signature=bbb\r\nx-amz-checksum-sha256:deadbeef\r\n\r\n"
        );
        let result = decode_aws_chunked(&body, Some(5)).unwrap();
        assert_eq!(result.as_ref(), b"hello");
    }

    // ── Malformed inputs (must reject with None, never pass through) ──

    #[test]
    fn decode_rejects_garbage_input() {
        let body = Bytes::from("this is not chunked data at all");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_truncated_chunk() {
        // Header says 0x64 = 100 bytes but only 5 follow.
        let body = Bytes::from("64;chunk-signature=abc\r\nhello");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_without_length_accepts_frame_valid_short_body() {
        // A body truncated at a chunk boundary but followed by a valid
        // terminator is frame-valid: with no expected_length the decoder CANNOT
        // tell it's short and returns the partial payload. This is exactly why
        // the s3s call site now REQUIRES x-amz-decoded-content-length for
        // aws-chunked bodies — the length reconciliation is the only truncation
        // guard at a chunk boundary. This test pins that rationale.
        let body = Bytes::from("5\r\nhello\r\n0\r\n\r\n"); // one 5-byte chunk, then terminator
        let decoded = decode_aws_chunked(&body, None).expect("frame-valid → decodes");
        assert_eq!(&decoded[..], b"hello");
        // With the true length it would still pass; with a LARGER expected length
        // (the real object was bigger, body was truncated) it is rejected.
        assert!(decode_aws_chunked(&body, Some(5)).is_some());
        assert!(
            decode_aws_chunked(&body, Some(100)).is_none(),
            "a length mismatch (truncation) must be rejected when the header IS present"
        );
    }

    #[test]
    fn decode_rejects_missing_crlf_after_chunk() {
        // 5 bytes of data but no trailing CRLF before the terminator.
        let body = Bytes::from("5\r\nhello0\r\n\r\n");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_bad_hex() {
        let body = Bytes::from("zz\r\n\r\n");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_missing_trailer_terminator() {
        // Zero-chunk followed by a trailer line but no final empty
        // line. The decoder must wait for the `\r\n` empty line and
        // reject when the body ends first.
        let body = Bytes::from("0\r\nx-amz-checksum-crc32:AAAAAA==\r\n");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_length_mismatch() {
        // Body says 5 bytes but decoded length claims 3 expected.
        let body = Bytes::from("5\r\nhello\r\n0\r\n\r\n");
        assert!(decode_aws_chunked(&body, Some(3)).is_none());
    }

    // ── Production-failure regression test ─────────────────────────────
    //
    // Rebuilds the exact byte pattern of the corrupted object the user
    // pointed at: a 49415-byte payload ("<0xc107> bytes") framed with
    // STREAMING-UNSIGNED-PAYLOAD-TRAILER and a CRC64NVME trailer. The
    // pre-fix decoder didn't recognise the content-sha256 value so the
    // entire framed body, including the `c107\r\n` prefix and
    // `0\r\n<checksum>\r\n\r\n` suffix, was stored verbatim as the
    // object. This test locks the fix: a single-shot decode must
    // produce exactly the 49415-byte raw payload.

    #[test]
    fn decode_matches_production_corruption_pattern() {
        // Payload same size as the corrupted object's real MP4 content.
        let payload: Vec<u8> = (0..0xc107u32).map(|i| (i & 0xff) as u8).collect();

        // Assemble a STREAMING-UNSIGNED-PAYLOAD-TRAILER wire body with
        // the real CRC64NVME-style trailer line.
        let mut wire = Vec::with_capacity(payload.len() + 64);
        wire.extend_from_slice(b"c107\r\n");
        wire.extend_from_slice(&payload);
        wire.extend_from_slice(b"\r\n0\r\nx-amz-checksum-crc64nvme:xEkkN635Gbg=\r\n\r\n");

        // Wire body should be 52 bytes longer than the payload (6 byte
        // header `c107\r\n`, 2 byte `\r\n` after data, 3 byte `0\r\n`,
        // 39 byte trailer `x-amz-checksum-crc64nvme:xEkkN635Gbg=\r\n`,
        // 2 byte terminating `\r\n` = 52).
        assert_eq!(wire.len(), payload.len() + 52);

        let decoded = decode_aws_chunked(&Bytes::from(wire), Some(payload.len())).unwrap();
        assert_eq!(
            decoded.as_ref(),
            payload.as_slice(),
            "decoded body must equal the raw payload byte-for-byte"
        );
    }

    // ── 3-chunk unsigned-trailer, large payload ───────────────────────

    #[test]
    fn decode_three_chunks_unsigned_trailer_large() {
        // Three chunks of 1000 bytes each, then a trailer. Stresses the
        // loop + CRLF handling without being so large the test is slow.
        let chunk_size = 1000usize;
        let total = chunk_size * 3;
        let payload: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();

        let mut wire = Vec::with_capacity(total + 64);
        for i in 0..3 {
            wire.extend_from_slice(format!("{:x}\r\n", chunk_size).as_bytes());
            wire.extend_from_slice(&payload[i * chunk_size..(i + 1) * chunk_size]);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"0\r\nx-amz-checksum-crc32c:AAAAAA==\r\n\r\n");

        let decoded = decode_aws_chunked(&Bytes::from(wire), Some(total)).unwrap();
        assert_eq!(decoded.len(), total);
        assert_eq!(decoded.as_ref(), payload.as_slice());
    }

    // ── Streaming decoder ─────────────────────────────────────────────

    /// Feed `wire` in pieces of the given sizes (cycled); the decoded
    /// payload and the trailers, or the first error.
    fn feed_split(
        wire: &[u8],
        expected: Option<u64>,
        sizes: &[usize],
    ) -> Result<(Vec<u8>, HeaderMap), ChunkedError> {
        let mut decoder = ChunkedDecoder::new(expected);
        let mut out = Vec::new();
        let wire = Bytes::copy_from_slice(wire);
        let mut at = 0;
        for size in sizes.iter().cycle() {
            if at >= wire.len() {
                break;
            }
            let end = (at + size.max(&1)).min(wire.len());
            decoder.feed(wire.slice(at..end), &mut out)?;
            at = end;
        }
        let trailers = decoder.finish()?;
        Ok((out.concat(), trailers))
    }

    /// A framed body: `chunks`, then the trailers.
    fn frame(chunks: &[Vec<u8>], trailers: &[(&str, &str)], signed: bool) -> Vec<u8> {
        let sig = if signed { ";chunk-signature=abc" } else { "" };
        let mut wire = Vec::new();
        for c in chunks.iter().filter(|c| !c.is_empty()) {
            wire.extend_from_slice(format!("{:x}{sig}\r\n", c.len()).as_bytes());
            wire.extend_from_slice(c);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(format!("0{sig}\r\n").as_bytes());
        for (k, v) in trailers {
            wire.extend_from_slice(format!("{k}:{v}\r\n").as_bytes());
        }
        wire.extend_from_slice(b"\r\n");
        wire
    }

    #[test]
    fn any_split_decodes_like_one_feed() {
        let chunks = vec![b"hello".to_vec(), vec![7u8; 300], b" world".to_vec()];
        let wire = frame(&chunks, &[("x-amz-checksum-crc32", "NhCmhg==")], true);
        let whole = feed_split(&wire, Some(311), &[wire.len()]).unwrap();
        assert_eq!(whole.0, chunks.concat());
        assert_eq!(whole.1["x-amz-checksum-crc32"], "NhCmhg==");
        for sizes in [&[1usize][..], &[2], &[3, 1, 7], &[64], &[5, 300]] {
            assert_eq!(
                feed_split(&wire, Some(311), sizes),
                Ok(whole.clone()),
                "{sizes:?}"
            );
        }
    }

    /// The payload pieces are slices of the input, not copies.
    #[test]
    fn pieces_are_zero_copy_slices() {
        let wire = Bytes::from_static(b"5\r\nhello\r\n0\r\n\r\n");
        let mut decoder = ChunkedDecoder::new(Some(5));
        let mut out = Vec::new();
        decoder.feed(wire.clone(), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_ptr(), wire[3..].as_ptr());
    }

    /// An overrun fails at the chunk header that announces it, before any
    /// of its bytes is handed out.
    #[test]
    fn an_overrun_fails_at_once() {
        let mut decoder = ChunkedDecoder::new(Some(4));
        let mut out = Vec::new();
        let err = decoder.feed(Bytes::from_static(b"5\r\nhel"), &mut out);
        assert_eq!(err, Err(ChunkedError::TooLong { expected: 4 }));
        assert!(out.is_empty());
        // Across chunks: 3 + 3 > 5.
        let wire = frame(&[b"abc".to_vec(), b"def".to_vec()], &[], false);
        assert_eq!(
            feed_split(&wire, Some(5), &[1]),
            Err(ChunkedError::TooLong { expected: 5 })
        );
        assert_eq!(
            feed_split(&wire, Some(7), &[4]),
            Err(ChunkedError::TooShort {
                expected: 7,
                decoded: 6
            })
        );
    }

    #[test]
    fn the_limits_hold() {
        let long_header = format!("5;{}\r\nhello\r\n0\r\n\r\n", "x".repeat(MAX_HEADER_LINE));
        assert_eq!(
            feed_split(long_header.as_bytes(), None, &[17]),
            Err(ChunkedError::HeaderLineTooLong)
        );
        // At the limit it is fine.
        let at_limit = format!(
            "5;{}\r\nhello\r\n0\r\n\r\n",
            "x".repeat(MAX_HEADER_LINE - 2)
        );
        assert!(feed_split(at_limit.as_bytes(), Some(5), &[17]).is_ok());

        let many: Vec<(String, String)> = (0..=MAX_TRAILER_LINES)
            .map(|i| (format!("x-amz-t{i}"), "v".into()))
            .collect();
        let many: Vec<(&str, &str)> = many.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let wire = frame(&[b"a".to_vec()], &many, false);
        assert_eq!(
            feed_split(&wire, None, &[9]),
            Err(ChunkedError::TrailersTooLarge)
        );
        assert!(feed_split(
            &frame(&[b"a".to_vec()], &many[..MAX_TRAILER_LINES], false),
            None,
            &[9]
        )
        .is_ok());

        let big = "v".repeat(MAX_TRAILER_BYTES);
        let wire = frame(&[b"a".to_vec()], &[("x-amz-big", &big)], false);
        assert_eq!(
            feed_split(&wire, None, &[999]),
            Err(ChunkedError::TrailersTooLarge)
        );
    }

    #[test]
    fn a_body_cut_short_is_truncated() {
        let wire = frame(
            &[b"hello".to_vec()],
            &[("x-amz-checksum-crc32", "AAAAAA==")],
            false,
        );
        for cut in 0..wire.len() {
            assert!(
                feed_split(&wire[..cut], None, &[3]).is_err(),
                "a body cut at {cut} decoded"
            );
        }
    }

    /// The stream adapter: the payload, then the trailers once it ends;
    /// a framing error is its last item.
    #[tokio::test]
    async fn the_stream_adapter_decodes_and_keeps_the_trailers() {
        use futures::StreamExt;
        let wire = frame(
            &[b"hello".to_vec(), b" world".to_vec()],
            &[("x-amz-checksum-crc32c", "x")],
            true,
        );
        let pieces: Vec<Result<Bytes, std::io::Error>> = wire
            .chunks(4)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let stream = AwsChunkedStream::new(futures::stream::iter(pieces), Some(11));
        let slot = stream.trailers();
        assert!(slot.get().is_none());
        let items: Vec<_> = stream.collect().await;
        let body: Vec<u8> = items
            .into_iter()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(body, b"hello world");
        assert_eq!(slot.get().unwrap()["x-amz-checksum-crc32c"], "x");

        let short = AwsChunkedStream::new(
            futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(
                b"5\r\nhel",
            ))]),
            Some(5),
        );
        let slot = short.trailers();
        let items: Vec<_> = short.collect().await;
        assert!(matches!(
            items.last(),
            Some(Err(AwsChunkedStreamError::Framing(ChunkedError::Truncated)))
        ));
        assert!(slot.get().is_none());
    }

    mod differential {
        use super::*;
        use proptest::prelude::*;

        /// Bodies near the grammar: framed bodies (signed or not, with or
        /// without trailers) that are then cut, mutated or left alone.
        fn wire() -> impl Strategy<Value = (Vec<u8>, Option<usize>)> {
            (
                proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..40), 0..5),
                proptest::collection::vec(("[a-z-]{1,12}", "[ -~]{0,12}"), 0..3),
                any::<bool>(),
                0usize..4,
                any::<usize>(),
                any::<u8>(),
                proptest::option::of(0usize..200),
            )
                .prop_map(|(chunks, trailers, signed, edit, at, byte, expected)| {
                    let t: Vec<(&str, &str)> = trailers
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    let mut w = frame(&chunks, &t, signed);
                    match edit {
                        1 if !w.is_empty() => w.truncate(at % w.len()),
                        2 if !w.is_empty() => {
                            let i = at % w.len();
                            w[i] = byte;
                        }
                        _ => {}
                    }
                    let expected = match edit {
                        // The true length most of the time.
                        0 | 3 => Some(chunks.iter().map(Vec::len).sum()),
                        _ => expected,
                    };
                    (w, expected)
                })
        }

        proptest! {
            /// Any split of the input gives what one feed gives, and one
            /// feed gives what the old buffered decoder gave.
            #[test]
            fn the_decoder_matches_the_old_one_for_any_split(
                (w, expected) in wire(),
                sizes in proptest::collection::vec(1usize..9, 1..6),
            ) {
                let one = feed_split(&w, expected.map(|n| n as u64), &[w.len().max(1)]);
                let split = feed_split(&w, expected.map(|n| n as u64), &sizes);
                prop_assert_eq!(&one, &split);
                let oracle = oracle_decode(&Bytes::copy_from_slice(&w), expected);
                prop_assert_eq!(
                    one.ok(),
                    oracle.map(|(b, t)| (b.to_vec(), t))
                );
                let wrapper = decode_aws_chunked_with_trailers(&Bytes::copy_from_slice(&w), expected);
                prop_assert_eq!(
                    wrapper.map(|(b, t)| (b.to_vec(), t)),
                    feed_split(&w, expected.map(|n| n as u64), &[w.len().max(1)]).ok()
                );
            }

            /// Arbitrary bytes: never a panic, and split invariance holds.
            #[test]
            fn arbitrary_bytes_never_panic(
                w in proptest::collection::vec(any::<u8>(), 0..200),
                sizes in proptest::collection::vec(1usize..9, 1..6),
            ) {
                let one = feed_split(&w, None, &[w.len().max(1)]);
                prop_assert_eq!(one, feed_split(&w, None, &sizes));
            }
        }
    }
}
