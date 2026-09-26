// SPDX-License-Identifier: BUSL-1.1

//! Streaming ZIP writer for the bulk download (`GET /_/api/admin/objects/zip`).
//!
//! Entries are STORED (no compression): the objects are mostly
//! already-compressed artifacts, deflate buys little and costs CPU, and a
//! STORED archive has a size that is the sum of its inputs. Each entry has a
//! data descriptor (flag bit 3), so its CRC-32 is computed while the bytes
//! pass through and nothing is buffered. ZIP64 records are written only
//! where a value does not fit the 32-bit (or 16-bit) field.
//!
//! [`ZipWriter`] is sans-I/O: it returns the header bytes to send and counts
//! the data bytes. [`stream_archive`] drives it over entry bodies and sends
//! the archive through a bounded channel. A body error ends the stream with
//! an error BEFORE the central directory: the client sees a failed download,
//! never an archive that looks complete.
//!
//! No maintained crate fit: `zip` 2 writes through a synchronous `Write`
//! and wants ZIP64 chosen per entry up front anyway; `async_zip` would be a
//! new dependency for about 200 lines of format code.

use bytes::{BufMut, Bytes, BytesMut};
use chrono::{DateTime, Datelike, Timelike, Utc};
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use std::future::Future;
use std::io;

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_DESCRIPTOR: u32 = 0x0807_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_ZIP64_EOCD: u32 = 0x0606_4b50;
const SIG_ZIP64_LOCATOR: u32 = 0x0706_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const FLAG_DESCRIPTOR: u16 = 1 << 3;
const FLAG_UTF8: u16 = 1 << 11;
const VERSION_DEFAULT: u16 = 20;
const VERSION_ZIP64: u16 = 45;
/// "Made by" host 3 = Unix, so the external attributes carry a file mode.
const MADE_BY_UNIX: u16 = 3 << 8;
const EXTERNAL_ATTR_FILE_0644: u32 = 0o100_644 << 16;
const ZIP64_EXTRA_ID: u16 = 0x0001;
const U16_SENTINEL: u16 = 0xFFFF;
const U32_SENTINEL: u32 = 0xFFFF_FFFF;

/// A value at or above the limit goes into a ZIP64 record. The format's
/// limits are [`Limits::ZIP`]; tests lower them to reach the ZIP64 paths
/// with small archives.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub u32_max: u64,
    pub u16_max: u64,
}

