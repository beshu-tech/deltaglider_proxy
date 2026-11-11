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
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

/// Bytes read from a file per frame.
const READ_BUFFER: usize = 256 * 1024;

/// The local files of one PUT body, in order, with the size of each as it
/// was when the PUT started.
#[derive(Clone)]
pub(super) struct FileBody {
    files: Arc<[(PathBuf, u64)]>,
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
            files.push((path.to_path_buf(), size));
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
            for (path, _) in files.iter() {
                crate::deltaglider::spool::for_each_chunk_blocking(path, |c| h.update(c)).ok()?;
            }
            Some(hex::encode(h.finalize()))
        })
        .await
        .ok()
        .flatten()
    }
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
    files: Arc<[(PathBuf, u64)]>,
    next: usize,
    state: State,
    /// Bytes of the whole body still to come.
    left: u64,
}

impl ChainedFiles {
    fn new(files: Arc<[(PathBuf, u64)]>, len: u64) -> Self {
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
                    let Some((path, size)) = this.files.get(this.next).cloned() else {
                        return Poll::Ready(None);
                    };
                    this.next += 1;
                    this.state = State::Opening(Box::pin(async move {
                        let file = tokio::fs::File::open(&path).await?;
                        let now = file.metadata().await?.len();
                        if now != size {
                            return Err(file_changed(&path, size, now));
                        }
                        Ok(file)
                    }));
                }
                State::Opening(opening) => {
                    let file = ready!(opening.as_mut().poll(cx))?;
                    let size = this.files[this.next - 1].1;
                    this.state = State::Reading {
                        stream: ReaderStream::with_capacity(file.take(size), READ_BUFFER),
                        left: size,
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
                            let (path, size) = &this.files[this.next - 1];
                            let e = file_changed(path, *size, size - *left);
                            return Poll::Ready(Some(Err(e.into())));
                        }
                        None => this.state = State::Between,
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0 && matches!(self.state, State::Between) && self.next >= self.files.len()
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

    #[tokio::test]
    async fn no_parts_is_an_empty_body() {
        let body = FileBody::open(&[]).await.unwrap();
        let sdk = body.byte_stream().into_inner();
        assert_eq!(sdk.content_length(), Some(0));
        assert!(collect(sdk).await.unwrap().is_empty());
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
    }
}
