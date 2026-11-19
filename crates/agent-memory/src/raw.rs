//! Append-only immutable raw history segments and verified exact-byte readback.
//!
//! Indexes store coordinates ([`RawCoordinate`]), never copies of raw events.
//! Framed segments authenticate both headers and payloads with SHA-256 and
//! publish an explicit acknowledgement frontier (`.ack`); existing JSONL files
//! can be scanned read-only or appended in place without reframing.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAGIC_V1: &[u8; 8] = b"AGRAW001";
const MAGIC_V2: &[u8; 8] = b"AGRAW002";
const HEADER_V1: u64 = 56;
// v2 keeps the v1 fields and adds a SHA-256 checksum of all header fields.
const HEADER_V2: u64 = 88;

/// Maximum bytes returned by a single bounded exact-byte readback ([`read_raw_range`]).
pub const MAX_RAW_RANGE_BYTES: u64 = 64 * 1024;

/// Compute lowercase hexadecimal SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Verified raw byte coordinate inside an immutable segment file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawCoordinate {
    pub segment: PathBuf,
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
    pub seq: u64,
}

/// Filesystem identity stamp used to detect modifications across reads.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub device: u64,
    pub inode: u64,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
}

impl FileStamp {
    pub fn read(file: &File) -> io::Result<Self> {
        Ok(Self::from_metadata(&file.metadata()?))
    }

    #[cfg(unix)]
    pub fn from_metadata(m: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            len: m.len(),
            device: m.dev(),
            inode: m.ino(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        }
    }