impl Limits {
    pub const ZIP: Limits = Limits {
        u32_max: U32_SENTINEL as u64,
        u16_max: U16_SENTINEL as u64,
    };
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ZipError {
    #[error("ZIP entry name {0:?} is longer than 65535 bytes")]
    NameTooLong(String),
    #[error("{name}: read {actual} bytes, but the object has {expected}")]
    SizeMismatch {
        name: String,
        expected: u64,
        actual: u64,
    },
}

struct OpenEntry {
    name: String,
    expected: u64,
    zip64: bool,
    header_offset: u64,
    dos: (u16, u16),
    crc: crc32fast::Hasher,
    size: u64,
}

struct CentralEntry {
    name: String,
    crc: u32,
    size: u64,
    zip64: bool,
    header_offset: u64,
    dos: (u16, u16),
}

/// Sans-I/O ZIP writer. Call `start_entry`, `write_data` for every chunk
/// (and send the chunk), `finish_entry`, and at the end `finish`.
pub struct ZipWriter {
    limits: Limits,
    offset: u64,
    central: Vec<CentralEntry>,
    open: Option<OpenEntry>,
}

/// MS-DOS (time, date) in UTC; the format cannot hold a date before 1980.
fn dos_datetime(t: DateTime<Utc>) -> (u16, u16) {
    if t.year() < 1980 {
        return (0, (1 << 5) | 1);
    }
    let year = (t.year() - 1980).min(127) as u16;
    let time = ((t.hour() as u16) << 11) | ((t.minute() as u16) << 5) | (t.second() as u16 / 2);
    let date = (year << 9) | ((t.month() as u16) << 5) | t.day() as u16;
    (time, date)
}

fn fits(v: u64, limit: u64) -> bool {
    v < limit
}

impl ZipWriter {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            offset: 0,
            central: Vec::new(),
            open: None,
        }
    }

    /// Bytes written so far.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The local file header of a new entry. `expected_size` decides ZIP64
    /// before any data is known, and `finish_entry` holds the entry to it.
    pub fn start_entry(
        &mut self,
        name: &str,
        expected_size: u64,
        modified: DateTime<Utc>,
    ) -> Result<Bytes, ZipError> {
        assert!(self.open.is_none(), "start_entry with an entry still open");
        if name.len() > U16_SENTINEL as usize {
            return Err(ZipError::NameTooLong(name.to_string()));
        }
        let zip64 = !fits(expected_size, self.limits.u32_max);
        let dos = dos_datetime(modified);
        let mut b = BytesMut::with_capacity(30 + name.len() + 20);
        b.put_u32_le(SIG_LOCAL);
        b.put_u16_le(if zip64 {
            VERSION_ZIP64
        } else {
            VERSION_DEFAULT
        });
        b.put_u16_le(FLAG_DESCRIPTOR | FLAG_UTF8);
        b.put_u16_le(0); // STORED
        b.put_u16_le(dos.0);
        b.put_u16_le(dos.1);
        // CRC and sizes follow in the data descriptor (flag bit 3).
        b.put_u32_le(0);
        let size32 = if zip64 { U32_SENTINEL } else { 0 };
        b.put_u32_le(size32);
        b.put_u32_le(size32);
        b.put_u16_le(name.len() as u16);
        b.put_u16_le(if zip64 { 20 } else { 0 });
        b.put_slice(name.as_bytes());
        if zip64 {
            b.put_u16_le(ZIP64_EXTRA_ID);
            b.put_u16_le(16);
            b.put_u64_le(0);
            b.put_u64_le(0);
        }
        self.open = Some(OpenEntry {
            name: name.to_string(),
            expected: expected_size,
            zip64,
            header_offset: self.offset,
            dos,
            crc: crc32fast::Hasher::new(),
            size: 0,
        });
        self.offset += b.len() as u64;
        Ok(b.freeze())
    }

    /// Count one chunk of the open entry's data. The caller sends the chunk
    /// itself, unchanged (STORED).
    pub fn write_data(&mut self, chunk: &[u8]) {
        let e = self.open.as_mut().expect("write_data with no open entry");
        e.crc.update(chunk);
        e.size += chunk.len() as u64;
        self.offset += chunk.len() as u64;
    }

    /// The data descriptor of the open entry. Fails when the entry did not
    /// have the size its metadata announced: a short read must never become
    /// a well-formed archive.
    pub fn finish_entry(&mut self) -> Result<Bytes, ZipError> {
        let e = self.open.take().expect("finish_entry with no open entry");
        if e.size != e.expected {
            return Err(ZipError::SizeMismatch {
                name: e.name,
                expected: e.expected,
                actual: e.size,
            });
        }
        let crc = e.crc.finalize();
        let mut b = BytesMut::with_capacity(24);
        b.put_u32_le(SIG_DESCRIPTOR);
        b.put_u32_le(crc);
        if e.zip64 {
            b.put_u64_le(e.size);
            b.put_u64_le(e.size);
        } else {
            b.put_u32_le(e.size as u32);
            b.put_u32_le(e.size as u32);
        }
        self.offset += b.len() as u64;
        self.central.push(CentralEntry {
            name: e.name,
            crc,
            size: e.size,
            zip64: e.zip64,
            header_offset: e.header_offset,
            dos: e.dos,
        });
        Ok(b.freeze())
    }

    /// Central directory and end records.
    pub fn finish(self) -> Bytes {
        assert!(self.open.is_none(), "finish with an entry still open");
        let limits = self.limits;
        let cd_offset = self.offset;
        let mut b = BytesMut::new();
        for e in &self.central {
            let offset_zip64 = !fits(e.header_offset, limits.u32_max);
            let mut extra = BytesMut::new();
            if e.zip64 || offset_zip64 {
                extra.put_u16_le(ZIP64_EXTRA_ID);
                let len = if e.zip64 { 16 } else { 0 } + if offset_zip64 { 8 } else { 0 };
                extra.put_u16_le(len);
                if e.zip64 {
                    extra.put_u64_le(e.size);
                    extra.put_u64_le(e.size);
                }
                if offset_zip64 {
                    extra.put_u64_le(e.header_offset);
                }
            }
            let version = if e.zip64 || offset_zip64 {
                VERSION_ZIP64
            } else {
                VERSION_DEFAULT
            };
            b.put_u32_le(SIG_CENTRAL);
            b.put_u16_le(MADE_BY_UNIX | VERSION_ZIP64);
            b.put_u16_le(version);
            b.put_u16_le(FLAG_DESCRIPTOR | FLAG_UTF8);
            b.put_u16_le(0);
            b.put_u16_le(e.dos.0);
            b.put_u16_le(e.dos.1);
            b.put_u32_le(e.crc);
            let size32 = if e.zip64 { U32_SENTINEL } else { e.size as u32 };
            b.put_u32_le(size32);
            b.put_u32_le(size32);
            b.put_u16_le(e.name.len() as u16);
            b.put_u16_le(extra.len() as u16);
            b.put_u16_le(0); // comment
            b.put_u16_le(0); // disk
            b.put_u16_le(0); // internal attributes
            b.put_u32_le(EXTERNAL_ATTR_FILE_0644);
            b.put_u32_le(if offset_zip64 {
                U32_SENTINEL
            } else {
                e.header_offset as u32
            });
            b.put_slice(e.name.as_bytes());
            b.put_slice(&extra);
        }
        let cd_size = b.len() as u64;
        let count = self.central.len() as u64;
        let zip64 = !fits(count, limits.u16_max)
            || !fits(cd_size, limits.u32_max)
            || !fits(cd_offset, limits.u32_max);
        if zip64 {
            let eocd64_offset = cd_offset + cd_size;
            b.put_u32_le(SIG_ZIP64_EOCD);
            b.put_u64_le(44); // record size after this field
            b.put_u16_le(MADE_BY_UNIX | VERSION_ZIP64);
            b.put_u16_le(VERSION_ZIP64);
            b.put_u32_le(0);
            b.put_u32_le(0);
            b.put_u64_le(count);
            b.put_u64_le(count);
            b.put_u64_le(cd_size);
            b.put_u64_le(cd_offset);
            b.put_u32_le(SIG_ZIP64_LOCATOR);
            b.put_u32_le(0);
            b.put_u64_le(eocd64_offset);
            b.put_u32_le(1);
        }
        b.put_u32_le(SIG_EOCD);
        b.put_u16_le(0);
        b.put_u16_le(0);
        // With ZIP64 every field is the sentinel, so a reader must take the
        // ZIP64 record (and the tests prove it reads it).
        let count16 = if zip64 { U16_SENTINEL } else { count as u16 };
        b.put_u16_le(count16);
        b.put_u16_le(count16);
        b.put_u32_le(if zip64 { U32_SENTINEL } else { cd_size as u32 });
        b.put_u32_le(if zip64 {
            U32_SENTINEL
        } else {
            cd_offset as u32
        });
        b.put_u16_le(0);
        b.freeze()
    }
}

