// SPDX-License-Identifier: BUSL-1.1

//! PUT bodies streamed from local files: one file, or the ordered part
//! files of a multipart upload sent as ONE object.
//!
//! The body has an exact length (the sum of the file sizes read when the
//! PUT starts), so the SDK signs and sends it like any fixed-length body,
//! and it is retryable: every attempt (an SDK retry or the application's
//! retry loop in `objects.rs`) reads the files again from their start. No
//! part is joined into another file and no byte is held in memory beyond
//! one read buffer.

use crate::storage::StorageError;
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_types::body::SdkBody;
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// Bytes read from a file per frame.
const READ_BUFFER: usize = 256 * 1024;

/// The part of one file that a body sends.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    path: PathBuf,
    /// The size of the whole file when the PUT started: a file that has
    /// another size when an attempt opens it fails that attempt.
    size: u64,
    /// Bytes of the file before the first byte that is sent.
    skip: u64,
    /// Bytes of the file that are sent, from `skip` on.
    take: u64,
}

/// The local files of one PUT body, in order, with the size of each as it
/// was when the PUT started. A body can also be a byte range of the
/// concatenation of the files ([`Self::slice`]): one part of a multipart
/// upload.
#[derive(Clone)]
pub(super) struct FileBody {
    files: Arc<[Segment]>,
    len: u64,
}

impl FileBody {
    /// Read the size of each file. The body sends exactly these sizes: a
    /// file that is shorter or longer when an attempt reads it fails that
    /// attempt instead of sending other bytes.
    pub(super) async fn open(paths: &[&Path]) -> Result<Self, StorageError> {
        let mut files = Vec::with_capacity(paths.len());
        let mut len = 0u64;
        for path in paths {
            let size = tokio::fs::metadata(path).await?.len();
            len += size;
            files.push(Segment {
                path: path.to_path_buf(),
                size,
                skip: 0,
                take: size,
            });
        }
        Ok(Self {
            files: files.into(),
            len,
        })
    }

    /// The number of bytes the body sends.
    pub(super) fn len(&self) -> u64 {
        self.len
    }

    /// The `len` bytes of this body that start at `offset`: the files (or
    /// file ranges) that hold them, nothing copied. A range that reaches
    /// past the end is cut at the end.
    pub(super) fn slice(&self, offset: u64, len: u64) -> Self {
        let end = offset.saturating_add(len).min(self.len);
        let mut files = Vec::new();
        let mut at = 0u64;
        for seg in self.files.iter() {
            let (from, to) = (at, at + seg.take);
            at = to;
            let (lo, hi) = (from.max(offset), to.min(end));
            if lo < hi {
                files.push(Segment {
                    path: seg.path.clone(),
                    size: seg.size,
                    skip: seg.skip + (lo - from),
                    take: hi - lo,
                });
            }
        }
        let len = end.saturating_sub(offset);
        Self {
            files: files.into(),
            len,
        }
    }

    /// A retryable request body of exactly [`Self::len`] bytes: each
    /// attempt reads the files again from their start.
    pub(super) fn byte_stream(&self) -> ByteStream {
        let files = self.files.clone();
        let len = self.len;
        ByteStream::new(SdkBody::retryable(move || {
            SdkBody::from_body_1_x(ChainedFiles::new(files.clone(), len))
        }))
    }

    /// The MD5 of the body, hex (the ETag of a single PUT of it); `None`
    /// when a file cannot be read.
    pub(super) async fn md5_hex(&self) -> Option<String> {
        let files = self.files.clone();
        tokio::task::spawn_blocking(move || {
            use md5::Digest;
            let mut h = md5::Md5::new();
            for seg in files.iter() {
                if seg.skip == 0 && seg.take == seg.size {
                    crate::deltaglider::spool::for_each_chunk_blocking(&seg.path, |c| h.update(c))
                        .ok()?;
                } else {
                    md5_range(&seg.path, seg.skip, seg.take, &mut h)?;
                }
            }
            Some(hex::encode(h.finalize()))
        })
        .await
        .ok()
        .flatten()
    }
}

/// Feed `take` bytes of a file, from `skip`, to an MD5; `None` when the
/// file is shorter or cannot be read.
fn md5_range(path: &Path, skip: u64, take: u64, h: &mut md5::Md5) -> Option<()> {
    use md5::Digest;
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(skip)).ok()?;
    let mut left = take;
    let mut buf = vec![0u8; READ_BUFFER];
    while left > 0 {
        let want = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        let n = file.read(&mut buf[..want]).ok()?;
        if n == 0 {
            return None;
        }
        h.update(&buf[..n]);
        left -= n as u64;
    }
    Some(())
}