    #[cfg(not(unix))]
    pub fn from_metadata(m: &fs::Metadata) -> Self {
        let modified = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs() as i64, d.subsec_nanos() as i64))
            .unwrap_or((0, 0));
        Self {
            len: m.len(),
            device: 0,
            inode: 0,
            modified,
            changed: modified,
        }
    }

    pub fn same_file(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegmentFormat {
    FramedV1,
    FramedV2,
    Jsonl,
}

/// Append-only raw event segment backed by either integrity-framed binary records
/// or newline-delimited JSON.
pub struct RawSegment {
    path: PathBuf,
    format: SegmentFormat,
    append_cursor: Mutex<Option<AppendCursor>>,
    #[cfg(test)]
    full_validations: std::sync::atomic::AtomicUsize,
}

struct AppendCursor {
    stamp: FileStamp,
    next_seq: u64,
}

struct LockedFile(File);

impl Drop for LockedFile {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock on an owned live file descriptor.
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

impl RawSegment {
    /// Open a segment and infer its on-disk format.
    ///
    /// Empty files start in the SHA-256 framed v2 format. Existing JSONL files
    /// are detected in place without copying or reframing.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let format = detect_format(path)?;
        let segment = Self {
            path: path.to_owned(),
            format,
            append_cursor: Mutex::new(None),
            #[cfg(test)]
            full_validations: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut file = segment.lock()?;
        let records = segment.recover(&mut file.0)?;
        segment.cache_append_cursor(&file.0, records.last().map_or(1, |r| r.seq + 1))?;
        Ok(segment)
    }

    /// Open a path explicitly as JSONL (needed for empty JSONL segments).
    pub fn open_jsonl(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let segment = Self {
            path: path.to_owned(),
            format: SegmentFormat::Jsonl,
            append_cursor: Mutex::new(None),
            #[cfg(test)]
            full_validations: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut file = segment.lock()?;
        let records = scan(&mut file.0, &segment.path, segment.format, false)?;
        segment.cache_append_cursor(&file.0, records.last().map_or(1, |r| r.seq + 1))?;
        Ok(segment)
    }

    /// Path of the underlying segment file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn recover(&self, file: &mut File) -> io::Result<Vec<RawCoordinate>> {
        #[cfg(test)]
        self.full_validations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.format == SegmentFormat::Jsonl {
            return scan_jsonl(file, &self.path, false);
        }
        let size = file.metadata()?.len();
        let frontier = read_frontier(&self.path, size)?;
        if frontier > size {
            return Err(io::Error::other("acknowledged raw segment shortened"));
        }
        let records = scan_framed(file, &self.path, self.format, false, Some(frontier))?;
        if size == frontier {
            return Ok(records);
        }
        match scan_framed(file, &self.path, self.format, false, None) {
            Ok(complete) => {
                // Adopt complete frames written before a crash prior to frontier update.
                write_frontier(&self.path, size)?;
                Ok(complete)
            }
            Err(_) => {
                // Quarantine torn/unverified suffix with a SHA-256 readback receipt before truncating.
                quarantine_tail(file, &self.path, frontier, size)?;
                file.set_len(frontier)?;
                file.sync_all()?;
                Ok(records)
            }
        }
    }

    fn lock(&self) -> io::Result<LockedFile> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: exclusive advisory lock on owned file descriptor.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(LockedFile(file))
    }

    fn cache_append_cursor(&self, file: &File, next_seq: u64) -> io::Result<()> {
        *self
            .append_cursor
            .lock()
            .map_err(|_| io::Error::other("raw append cursor poisoned"))? = Some(AppendCursor {
            stamp: FileStamp::read(file)?,
            next_seq,
        });
        Ok(())
    }

    /// Durably append `payload` and return its authenticated [`RawCoordinate`].
    pub fn append(&self, payload: &[u8]) -> io::Result<RawCoordinate> {
        let mut file = self.lock()?;
        let stamp = FileStamp::read(&file.0)?;
        let cached = self
            .append_cursor
            .lock()
            .map_err(|_| io::Error::other("raw append cursor poisoned"))?
            .take();
        let frontier = if self.format == SegmentFormat::Jsonl {
            stamp.len
        } else {
            read_frontier(&self.path, stamp.len)?
        };
        let next_seq = if let Some(cursor) = cached
            && cursor.stamp == stamp
            && frontier == stamp.len
        {
            cursor.next_seq
        } else {
            self.recover(&mut file.0)?.last().map_or(1, |r| r.seq + 1)
        };
        let coordinate = match self.format {
            SegmentFormat::Jsonl => append_jsonl(&mut file.0, &self.path, payload, next_seq)?,
            SegmentFormat::FramedV1 | SegmentFormat::FramedV2 => {
                let coordinate =
                    append_framed(&mut file.0, &self.path, self.format, payload, next_seq)?;
                write_frontier(&self.path, file.0.metadata()?.len())?;
                coordinate
            }
        };
        self.cache_append_cursor(&file.0, coordinate.seq + 1)?;
        Ok(coordinate)
    }

    /// Validate and enumerate every acknowledged record in the segment.
    pub fn records(&self) -> io::Result<Vec<RawCoordinate>> {
        let mut file = self.lock()?;
        *self
            .append_cursor
            .lock()
            .map_err(|_| io::Error::other("raw append cursor poisoned"))? = None;
        let records = self.recover(&mut file.0)?;
        self.cache_append_cursor(&file.0, records.last().map_or(1, |r| r.seq + 1))?;
        Ok(records)
    }
}

/// Open a retained file read-only without following a symlink at any path component
/// and requiring a single-link regular file on Unix.
pub fn open_readonly(path: &Path) -> io::Result<File> {
    open_readonly_with_metadata(path).map(|(file, _)| file)
}

fn open_readonly_with_metadata(path: &Path) -> io::Result<(File, fs::Metadata)> {
    #[cfg(unix)]
    {
        use std::{
            ffi::CString,
            os::fd::{AsRawFd, FromRawFd},
            os::unix::{ffi::OsStrExt, fs::MetadataExt},
        };
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let parts: Vec<_> = absolute
            .components()
            .filter_map(|c| match c {
                std::path::Component::RootDir => None,
                std::path::Component::Normal(s) => Some(Ok(s)),
                _ => Some(Err(io::Error::other("unsupported raw path component"))),
            })
            .collect::<io::Result<_>>()?;
        let mut directory = File::open("/")?;
        for (i, part) in parts.iter().enumerate() {
            let name = CString::new(part.as_bytes()).map_err(io::Error::other)?;
            let final_part = i + 1 == parts.len();
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if final_part { 0 } else { libc::O_DIRECTORY };
            // SAFETY: openat with valid directory fd and NUL-terminated path component.
            let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd was just opened and ownership transfers to `File`.
            let file = unsafe { File::from_raw_fd(fd) };
            if final_part {
                let meta = file.metadata()?;
                if !meta.is_file() || meta.nlink() != 1 {
                    return Err(io::Error::other(
                        "raw source must be a single-link regular file",
                    ));
                }
                return Ok((file, meta));
            }
            directory = file;
        }
        Err(io::Error::other("raw source needs a file path"))
    }
    #[cfg(not(unix))]
    {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        Ok((file, meta))
    }
}

/// Positioned read-ahead view over one retained file opened through [`open_readonly`].
#[cfg(feature = "sqlite")]
pub(crate) struct RawSource {
    path: PathBuf,
    file: File,
    stamp: FileStamp,
    window: Vec<u8>,
    window_at: u64,
    window_len: usize,
}

#[cfg(feature = "sqlite")]
const MIN_WINDOW: u64 = 4 * 1024;
#[cfg(feature = "sqlite")]
const READ_AHEAD: u64 = 64 * 1024;
#[cfg(feature = "sqlite")]
const MAX_WINDOW: u64 = 1024 * 1024;

#[cfg(feature = "sqlite")]
impl RawSource {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let (file, metadata) = open_readonly_with_metadata(path)?;
        Ok(Self {
            path: path.to_owned(),
            file,
            stamp: FileStamp::from_metadata(&metadata),
            window: Vec::new(),
            window_at: 0,
            window_len: 0,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn range(&mut self, offset: u64, length: u64) -> SourceRange<'_> {
        SourceRange {
            source: self,
            offset,
            end: offset.saturating_add(length),
        }
    }

    pub(crate) fn verify(&mut self, raw: &RawCoordinate) -> io::Result<()> {
        let (length, sha256) = hash_all(self.range(raw.offset, raw.length))?;
        if length != raw.length || sha256 != raw.sha256 {
            return Err(io::Error::other("index coordinate integrity mismatch"));
        }
        Ok(())
    }

    fn fill(&mut self, offset: u64, wanted: u64) -> io::Result<&[u8]> {
        let held = self.window_at..self.window_at + self.window_len as u64;
        if !held.contains(&offset) {
            let size = wanted
                .clamp(READ_AHEAD, MAX_WINDOW)
                .min(self.stamp.len.max(MIN_WINDOW)) as usize;
            if self.window.len() < size {
                self.window.resize(size, 0);
            }
            self.window_at = offset;
            self.window_len = 0;
            let mut reader = ReadAt {
                file: &mut self.file,
                offset,
            };
            while self.window_len < size {
                match reader.read(&mut self.window[self.window_len..size]) {
                    Ok(0) => break,
                    Ok(n) => self.window_len += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        self.window_len = 0;
                        return Err(e);
                    }
                }
            }
        }
        let start = (offset - self.window_at) as usize;
        Ok(&self.window[start..self.window_len])
    }
}

#[cfg(feature = "sqlite")]
pub(crate) struct SourceRange<'a> {
    source: &'a mut RawSource,
    offset: u64,
    end: u64,
}