// ---------------------------------------------------------------------------
// Async driver
// ---------------------------------------------------------------------------

/// One entry's body: its announced size, its time, and its bytes.
pub struct EntrySource {
    pub size: u64,
    pub modified: DateTime<Utc>,
    pub body: BoxStream<'static, io::Result<Bytes>>,
}

/// A body already in memory, sent in bounded chunks.
pub fn in_memory_body(data: Vec<u8>) -> BoxStream<'static, io::Result<Bytes>> {
    const CHUNK: usize = 256 * 1024;
    let data = Bytes::from(data);
    let chunks: Vec<io::Result<Bytes>> = (0..data.len())
        .step_by(CHUNK)
        .map(|i| Ok(data.slice(i..(i + CHUNK).min(data.len()))))
        .collect();
    futures::stream::iter(chunks).boxed()
}

pub struct Entry<K> {
    /// Name inside the archive.
    pub name: String,
    /// Name for the skip report.
    pub label: String,
    /// What `open` needs to read it.
    pub key: K,
    /// Already opened (the handler opens the first entry before it answers).
    pub opened: Option<EntrySource>,
}

/// Archive bytes in flight: this many chunks at most, each one backend
/// chunk or one header.
const CHANNEL_DEPTH: usize = 8;

/// Stream the archive. `open` reads an entry; an `Err(reason)` there skips
/// it (nothing of it is written yet) and `skipped` gains `(label, reason)`.
/// `trailer` gets the written names and every skipped entry, and may return
/// one last entry (the skip report). A body error, a size mismatch or a
/// client that goes away ends the stream before the central directory.
pub fn stream_archive<K, Open, Fut, Trailer>(
    limits: Limits,
    entries: Vec<Entry<K>>,
    mut skipped: Vec<(String, String)>,
    mut open: Open,
    trailer: Trailer,
) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static
where
    K: Send + 'static,
    Open: FnMut(&K) -> Fut + Send + 'static,
    Fut: Future<Output = Result<EntrySource, String>> + Send,
    Trailer: FnOnce(&[String], &[(String, String)]) -> Option<(String, Vec<u8>)> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(CHANNEL_DEPTH);
    tokio::spawn(async move {
        let mut w = ZipWriter::new(limits);
        let mut written: Vec<String> = Vec::with_capacity(entries.len() + 1);
        for entry in entries {
            let src = match entry.opened {
                Some(src) => src,
                None => match open(&entry.key).await {
                    Ok(src) => src,
                    Err(reason) => {
                        skipped.push((entry.label, reason));
                        continue;
                    }
                },
            };
            match pump(&tx, &mut w, &entry.name, src).await {
                Ok(()) => written.push(entry.name),
                Err(Stop::Closed) => return,
                Err(Stop::Failed(e)) => {
                    tracing::warn!(
                        "zip: {} failed mid-stream, download aborted: {e}",
                        entry.label
                    );
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
        if let Some((name, data)) = trailer(&written, &skipped) {
            let src = EntrySource {
                size: data.len() as u64,
                modified: Utc::now(),
                body: in_memory_body(data),
            };
            match pump(&tx, &mut w, &name, src).await {
                Ok(()) => {}
                Err(Stop::Closed) => return,
                Err(Stop::Failed(e)) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
        let _ = tx.send(Ok(w.finish())).await;
    });
    futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) })
}

enum Stop {
    /// The client went away: stop without a word.
    Closed,
    /// The entry could not be read to its end: abort the download.
    Failed(io::Error),
}

async fn pump(
    tx: &tokio::sync::mpsc::Sender<io::Result<Bytes>>,
    w: &mut ZipWriter,
    name: &str,
    src: EntrySource,
) -> Result<(), Stop> {
    let send = |b: Bytes| async move { tx.send(Ok(b)).await.map_err(|_| Stop::Closed) };
    let header = w
        .start_entry(name, src.size, src.modified)
        .map_err(|e| Stop::Failed(io::Error::other(e)))?;
    send(header).await?;
    let mut body = src.body;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(Stop::Failed)?;
        w.write_data(&chunk);
        send(chunk).await?;
    }
    let descriptor = w
        .finish_entry()
        .map_err(|e| Stop::Failed(io::Error::other(e)))?;
    send(descriptor).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::{Read, Seek, SeekFrom};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 26, 12, 34, 56).unwrap()
    }

    /// Write `entries` in chunks of `chunk` bytes; return the archive.
    fn build(limits: Limits, entries: &[(&str, &[u8])], chunk: usize) -> Vec<u8> {
        let mut w = ZipWriter::new(limits);
        let mut out = Vec::new();
        for (name, data) in entries {
            out.extend_from_slice(&w.start_entry(name, data.len() as u64, t0()).unwrap());
            for c in data.chunks(chunk.max(1)) {
                w.write_data(c);
                out.extend_from_slice(c);
            }
            out.extend_from_slice(&w.finish_entry().unwrap());
        }
        let expected_offset = out.len() as u64;
        assert_eq!(w.offset(), expected_offset);
        out.extend_from_slice(&w.finish());
        out
    }

    fn read_back(archive: Vec<u8>) -> Vec<(String, Vec<u8>)> {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
        (0..z.len())
            .map(|i| {
                let mut f = z.by_index(i).unwrap();
                assert_eq!(f.compression(), zip::CompressionMethod::Stored);
                let mut data = Vec::new();
                // read_to_end checks the CRC-32 at the end of the entry.
                f.read_to_end(&mut data).unwrap();
                (f.name().to_string(), data)
            })
            .collect()
    }

    fn sample() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("a.txt", b"alpha".to_vec()),
            ("empty", Vec::new()),
            (
                "dir/ünïcödé ✓.bin",
                (0..70_000u32).map(|i| (i % 251) as u8).collect(),
            ),
        ]
    }

    #[test]
    fn round_trips_through_a_zip_reader() {
        let s = sample();
        let entries: Vec<(&str, &[u8])> = s.iter().map(|(n, d)| (*n, d.as_slice())).collect();
        for chunk in [1, 7, 65_536] {
            let got = read_back(build(Limits::ZIP, &entries, chunk));
            let want: Vec<(String, Vec<u8>)> =
                s.iter().map(|(n, d)| (n.to_string(), d.clone())).collect();
            assert_eq!(got, want, "chunk {chunk}");
        }
    }

    /// Low limits put every ZIP64 record in a small archive: entry sizes,
    /// header offsets, the entry count and the directory position all
    /// overflow, and the reader must take the ZIP64 values.
    #[test]
    fn zip64_records_round_trip() {
        let s = sample();
        let entries: Vec<(&str, &[u8])> = s.iter().map(|(n, d)| (*n, d.as_slice())).collect();
        let limits = Limits {
            u32_max: 3,
            u16_max: 2,
        };
        let archive = build(limits, &entries, 4096);
        // The ZIP64 end record and locator are present.
        assert!(archive
            .windows(4)
            .any(|w| w == SIG_ZIP64_EOCD.to_le_bytes()));
        assert!(archive
            .windows(4)
            .any(|w| w == SIG_ZIP64_LOCATOR.to_le_bytes()));
        let got = read_back(archive);
        assert_eq!(got.len(), 3);
        assert_eq!(got[2].1, s[2].1);
    }

    #[test]
    fn a_short_or_long_entry_is_an_error() {
        let mut w = ZipWriter::new(Limits::ZIP);
        w.start_entry("x", 10, t0()).unwrap();
        w.write_data(b"12345");
        assert_eq!(
            w.finish_entry().unwrap_err(),
            ZipError::SizeMismatch {
                name: "x".into(),
                expected: 10,
                actual: 5
            }
        );
        w.start_entry("y", 1, t0()).unwrap();
        w.write_data(b"12");
        assert!(w.finish_entry().is_err());
    }

    #[test]
    fn dos_time_is_utc_and_clamped_to_1980() {
        // 12:34:56 → 12<<11 | 34<<5 | 28; 2026-09-26 → 46<<9 | 9<<5 | 26.
        assert_eq!(
            dos_datetime(t0()),
            ((12 << 11) | (34 << 5) | 28, (46 << 9) | (9 << 5) | 26)
        );
        let old = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(dos_datetime(old), (0, (1 << 5) | 1));
    }

    /// An archive held as segments: literal bytes, or a run of zeros that
    /// is never allocated. Read + Seek, so a zip reader can open it.
    struct Sparse {
        segs: Vec<(u64, Seg)>, // (start offset, segment)
        len: u64,
        pos: u64,
    }
    enum Seg {
        Bytes(Vec<u8>),
        Zeros(u64),
    }
    impl Sparse {
        fn new() -> Self {
            Self {
                segs: Vec::new(),
                len: 0,
                pos: 0,
            }
        }
        fn push_bytes(&mut self, b: &[u8]) {
            self.segs.push((self.len, Seg::Bytes(b.to_vec())));
            self.len += b.len() as u64;
        }
        fn push_zeros(&mut self, n: u64) {
            self.segs.push((self.len, Seg::Zeros(n)));
            self.len += n;
        }
    }
    impl Read for Sparse {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.len || buf.is_empty() {
                return Ok(0);
            }
            let i = self.segs.partition_point(|(start, _)| *start <= self.pos) - 1;
            let (start, seg) = &self.segs[i];
            let at = self.pos - start;
            let n = match seg {
                Seg::Bytes(b) => {
                    let n = buf.len().min(b.len() - at as usize);
                    buf[..n].copy_from_slice(&b[at as usize..at as usize + n]);
                    n
                }
                Seg::Zeros(z) => {
                    let n = (buf.len() as u64).min(z - at) as usize;
                    buf[..n].fill(0);
                    n
                }
            };
            self.pos += n as u64;
            Ok(n)
        }
    }
    impl Seek for Sparse {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.pos = match to {
                SeekFrom::Start(p) => p,
                SeekFrom::End(d) => (self.len as i64 + d) as u64,
                SeekFrom::Current(d) => (self.pos as i64 + d) as u64,
            };
            Ok(self.pos)
        }
    }

    /// A real entry above 4 GiB, from a source of zeros that is never held
    /// in memory: the writer takes the ZIP64 path by the format's own
    /// limits, and a zip reader reads the entry back with a correct CRC.
    #[test]
    fn a_4_gib_entry_uses_zip64() {
        const BIG: u64 = (4 << 30) + 12_345;
        let zeros = vec![0u8; 1 << 20];
        let mut w = ZipWriter::new(Limits::ZIP);
        let mut out = Sparse::new();
        out.push_bytes(&w.start_entry("small.txt", 5, t0()).unwrap());
        w.write_data(b"hello");
        out.push_bytes(b"hello");
        out.push_bytes(&w.finish_entry().unwrap());
        out.push_bytes(&w.start_entry("big.bin", BIG, t0()).unwrap());
        let mut left = BIG;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            w.write_data(&zeros[..n]);
            left -= n as u64;
        }
        out.push_zeros(BIG);
        out.push_bytes(&w.finish_entry().unwrap());
        // This entry's header starts above 4 GiB: its offset needs ZIP64.
        out.push_bytes(&w.start_entry("after.txt", 3, t0()).unwrap());
        w.write_data(b"end");
        out.push_bytes(b"end");
        out.push_bytes(&w.finish_entry().unwrap());
        out.push_bytes(&w.finish());

        let mut z = zip::ZipArchive::new(out).unwrap();
        assert_eq!(z.len(), 3);
        {
            let mut big = z.by_name("big.bin").unwrap();
            assert_eq!(big.size(), BIG);
            // io::copy through the reader verifies the CRC at the end.
            assert_eq!(io::copy(&mut big, &mut io::sink()).unwrap(), BIG);
        }
        let mut after = String::new();
        z.by_name("after.txt")
            .unwrap()
            .read_to_string(&mut after)
            .unwrap();
        assert_eq!(after, "end");
    }

    fn src(data: &'static [u8]) -> EntrySource {
        EntrySource {
            size: data.len() as u64,
            modified: t0(),
            body: in_memory_body(data.to_vec()),
        }
    }

    fn entry(name: &str) -> Entry<String> {
        Entry {
            name: name.to_string(),
            label: format!("b/{name}"),
            key: name.to_string(),
            opened: None,
        }
    }

    async fn collect(s: impl Stream<Item = io::Result<Bytes>>) -> (Vec<u8>, Option<io::Error>) {
        let mut out = Vec::new();
        futures::pin_mut!(s);
        while let Some(item) = s.next().await {
            match item {
                Ok(b) => out.extend_from_slice(&b),
                Err(e) => return (out, Some(e)),
            }
        }
        (out, None)
    }

    #[tokio::test]
    async fn the_driver_skips_unopenable_entries_and_appends_the_trailer() {
        let mut first = entry("one.txt");
        first.opened = Some(src(b"first"));
        let entries = vec![first, entry("missing.txt"), entry("two.txt")];
        let s = stream_archive(
            Limits::ZIP,
            entries,
            vec![("b/denied.txt".into(), "AccessDenied".into())],
            |k: &String| {
                let k = k.clone();
                async move {
                    match k.as_str() {
                        "two.txt" => Ok(src(b"second")),
                        _ => Err("NoSuchKey".to_string()),
                    }
                }
            },
            |written: &[String], skipped: &[(String, String)]| {
                assert_eq!(written, ["one.txt", "two.txt"]);
                let report: Vec<String> =
                    skipped.iter().map(|(l, r)| format!("{l}: {r}")).collect();
                Some(("skipped.txt".into(), report.join("\n").into_bytes()))
            },
        );
        let (bytes, err) = collect(s).await;
        assert!(err.is_none());
        let got = read_back(bytes);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["one.txt", "two.txt", "skipped.txt"]);
        assert_eq!(got[1].1, b"second");
        assert_eq!(
            String::from_utf8(got[2].1.clone()).unwrap(),
            "b/denied.txt: AccessDenied\nb/missing.txt: NoSuchKey"
        );
    }

    /// A body that fails mid-stream, or ends short of its announced size,
    /// ends the stream with an error and without a central directory.
    #[tokio::test]
    async fn a_mid_stream_failure_aborts_before_the_directory() {
        let failing = || EntrySource {
            size: 10,
            modified: t0(),
            body: futures::stream::iter(vec![
                Ok(Bytes::from_static(b"12345")),
                Err(io::Error::other("backend reset")),
            ])
            .boxed(),
        };
        let short = || EntrySource {
            size: 10,
            modified: t0(),
            body: in_memory_body(b"12345".to_vec()),
        };
        for make in [failing as fn() -> EntrySource, short] {
            let mut a = entry("ok.txt");
            a.opened = Some(src(b"fine"));
            let mut b = entry("bad.bin");
            b.opened = Some(make());
            let s = stream_archive(
                Limits::ZIP,
                vec![a, b, entry("never.txt")],
                vec![],
                |_: &String| async { panic!("no entry after the failure is opened") },
                |_: &[String], _: &[(String, String)]| panic!("no trailer after a failure"),
            );
            let (bytes, err) = collect(s).await;
            assert!(err.is_some());
            assert!(!bytes.windows(4).any(|w| w == SIG_CENTRAL.to_le_bytes()));
            assert!(!bytes.windows(4).any(|w| w == SIG_EOCD.to_le_bytes()));
            assert!(zip::ZipArchive::new(std::io::Cursor::new(bytes)).is_err());
        }
    }

    /// The producer runs at most a channel's depth ahead of the client
    /// (backpressure, so memory is bounded), and a client that goes away
    /// stops it.
    #[tokio::test]
    async fn the_producer_waits_for_the_client_and_stops_without_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let opened = std::sync::Arc::new(AtomicUsize::new(0));
        let entries: Vec<Entry<String>> = (0..1000).map(|i| entry(&format!("{i}"))).collect();
        let counter = opened.clone();
        let mut s = Box::pin(stream_archive(
            Limits::ZIP,
            entries,
            vec![],
            move |_: &String| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Ok(src(b"x")) }
            },
            |_: &[String], _: &[(String, String)]| None,
        ));
        s.next().await.unwrap().unwrap();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        // Three chunks per entry (header, data, descriptor).
        let ahead = opened.load(Ordering::SeqCst);
        assert!(
            ahead <= CHANNEL_DEPTH / 3 + 2,
            "producer ran {ahead} entries ahead"
        );
        drop(s);
        // The producer task ends and drops `open` (and its counter clone).
        for _ in 0..1000 {
            if std::sync::Arc::strong_count(&opened) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            std::sync::Arc::strong_count(&opened),
            1,
            "producer still runs"
        );
        assert!(opened.load(Ordering::SeqCst) <= ahead + 1);
    }
}