type OpenFile = Pin<Box<dyn Future<Output = io::Result<tokio::fs::File>> + Send + Sync>>;

/// Where a [`ChainedFiles`] read is.
enum State {
    /// Between two files: the next one is `ChainedFiles::next`.
    Between,
    Opening(OpenFile),
    /// Reading a file; `left`: its bytes still to come.
    Reading {
        stream: ReaderStream<tokio::io::Take<tokio::fs::File>>,
        left: u64,
    },
}

/// One attempt's read of a [`FileBody`]: the files one after the other.
struct ChainedFiles {
    files: Arc<[Segment]>,
    next: usize,
    state: State,
    /// Bytes of the whole body still to come.
    left: u64,
}

impl ChainedFiles {
    fn new(files: Arc<[Segment]>, len: u64) -> Self {
        Self {
            files,
            next: 0,
            state: State::Between,
            left: len,
        }
    }
}

/// The error of a body file whose size is not the one the PUT started
/// with.
fn file_changed(path: &Path, recorded: u64, now: u64) -> io::Error {
    io::Error::other(format!(
        "{} changed while it was uploaded ({recorded} bytes when the upload started, {now} now)",
        path.display()
    ))
}

impl Body for ChainedFiles {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                State::Between => {
                    let Some(seg) = this.files.get(this.next).cloned() else {
                        return Poll::Ready(None);
                    };
                    this.next += 1;
                    this.state = State::Opening(Box::pin(async move {
                        let mut file = tokio::fs::File::open(&seg.path).await?;
                        let now = file.metadata().await?.len();
                        if now != seg.size {
                            return Err(file_changed(&seg.path, seg.size, now));
                        }
                        if seg.skip > 0 {
                            file.seek(io::SeekFrom::Start(seg.skip)).await?;
                        }
                        Ok(file)
                    }));
                }
                State::Opening(opening) => {
                    let file = ready!(opening.as_mut().poll(cx))?;
                    let take = this.files[this.next - 1].take;
                    this.state = State::Reading {
                        stream: ReaderStream::with_capacity(file.take(take), READ_BUFFER),
                        left: take,
                    };
                }
                State::Reading { stream, left } => {
                    match ready!(futures::Stream::poll_next(Pin::new(stream), cx)) {
                        Some(Ok(bytes)) => {
                            *left -= bytes.len() as u64;
                            this.left -= bytes.len() as u64;
                            return Poll::Ready(Some(Ok(Frame::data(bytes))));
                        }
                        Some(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                        None if *left > 0 => {
                            let seg = &this.files[this.next - 1];
                            let e = file_changed(&seg.path, seg.size, seg.skip + seg.take - *left);
                            return Poll::Ready(Some(Err(e.into())));
                        }
                        None => this.state = State::Between,
                    }
                }
            }
        }
    }

    /// True as soon as the last byte is out, in any state: hyper stops
    /// polling after the `Content-Length`, and the SDK's throughput check
    /// takes the upload as complete only through this method.
    fn is_end_stream(&self) -> bool {
        self.left == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_parts(dir: &Path, parts: &[&[u8]]) -> Vec<PathBuf> {
        parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let path = dir.join(format!("part-{i}"));
                std::fs::write(&path, p).unwrap();
                path
            })
            .collect()
    }

    async fn body_of(paths: &[PathBuf]) -> FileBody {
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        FileBody::open(&refs).await.unwrap()
    }

    async fn collect(body: SdkBody) -> Result<Vec<u8>, String> {
        let collected = ByteStream::new(body).collect().await;
        // `{:#}`-style chain: the cause names the changed file.
        collected
            .map(|b| b.into_bytes().to_vec())
            .map_err(|e| format!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))
    }

    /// The parts go out as one body of their exact total length, streamed
    /// (not one in-memory buffer), in order.
    #[tokio::test]
    async fn parts_are_one_streamed_body_of_their_exact_length() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![7u8; 3 * READ_BUFFER + 11];
        let paths = write_parts(dir.path(), &[b"alpha-", b"", &big, b"-omega"]);
        let body = body_of(&paths).await;
        assert_eq!(body.len(), 6 + big.len() as u64 + 6);
        let sdk = body.byte_stream().into_inner();
        assert!(sdk.is_streaming(), "the body is read from the files");
        assert!(sdk.bytes().is_none(), "no in-memory copy of the body");
        assert_eq!(sdk.content_length(), Some(body.len()));
        let mut want = b"alpha-".to_vec();
        want.extend_from_slice(&big);
        want.extend_from_slice(b"-omega");
        assert_eq!(collect(sdk).await.unwrap(), want);
        let md5 = hex::encode(<md5::Md5 as md5::Digest>::digest(&want));
        assert_eq!(body.md5_hex().await.as_deref(), Some(md5.as_str()));
    }

    /// A retry reads the files again from their start: an attempt that
    /// broke off after some frames leaves the next one whole.
    #[tokio::test]
    async fn every_attempt_reads_the_files_from_their_start() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![3u8; 2 * READ_BUFFER];
        let paths = write_parts(dir.path(), &[&big, b"tail"]);
        let body = body_of(&paths).await;
        let sdk = body.byte_stream().into_inner();
        let mut first = ByteStream::new(sdk.try_clone().expect("the body is retryable"));
        let frame = first.next().await.unwrap().unwrap();
        assert!(!frame.is_empty());
        drop(first);
        let again = sdk.try_clone().expect("the body is retryable");
        assert_eq!(again.content_length(), Some(body.len()));
        assert_eq!(collect(again).await.unwrap().len() as u64, body.len());
    }

    /// A part that is shorter or longer than when the PUT started fails the
    /// attempt: the body never sends other bytes than it announced.
    #[tokio::test]
    async fn a_part_that_changed_size_fails_the_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_parts(dir.path(), &[b"first", b"second"]);
        let body = body_of(&paths).await;

        std::fs::write(&paths[1], b"sec").unwrap();
        let e = collect(body.byte_stream().into_inner()).await.unwrap_err();
        assert!(e.contains("changed while it was uploaded"), "{e}");

        std::fs::write(&paths[1], b"second, longer").unwrap();
        let e = collect(body.byte_stream().into_inner()).await.unwrap_err();
        assert!(e.contains("changed while it was uploaded"), "{e}");

        std::fs::remove_file(&paths[0]).unwrap();
        assert!(collect(body.byte_stream().into_inner()).await.is_err());
        assert_eq!(body.md5_hex().await, None, "a missing part has no MD5");
    }

    /// The body says that it is at its end as soon as its last byte is out,
    /// in every state, also before the poll that returns `None` and with
    /// empty files after the last data. The SDK marks an upload as complete
    /// only through `is_end_stream` (aws-smithy-runtime 1.16.2,
    /// `minimum_throughput/http_body_1_x.rs`: hyper stops polling a body
    /// after its `Content-Length`, so `None` may never come). A body that
    /// stays silent about it reads as stalled while the proxy waits for the
    /// backend's answer.
    #[tokio::test]
    async fn the_body_is_at_its_end_when_its_last_byte_is_out() {
        use http_body::Body as _;
        let dir = tempfile::tempdir().unwrap();
        for parts in [
            vec![b"abc".to_vec()],
            vec![b"abc".to_vec(), vec![9u8; 2 * READ_BUFFER + 5]],
            vec![b"abc".to_vec(), Vec::new(), Vec::new()],
            vec![Vec::new()],
        ] {
            let slices: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
            let paths = write_parts(dir.path(), &slices);
            let body = body_of(&paths).await;
            let total = body.len();
            for (offset, len) in [(0, total), (1, total.saturating_sub(1))] {
                let slice = body.slice(offset, len);
                let mut chained = ChainedFiles::new(slice.files.clone(), slice.len());
                let mut seen = 0u64;
                while seen < slice.len() {
                    assert!(!chained.is_end_stream(), "bytes are left ({seen}/{total})");
                    let frame = std::future::poll_fn(|cx| Pin::new(&mut chained).poll_frame(cx))
                        .await
                        .expect("a frame")
                        .unwrap();
                    seen += frame.into_data().map_or(0, |d| d.len() as u64);
                }
                assert!(
                    chained.is_end_stream(),
                    "the last byte is out: {parts:?} slice {offset}+{len}",
                    parts = parts.iter().map(Vec::len).collect::<Vec<_>>()
                );
            }
        }
    }

    #[tokio::test]
    async fn no_parts_is_an_empty_body() {
        let body = FileBody::open(&[]).await.unwrap();
        let sdk = body.byte_stream().into_inner();
        assert_eq!(sdk.content_length(), Some(0));
        assert!(collect(sdk).await.unwrap().is_empty());
    }

    /// A slice is the bytes `offset..offset + len` of the concatenation:
    /// across a file boundary, inside one file, and cut at the end. It is
    /// streamed from the files (no copy), retryable, and has the MD5 of
    /// those bytes.
    #[tokio::test]
    async fn a_slice_is_a_byte_range_of_the_concatenation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_parts(dir.path(), &[b"0123456789", b"", b"abcdefghij", b"KLMNOP"]);
        let body = body_of(&paths).await;
        let all = b"0123456789abcdefghijKLMNOP";
        for (offset, len, want) in [
            (0u64, 4u64, &all[0..4]),
            (8, 6, &all[8..14]),
            (10, 10, &all[10..20]),
            (19, 7, &all[19..26]),
            (20, 100, &all[20..26]),
            (26, 5, &all[26..26]),
            (3, 0, &all[3..3]),
        ] {
            let slice = body.slice(offset, len);
            assert_eq!(slice.len(), want.len() as u64, "{offset}+{len}");
            let sdk = slice.byte_stream().into_inner();
            assert!(sdk.is_streaming(), "{offset}+{len}: read from the files");
            assert_eq!(sdk.content_length(), Some(slice.len()), "{offset}+{len}");
            let retry = sdk.try_clone().expect("a slice is retryable");
            assert_eq!(collect(sdk).await.unwrap(), want, "{offset}+{len}");
            assert_eq!(collect(retry).await.unwrap(), want, "{offset}+{len} again");
            let md5 = hex::encode(<md5::Md5 as md5::Digest>::digest(want));
            assert_eq!(slice.md5_hex().await.as_deref(), Some(md5.as_str()));
        }
        // A slice of a slice.
        let inner = body.slice(5, 15).slice(5, 6);
        assert_eq!(
            collect(inner.byte_stream().into_inner()).await.unwrap(),
            &all[10..16]
        );
    }

    /// A file that changed size fails the slices that read it, as it fails
    /// the whole body.
    #[tokio::test]
    async fn a_slice_of_a_changed_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_parts(dir.path(), &[b"first", b"second"]);
        let body = body_of(&paths).await;
        std::fs::write(&paths[1], b"seconds, longer").unwrap();
        let e = collect(body.slice(5, 6).byte_stream().into_inner())
            .await
            .unwrap_err();
        assert!(e.contains("changed while it was uploaded"), "{e}");
    }

    fn passthrough_meta(len: usize) -> crate::types::FileMetadata {
        crate::types::FileMetadata::new_passthrough(
            "o".into(),
            "0".repeat(64),
            "0".repeat(32),
            len as u64,
            None,
        )
    }

    fn puts(fake: &crate::storage::FakeS3, path: &str) -> usize {
        let want = format!("PUT {path}");
        fake.requests()
            .iter()
            .filter(|r| r.split('?').next() == Some(want.as_str()))
            .count()
    }

    /// Relayed parts on S3 are ONE PUT streamed from the part files: the
    /// stored object is their concatenation, the backend received exactly
    /// its bytes, and no spool is reserved for a joined copy.
    #[tokio::test]
    async fn s3_stores_parts_as_one_put_from_the_files() {
        use crate::storage::{PutBody, StorageBackend, StoredObject, StoredWrite};
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let s3 = super::super::test_support::for_test_endpoint(&endpoint);
        let dir = tempfile::tempdir().unwrap();
        let sd =
            crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 8 << 20).unwrap();
        let big = vec![5u8; 2 * READ_BUFFER + 1];
        let paths = write_parts(dir.path(), &[b"head-", &big, b"-tail"]);
        let total = 10 + big.len();
        assert_eq!(s3.file_put_spool_bytes("b", total as u64, true).await, 0);
        fake.clear();
        let body = PutBody::Parts {
            paths: &paths,
            spool: crate::deltaglider::spool::SpoolBudget::new(&sd, None, None),
        };
        let target = StoredWrite::Passthrough { name: "o" };
        s3.put_object("b", "p", target, body, &passthrough_meta(total))
            .await
            .unwrap();
        assert_eq!(puts(&fake, "/b/p/o"), 1, "{:?}", fake.requests());
        assert_eq!(fake.bytes().1, total as u64, "exactly the body was sent");
        let back = s3
            .get_object("b", "p", StoredObject::Passthrough("o"))
            .await
            .unwrap();
        assert_eq!(back, [&b"head-"[..], &big, b"-tail"].concat());
        assert_eq!(sd.free_mib(), 8, "no spool for a joined copy");
    }

    /// The application retry of a PUT (the Hetzner 400) sends the part
    /// files again from their start: the stored object is whole.
    #[tokio::test]
    async fn s3_retries_a_parts_put_from_the_start_of_the_files() {
        use crate::storage::{PutBody, StorageBackend, StoredObject, StoredWrite};
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let s3 = super::super::test_support::for_test_endpoint(&endpoint);
        let dir = tempfile::tempdir().unwrap();
        let sd =
            crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 8 << 20).unwrap();
        let paths = write_parts(dir.path(), &[b"one,", b"two,", b"three"]);
        fake.fail("PUT", "b/p/o", 400, "BadRequest", 1);
        let body = PutBody::Parts {
            paths: &paths,
            spool: crate::deltaglider::spool::SpoolBudget::new(&sd, None, None),
        };
        let target = StoredWrite::Passthrough { name: "o" };
        s3.put_object("b", "p", target, body, &passthrough_meta(13))
            .await
            .expect("the retry succeeds");
        assert_eq!(puts(&fake, "/b/p/o"), 2);
        let back = s3
            .get_object("b", "p", StoredObject::Passthrough("o"))
            .await
            .unwrap();
        assert_eq!(back, b"one,two,three");
    }

    /// A delta from a file is one PUT streamed from it, and a conditional
    /// rewrite of a delta or the reference from a file is fenced on the
    /// version: a stale version writes nothing.
    #[tokio::test]
    async fn s3_writes_deltas_from_files_and_fences_file_rewrites() {
        use crate::storage::{
            PutBody, RewriteTarget, StorageBackend, StoredObject, StoredWrite, WriteIf,
        };
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let s3 = super::super::test_support::for_test_endpoint(&endpoint);
        let dir = tempfile::tempdir().unwrap();
        let sd =
            crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 8 << 20).unwrap();
        let spool = crate::deltaglider::spool::SpoolBudget::new(&sd, None, None);
        let proof = crate::deltaglider::RefWriteProof::for_tests();
        let src = dir.path().join("delta");
        std::fs::write(&src, b"first delta").unwrap();
        let meta = passthrough_meta(11);
        let target = StoredWrite::Delta {
            name: "app.zip",
            proof,
        };
        fake.clear();
        s3.put_object("b", "p", target, PutBody::File { path: &src, spool }, &meta)
            .await
            .unwrap();
        assert_eq!(fake.bytes().1, 11);
        let delta = StoredObject::Delta("app.zip");
        assert_eq!(
            s3.get_object("b", "p", delta).await.unwrap(),
            b"first delta"
        );
        let version = s3
            .head_object("b", "p", delta)
            .await
            .unwrap()
            .backend_version
            .unwrap();

        std::fs::write(&src, b"second delta").unwrap();
        let target = RewriteTarget::Delta {
            name: "app.zip",
            proof,
        };
        let body = || PutBody::File { path: &src, spool };
        let w = s3.put_if_unchanged("b", "p", target, body(), &meta, "\"stale\"");
        assert_eq!(w.await.unwrap(), WriteIf::Changed);
        assert_eq!(
            s3.get_object("b", "p", delta).await.unwrap(),
            b"first delta"
        );
        let w = s3.put_if_unchanged("b", "p", target, body(), &meta, &version);
        assert_eq!(w.await.unwrap(), WriteIf::Written);
        assert_eq!(
            s3.get_object("b", "p", delta).await.unwrap(),
            b"second delta"
        );

        let target = RewriteTarget::Reference { proof };
        let w = s3.put_if_unchanged("b", "p", target, body(), &meta, "\"stale\"");
        assert_eq!(
            w.await.unwrap(),
            WriteIf::Changed,
            "no reference to rewrite"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
        /// Any split of a body into parts sends the body.
        #[test]
        fn any_split_sends_the_concatenation(
            parts in proptest::collection::vec(
                proptest::collection::vec(proptest::num::u8::ANY, 0..4096),
                0..5,
            )
        ) {
            let dir = tempfile::tempdir().unwrap();
            let slices: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
            let paths = write_parts(dir.path(), &slices);
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let got = rt.block_on(async {
                let body = body_of(&paths).await;
                collect(body.byte_stream().into_inner()).await.unwrap()
            });
            proptest::prop_assert_eq!(got, parts.concat());
        }

        /// Any split into files and any range: the slice is that range of
        /// the concatenation.
        #[test]
        fn any_slice_is_the_range_of_the_concatenation(
            parts in proptest::collection::vec(
                proptest::collection::vec(proptest::num::u8::ANY, 0..2048),
                0..5,
            ),
            offset in 0u64..9000,
            len in 0u64..9000,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let slices: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
            let paths = write_parts(dir.path(), &slices);
            let all = parts.concat();
            let from = (offset as usize).min(all.len());
            let to = (offset.saturating_add(len) as usize).min(all.len());
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let got = rt.block_on(async {
                let slice = body_of(&paths).await.slice(offset, len);
                assert_eq!(slice.len(), (to - from) as u64);
                collect(slice.byte_stream().into_inner()).await.unwrap()
            });
            proptest::prop_assert_eq!(got, all[from..to].to_vec());
        }
    }
}