#[cfg(feature = "sqlite")]
impl Read for SourceRange<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(output.len());
        output[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

#[cfg(feature = "sqlite")]
impl BufRead for SourceRange<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.offset >= self.end {
            return Ok(&[]);
        }
        let wanted = self.end - self.offset;
        let bytes = self.source.fill(self.offset, wanted)?;
        Ok(&bytes[..bytes
            .len()
            .min(usize::try_from(wanted).unwrap_or(usize::MAX))])
    }

    fn consume(&mut self, amount: usize) {
        self.offset += amount as u64;
    }
}

#[cfg(feature = "sqlite")]
struct ReadAt<'a> {
    file: &'a mut File,
    offset: u64,
}

#[cfg(feature = "sqlite")]
impl Read for ReadAt<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(self.file, output, self.offset)?;
        #[cfg(not(unix))]
        let n = {
            self.file.seek(SeekFrom::Start(self.offset))?;
            self.file.read(output)?
        };
        self.offset += n as u64;
        Ok(n)
    }
}

#[cfg(feature = "sqlite")]
fn hash_all(mut reader: impl BufRead) -> io::Result<(u64, String)> {
    let mut hash = Sha256::new();
    let mut length = 0u64;
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            return Ok((length, format!("{:x}", hash.finalize())));
        }
        hash.update(bytes);
        length += bytes.len() as u64;
        let n = bytes.len();
        reader.consume(n);
    }
}

/// One JSONL line read from a buffered stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JsonlLine {
    /// A newline-terminated line holding an indexed raw coordinate.
    Complete { length: u64, sha256: String },
    /// Final bytes with no newline yet: never validated or committed.
    Partial { length: u64 },
}

pub(crate) fn read_jsonl_line(reader: &mut impl BufRead) -> io::Result<Option<JsonlLine>> {
    read_jsonl_line_with_policy(reader, false)
}

pub(crate) fn read_jsonl_line_with_policy(
    reader: &mut impl BufRead,
    tolerate_malformed: bool,
) -> io::Result<Option<JsonlLine>> {
    let mut line = LineState {
        hash: Sha256::new(),
        length: 0,
        complete: false,
    };
    let parsed = serde_json::from_reader::<_, serde::de::IgnoredAny>(BufReader::with_capacity(
        8 * 1024,
        Line {
            reader: &mut *reader,
            state: &mut line,
        },
    ));
    io::copy(
        &mut Line {
            reader: &mut *reader,
            state: &mut line,
        },
        &mut io::sink(),
    )?;
    if line.length == 0 {
        return Ok(None);
    }
    if !line.complete {
        return Ok(Some(JsonlLine::Partial {
            length: line.length,
        }));
    }
    if let Err(e) = parsed {
        if e.classify() == serde_json::error::Category::Io {
            return Err(io::Error::new(
                e.io_error_kind().unwrap_or(io::ErrorKind::Other),
                e,
            ));
        }
        if !tolerate_malformed {
            return Err(io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
    Ok(Some(JsonlLine::Complete {
        length: line.length,
        sha256: format!("{:x}", line.hash.finalize()),
    }))
}

struct LineState {
    hash: Sha256,
    length: u64,
    complete: bool,
}

struct Line<'a, R> {
    reader: &'a mut R,
    state: &'a mut LineState,
}

impl<R: BufRead> Read for Line<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.state.complete || output.is_empty() {
            return Ok(0);
        }
        let bytes = self.reader.fill_buf()?;
        let bytes = &bytes[..bytes.len().min(output.len())];
        let n = match bytes.iter().position(|b| *b == b'\n') {
            Some(newline) => {
                self.state.complete = true;
                newline + 1
            }
            None => bytes.len(),
        };
        output[..n].copy_from_slice(&bytes[..n]);
        self.state.hash.update(&bytes[..n]);
        self.state.length += n as u64;
        self.reader.consume(n);
        Ok(n)
    }
}

/// Read-only indexing entry point for a JSONL file.
/// Incomplete final bytes without a trailing newline are left untouched.
pub fn retained_jsonl_records(path: &Path) -> io::Result<Vec<RawCoordinate>> {
    let mut file = open_readonly(path)?;
    scan_jsonl(&mut file, path, true)
}

fn quarantine_tail(file: &mut File, path: &Path, start: u64, end: u64) -> io::Result<()> {
    let quarantine = path.with_file_name(format!(
        "{}.uncertain-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::now_v7()
    ));
    let mut out = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&quarantine)?;
    file.seek(SeekFrom::Start(start))?;
    let mut source_hash = Sha256::new();
    let mut remaining = end - start;
    let mut buffer = [0u8; 65536];
    while remaining > 0 {
        let n = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..n])?;
        out.write_all(&buffer[..n])?;
        source_hash.update(&buffer[..n]);
        remaining -= n as u64;
    }
    out.sync_all()?;
    let source_hash = format!("{:x}", source_hash.finalize());
    let mut readback = File::open(&quarantine)?;
    let mut hash = Sha256::new();
    let mut count = 0u64;
    loop {
        let n = readback.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        count += n as u64;
    }
    if count != end - start || format!("{:x}", hash.finalize()) != source_hash {
        return Err(io::Error::other("uncertain raw tail readback mismatch"));
    }
    let receipt = serde_json::json!({
        "source": path,
        "offset": start,
        "bytes": count,
        "destination": quarantine,
        "sha256": source_hash,
        "readback_verified": true,
    });
    crate::notes::atomic_write(
        &PathBuf::from(format!("{}.receipt.json", quarantine.display())),
        &serde_json::to_vec(&receipt).map_err(io::Error::other)?,
    )?;
    Ok(())
}

fn frontier_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".ack");
    PathBuf::from(name)
}

fn write_frontier(path: &Path, end: u64) -> io::Result<()> {
    let mut bytes = end.to_le_bytes().to_vec();
    let hash = Sha256::digest(&bytes);
    bytes.extend_from_slice(&hash);
    crate::notes::atomic_write(&frontier_path(path), &bytes)
}

fn read_frontier(path: &Path, size: u64) -> io::Result<u64> {
    let bytes = match fs::read(frontier_path(path)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound && size == 0 => {
            write_frontier(path, 0)?;
            return Ok(0);
        }
        Err(e) => return Err(e),
    };
    if bytes.len() != 40 || Sha256::digest(&bytes[..8])[..] != bytes[8..] {
        return Err(io::Error::other(
            "raw acknowledgement frontier integrity mismatch",
        ));
    }
    Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

fn detect_format(path: &Path) -> io::Result<SegmentFormat> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let mut magic = [0u8; 8];
    let n = file.read(&mut magic)?;
    if n == 0 {
        return Ok(SegmentFormat::FramedV2);
    }
    if n == 8 && &magic == MAGIC_V1 {
        Ok(SegmentFormat::FramedV1)
    } else if (n == 8 && &magic == MAGIC_V2) || frontier_path(path).try_exists()? {
        Ok(SegmentFormat::FramedV2)
    } else {
        scan_jsonl(&mut file, path, false)?;
        Ok(SegmentFormat::Jsonl)
    }
}

fn append_framed(
    file: &mut File,
    path: &Path,
    format: SegmentFormat,
    payload: &[u8],
    seq: u64,
) -> io::Result<RawCoordinate> {
    let start = file.seek(SeekFrom::End(0))?;
    let hash = Sha256::digest(payload);
    let mut header = [0u8; HEADER_V2 as usize];
    header[..8].copy_from_slice(match format {
        SegmentFormat::FramedV1 => MAGIC_V1,
        SegmentFormat::FramedV2 => MAGIC_V2,
        SegmentFormat::Jsonl => unreachable!(),
    });
    header[8..16].copy_from_slice(&seq.to_le_bytes());
    header[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[24..56].copy_from_slice(&hash);
    if format == SegmentFormat::FramedV2 {
        let header_hash = Sha256::digest(&header[..56]);
        header[56..88].copy_from_slice(&header_hash);
        file.write_all(&header)?;
    } else {
        file.write_all(&header[..HEADER_V1 as usize])?;
    }
    file.write_all(payload)?;
    file.sync_all()?;
    Ok(RawCoordinate {
        segment: path.to_owned(),
        offset: start
            + match format {
                SegmentFormat::FramedV1 => HEADER_V1,
                SegmentFormat::FramedV2 => HEADER_V2,
                SegmentFormat::Jsonl => unreachable!(),
            },
        length: payload.len() as u64,
        sha256: format!("{hash:x}"),
        seq,
    })
}

fn append_jsonl(
    file: &mut File,
    path: &Path,
    payload: &[u8],
    seq: u64,
) -> io::Result<RawCoordinate> {
    let end = file.seek(SeekFrom::End(0))?;
    if end > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            return Err(io::Error::other("incomplete JSONL final line"));
        }
    }
    let json = payload.strip_suffix(b"\n").unwrap_or(payload);
    if json.contains(&b'\n') || serde_json::from_slice::<serde_json::Value>(json).is_err() {
        return Err(io::Error::other(
            "JSONL append requires one valid JSON value",
        ));
    }
    let start = file.seek(SeekFrom::End(0))?;
    let line = if payload.ends_with(b"\n") {
        payload.to_vec()
    } else {
        let mut line = Vec::with_capacity(payload.len() + 1);
        line.extend_from_slice(payload);
        line.push(b'\n');
        line
    };
    let hash = Sha256::digest(&line);
    file.write_all(&line)?;
    file.sync_all()?;
    Ok(RawCoordinate {
        segment: path.to_owned(),
        offset: start,
        length: line.len() as u64,
        sha256: format!("{hash:x}"),
        seq,
    })
}

fn scan(
    file: &mut File,
    path: &Path,
    format: SegmentFormat,
    recover_tail: bool,
) -> io::Result<Vec<RawCoordinate>> {
    match format {
        SegmentFormat::Jsonl => scan_jsonl(file, path, recover_tail),
        SegmentFormat::FramedV1 | SegmentFormat::FramedV2 => {
            scan_framed(file, path, format, recover_tail, None)
        }
    }
}

fn scan_framed(
    file: &mut File,
    path: &Path,
    format: SegmentFormat,
    _recover_tail: bool,
    limit: Option<u64>,
) -> io::Result<Vec<RawCoordinate>> {
    file.seek(SeekFrom::Start(0))?;
    let size = limit.unwrap_or(file.metadata()?.len());
    let header_len = match format {
        SegmentFormat::FramedV1 => HEADER_V1,
        SegmentFormat::FramedV2 => HEADER_V2,
        SegmentFormat::Jsonl => unreachable!(),
    };
    let mut at = 0;
    let mut records = Vec::new();
    while at < size {
        let remaining = size - at;
        if remaining < header_len {
            return Err(io::Error::other("incomplete raw frame header"));
        }
        let mut header = [0u8; HEADER_V2 as usize];
        file.read_exact(&mut header[..header_len as usize])?;
        let expected_magic = match format {
            SegmentFormat::FramedV1 => MAGIC_V1,
            SegmentFormat::FramedV2 => MAGIC_V2,
            SegmentFormat::Jsonl => unreachable!(),
        };
        if &header[..8] != expected_magic {
            return Err(io::Error::other("invalid raw segment magic"));
        }
        if format == SegmentFormat::FramedV2 {
            let expected = Sha256::digest(&header[..56]);
            if header[56..88] != expected[..] {
                return Err(io::Error::other("raw frame header integrity mismatch"));
            }
        }
        let seq = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let len = u64::from_le_bytes(header[16..24].try_into().unwrap());
        if seq != records.last().map_or(1, |r: &RawCoordinate| r.seq + 1) {
            return Err(io::Error::other("noncontiguous raw sequence"));
        }
        if len > size - at - header_len {
            return Err(io::Error::other("incomplete or corrupt raw frame payload"));
        }
        let mut hasher = Sha256::new();
        let mut remaining = len;
        let mut buf = [0u8; 65536];
        while remaining > 0 {
            let n = remaining.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..n])?;
            hasher.update(&buf[..n]);
            remaining -= n as u64;
        }
        let hash = hasher.finalize();
        if hash[..] != header[24..56] {
            return Err(io::Error::other("raw frame SHA mismatch"));
        }
        records.push(RawCoordinate {
            segment: path.to_owned(),
            offset: at + header_len,
            length: len,
            sha256: format!("{hash:x}"),
            seq,
        });
        at += header_len + len;
    }
    Ok(records)
}

fn scan_jsonl(
    file: &mut File,
    path: &Path,
    allow_partial_tail: bool,
) -> io::Result<Vec<RawCoordinate>> {
    file.seek(SeekFrom::Start(0))?;
    let mut records = Vec::new();
    let mut at = 0u64;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    while let Some(line) = read_jsonl_line(&mut reader)? {
        match line {
            JsonlLine::Complete { length, sha256 } => {
                records.push(RawCoordinate {
                    segment: path.to_owned(),
                    offset: at,
                    length,
                    sha256,
                    seq: records.len() as u64 + 1,
                });
                at += length;
            }
            JsonlLine::Partial { .. } if allow_partial_tail => break,
            JsonlLine::Partial { .. } => {
                return Err(io::Error::other("incomplete JSONL final line"));
            }
        }
    }
    Ok(records)
}

/// Read exact raw bytes for an authenticated [`RawCoordinate`].
/// Fails closed if the length or SHA-256 no longer matches.
pub fn read_raw(coordinate: &RawCoordinate) -> io::Result<Vec<u8>> {
    let mut file = open_readonly(&coordinate.segment)?;
    file.seek(SeekFrom::Start(coordinate.offset))?;
    let mut bytes = Vec::new();
    file.take(coordinate.length).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != coordinate.length || sha256_hex(&bytes) != coordinate.sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "raw coordinate readback mismatch",
        ));
    }
    Ok(bytes)
}

/// Bounded exact-byte readback within a verified event.
///
/// The entire indexed event is streamed and authenticated against `coordinate.sha256`
/// in 64 KiB chunks before returning at most [`MAX_RAW_RANGE_BYTES`], preventing
/// unauthenticated slice reads or unbounded allocations.
pub fn read_raw_range(
    coordinate: &RawCoordinate,
    offset: u64,
    length: u64,
) -> io::Result<(RawCoordinate, Vec<u8>)> {
    if length == 0
        || length > MAX_RAW_RANGE_BYTES
        || offset > coordinate.length
        || offset.checked_add(length).is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw range requires offset within event and length 1..=65536",
        ));
    }
    let end = offset.saturating_add(length).min(coordinate.length);
    let mut file = open_readonly(&coordinate.segment)?;
    let stamp = FileStamp::read(&file)?;
    file.seek(SeekFrom::Start(coordinate.offset))?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut at = 0;
    let mut bytes = Vec::with_capacity((end - offset) as usize);
    while at < coordinate.length {
        let chunk_len = (coordinate.length - at).min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..chunk_len])?;
        hash.update(&buffer[..chunk_len]);
        let from = offset.max(at);
        let to = end.min(at + chunk_len as u64);
        if from < to {
            bytes.extend_from_slice(&buffer[(from - at) as usize..(to - at) as usize]);
        }
        at += chunk_len as u64;
    }
    if format!("{:x}", hash.finalize()) != coordinate.sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "raw coordinate readback mismatch",
        ));
    }
    if FileStamp::read(&open_readonly(&coordinate.segment)?)? != stamp {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "raw source changed during range read; retry",
        ));
    }
    let window = RawCoordinate {
        segment: coordinate.segment.clone(),
        offset: coordinate.offset.checked_add(offset).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "raw range offset overflow")
        })?,
        length: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
        seq: coordinate.seq,
    };
    Ok((window, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_range_authenticates_whole_event_and_bounds_exact_binary_output() {
        let dir = crate::test_tempdir();
        let path = dir.path().join("binary");
        let bytes: Vec<u8> = (0..200_000).map(|n| (n % 256) as u8).collect();
        fs::write(&path, &bytes).unwrap();
        let coordinate = RawCoordinate {
            segment: path.clone(),
            offset: 13,
            length: 180_000,
            sha256: sha256_hex(&bytes[13..180_013]),
            seq: 7,
        };
        let (window, slice) = read_raw_range(&coordinate, 65_530, MAX_RAW_RANGE_BYTES).unwrap();
        assert_eq!(slice, bytes[65_543..131_079]);
        assert_eq!(
            (window.offset, window.length, window.seq),
            (65_543, MAX_RAW_RANGE_BYTES, 7)
        );
        assert_eq!(window.sha256, sha256_hex(&slice));
        assert_eq!(
            read_raw_range(&coordinate, 179_990, 32).unwrap().1,
            bytes[180_003..180_013]
        );
        assert!(
            read_raw_range(&coordinate, 180_000, 1)
                .unwrap()
                .1
                .is_empty()
        );
        for (offset, length) in [
            (0, 0),
            (0, MAX_RAW_RANGE_BYTES + 1),
            (180_001, 1),
            (u64::MAX, 1),
        ] {
            assert_eq!(
                read_raw_range(&coordinate, offset, length)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        // Corruption outside the returned range must still refuse the response.
        let mut modified = bytes;
        modified[100] ^= 1;
        fs::write(&path, modified).unwrap();
        assert_eq!(
            read_raw_range(&coordinate, 65_530, 32).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn readonly_descriptor_walk_refuses_linked_files_and_ancestors() {
        let dir = crate::test_tempdir();
        let plain = dir.path().join("plain");
        fs::create_dir(&plain).unwrap();
        fs::write(plain.join("event"), b"exact").unwrap();
        let mut bytes = Vec::new();
        open_readonly(&plain.join("event"))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"exact");
        std::os::unix::fs::symlink(&plain, dir.path().join("alias")).unwrap();
        assert!(open_readonly(&dir.path().join("alias/event")).is_err());
        fs::hard_link(plain.join("event"), dir.path().join("linked")).unwrap();
        let error = open_readonly(&dir.path().join("linked")).unwrap_err();
        assert!(error.to_string().contains("single-link"));
    }

    #[test]
    fn append_restart_and_torn_tail_recovery() {
        let dir = crate::test_tempdir();
        let path = dir.path().join("segments/first.raw");
        let segment = RawSegment::open(&path).unwrap();
        let first = segment.append(b"{\"tool\":\"web\"}\n\x00\xff").unwrap();
        let second = segment.append("你好\nreasoning".as_bytes()).unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        drop(segment);

        // Append torn bytes past the frontier.
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"AGR")
            .unwrap();
        let restarted = RawSegment::open(&path).unwrap();
        assert_eq!(
            restarted.records().unwrap(),
            vec![first.clone(), second.clone()]
        );
        assert_eq!(read_raw(&first).unwrap(), b"{\"tool\":\"web\"}\n\x00\xff");
        assert_eq!(read_raw(&second).unwrap(), "你好\nreasoning".as_bytes());
        assert_eq!(restarted.append(b"next").unwrap().seq, 3);
    }

    #[test]
    fn jsonl_in_place_append_and_readonly_partial_tail() {
        let dir = crate::test_tempdir();
        let path = dir.path().join("history.jsonl");
        fs::write(&path, b"{\"seq\":1}\n{\"seq\":2}\n").unwrap();
        let segment = RawSegment::open(&path).unwrap();
        assert_eq!(segment.records().unwrap().len(), 2);
        let third = segment.append(br#"{"seq":3}"#).unwrap();
        assert_eq!(third.seq, 3);
        assert_eq!(read_raw(&third).unwrap(), b"{\"seq\":3}\n");

        let partial = dir.path().join("active.jsonl");
        fs::write(&partial, b"{\"event\":1}\n{\"event\":").unwrap();
        let recs = retained_jsonl_records(&partial).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(read_raw(&recs[0]).unwrap(), b"{\"event\":1}\n");
    }
}
