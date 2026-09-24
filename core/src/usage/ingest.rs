//! Reading transcript files into the store (ADR-034 §2).
//!
//! Per file, a refresh decides between three plans from the stored read
//! state and the file on disk:
//!
//! - **skip** — same identity (device, inode), size, mtime and ctime as
//!   stored, compared to the nanosecond;
//! - **append** — same identity, grown past both the stored size and the
//!   stored offset, and the content fingerprint of the part already read
//!   still matches: read from the stored offset with the stored parser
//!   carry;
//! - **re-read** — anything else: delete every observation of the path and
//!   read from byte 0 with an empty carry, in the same transaction. This
//!   covers a new file, `--full`, a different identity, a file that did not
//!   grow but whose mtime or ctime moved (rewritten in place, at any
//!   position), and a grown file whose fingerprint changed.
//!
//! The fingerprint covers the first [`FINGERPRINT_BYTES`] bytes and the
//! [`FINGERPRINT_BYTES`] bytes that end at the stored offset. It is only
//! trusted for a file that grew: an in-place edit that falls between the
//! two windows of a file that also grew in the same interval is not
//! detected; `nucleus usage refresh --full` re-reads everything.
//!
//! A read covers the bytes up to the size `fstat` reported before it. After
//! the read the descriptor is `fstat`ed again: when the identity, mtime or
//! ctime changed and the file did not grow (an in-place write during the
//! read), the file's batch is discarded (its transaction rolls back and the
//! stored state stays as it was), and the file is reported as not read. The
//! next refresh sees metadata that differs from the stored state and plans
//! again. Growth during the read is an append by the writer and is kept:
//! the bytes past the read size are left for the next refresh.
//!
//! Opening: discovery records each regular file's (device, inode) without
//! following links. A file is then opened relative to a descriptor of its
//! discovery root, one component at a time with `O_NOFOLLOW`, and the
//! descriptor is `fstat`ed: it must be a regular file with the discovered
//! identity. All reads use that descriptor. A path swapped for a link out
//! of the root, a directory link, or another file between discovery and
//! reading is refused and reported as a failed file.
//!
//! Memory is bounded: the reader holds one line (at most
//! [`MAX_LINE_BYTES`]; a longer line is skipped and counted) and records
//! reach the database in batches of [`BATCH_RECORDS`] through a bounded
//! channel, all inside the file's one transaction.

use super::claude;
use super::codex;
use super::records::{LineStatus, Record, Vendor};
use super::store::{self, FileState, Fingerprint, Local};
use anyhow::{Context, Result};
use sqlx::SqlitePool;
use std::io::{BufRead, Read, Seek};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Bytes hashed at each end of the part of a file already read.
pub const FINGERPRINT_BYTES: u64 = 64 * 1024;
/// Longest line the reader buffers. Longer lines are skipped and counted.
pub const MAX_LINE_BYTES: usize = 32 << 20;
/// Records per batch sent from the parser thread to the writer.
pub const BATCH_RECORDS: usize = 2000;
/// Deepest directory level searched under the Codex root.
const MAX_DEPTH: usize = 12;

/// One source file to consider.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    /// The discovery root `path` lies under. The file is opened through a
    /// descriptor of this directory, never by its full path (see
    /// [`open_source`]).
    pub root: PathBuf,
    /// Identity (device, inode) of the regular file discovery saw at
    /// `path`. The opened descriptor must refer to the same file.
    pub dev: i64,
    pub ino: i64,
    pub vendor: Vendor,
    /// Claude only: the file's attribution.
    pub claude_ctx: Option<claude::FileCtx>,
    /// Claude main transcript (vs subagent).
    pub main: bool,
    pub agent_type: Option<String>,
}

fn is_jsonl(p: &Path) -> bool {
    p.extension().and_then(|x| x.to_str()) == Some("jsonl")
}

/// `(dev, ino)` of a directory entry that is a regular file, without
/// following a symbolic link.
fn regular_file_id(e: &std::fs::DirEntry) -> Option<(i64, i64)> {
    let m = e.metadata().ok()?;
    m.file_type().is_file().then(|| (m.dev() as i64, m.ino() as i64))
}

/// Open `rel` below the directory `root` without following a symbolic link
/// at any component below `root`: each directory is opened relative to its
/// parent's descriptor with `O_DIRECTORY | O_NOFOLLOW`, and the last
/// component with `O_NOFOLLOW | O_NONBLOCK` (a FIFO swapped in cannot block
/// the open). `root` itself is configuration and may be a link. `rel` must
/// consist of plain names only.
pub fn open_beneath(root: &Path, rel: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;
    let names: Vec<&std::ffi::OsStr> = rel
        .components()
        .map(|c| match c {
            Component::Normal(n) => Ok(n),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "path component is not a plain name")),
        })
        .collect::<std::io::Result<_>>()?;
    if names.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty relative path"));
    }
    let mut dir = std::fs::File::open(root)?;
    for (i, name) in names.iter().enumerate() {
        let last = i + 1 == names.len();
        let c = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last { libc::O_NONBLOCK } else { libc::O_DIRECTORY };
        // SAFETY: `dir` is an open descriptor for the duration of the call,
        // `c` is a NUL-terminated string, and a non-negative result is a
        // fresh descriptor that nothing else owns.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let f = unsafe { std::fs::File::from_raw_fd(fd) };
        if last {
            return Ok(f);
        }
        dir = f;
    }
    unreachable!("names is not empty")
}

/// Open a discovered source for reading: through [`open_beneath`] from its
/// root, then `fstat` the descriptor and require a regular file with the
/// identity discovery recorded. Every later read uses this descriptor, so
/// a path swapped for a link, another file, or a directory between
/// discovery and reading is refused instead of followed.
pub fn open_source(src: &Source) -> std::io::Result<(std::fs::File, FileMeta)> {
    let rel = src
        .path
        .strip_prefix(&src.root)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "source outside its root"))?;
    let f = open_beneath(&src.root, rel)?;
    let m = f.metadata()?;
    if !m.file_type().is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not a regular file"));
    }
    let meta = FileMeta::of(&m);
    if (meta.dev, meta.ino) != (src.dev, src.ino) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "file replaced since discovery"));
    }
    Ok((f, meta))
}

/// Largest subagent `.meta.json` read.
const MAX_META_BYTES: u64 = 1 << 20;

/// `agentType` from a subagent's `agent-<id>.meta.json`, opened the same
/// way as a transcript.
fn subagent_type(root: &Path, meta_path: &Path) -> Option<String> {
    let rel = meta_path.strip_prefix(root).ok()?;
    let f = open_beneath(root, rel).ok()?;
    if !f.metadata().ok()?.file_type().is_file() {
        return None;
    }
    let mut text = String::new();
    f.take(MAX_META_BYTES).read_to_string(&mut text).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("agentType")
        .and_then(|a| a.as_str())
        .map(String::from)
}

/// Claude sources: `<root>/<project>/<session>.jsonl` and
/// `<root>/<project>/<session>/subagents/agent-<id>.jsonl`. Symbolic links
/// are never followed below the root, so discovery cannot loop or leave the
/// root.
pub fn claude_sources(root: &Path) -> Vec<Source> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return out;
    };
    for proj in projects.flatten() {
        if !proj.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(proj.path()) else { continue };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                let Some(parent_sid) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                    continue;
                };
                let subdir = p.join("subagents");
                if !std::fs::symlink_metadata(&subdir).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                let Ok(subs) = std::fs::read_dir(&subdir) else { continue };
                for s in subs.flatten() {
                    let sp = s.path();
                    if !is_jsonl(&sp) {
                        continue;
                    }
                    let Some((dev, ino)) = regular_file_id(&s) else { continue };
                    let stem = sp.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let id = stem.strip_prefix("agent-").unwrap_or(&stem).to_string();
                    let agent_type = subagent_type(root, &sp.with_extension("meta.json"));
                    out.push(Source {
                        path: sp,
                        root: root.to_path_buf(),
                        dev,
                        ino,
                        vendor: Vendor::Claude,
                        claude_ctx: Some(claude::FileCtx { session_id: parent_sid.clone(), subagent_id: Some(id) }),
                        main: false,
                        agent_type,
                    });
                }
            } else if ft.is_file() && is_jsonl(&p) {
                let Some((dev, ino)) = regular_file_id(&e) else { continue };
                let sid = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                out.push(Source {
                    path: p,
                    root: root.to_path_buf(),
                    dev,
                    ino,
                    vendor: Vendor::Claude,
                    claude_ctx: Some(claude::FileCtx { session_id: sid, subagent_id: None }),
                    main: true,
                    agent_type: None,
                });
            }
        }
    }
    out
}

/// Codex sources: every `*.jsonl` below the root (`YYYY/MM/DD/`). Symbolic
/// links are skipped, so a link cycle or a link out of the root is never
/// walked; depth is capped at [`MAX_DEPTH`].
pub fn codex_sources(root: &Path, out: &mut Vec<Source>) {
    walk_codex(root, root, 0, out);
}

fn walk_codex(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Source>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk_codex(root, &p, depth + 1, out);
        } else if ft.is_file() && is_jsonl(&p) {
            let Some((dev, ino)) = regular_file_id(&e) else { continue };
            out.push(Source {
                path: p,
                root: root.to_path_buf(),
                dev,
                ino,
                vendor: Vendor::Codex,
                claude_ctx: None,
                main: true,
                agent_type: None,
            });
        }
    }
}

/// FNV-1a, 64-bit. Stable across builds and platforms (the stored value
/// must compare equal after a toolchain upgrade); change detection only.
fn fnv1a(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Fingerprint of `[0, offset)`: its head and the window ending at `offset`.
pub fn fingerprint(f: &std::fs::File, offset: u64) -> std::io::Result<Fingerprint> {
    let head_len = offset.min(FINGERPRINT_BYTES);
    let mut head = vec![0u8; head_len as usize];
    f.read_exact_at(&mut head, 0)?;
    let tail_len = offset.min(FINGERPRINT_BYTES);
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact_at(&mut tail, offset - tail_len)?;
    Ok(Fingerprint { head_len: head_len as i64, head_hash: fnv1a(&head), tail_hash: fnv1a(&tail) })
}

/// Result of reading complete lines from a start offset.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadResult {
    /// Offset just after the last complete line.
    pub offset: u64,
    pub oversized: i64,
}

/// Read complete lines in `[start, end)`, handing each to `each`. A
/// trailing line without its newline is left for the next refresh, as is
/// everything at or past `end`. A line longer than `max_line` is not
/// buffered: its bytes are discarded up to its newline and it is counted in
/// `oversized`. `each` returns `false` to stop early.
pub fn read_lines<R: Read + Seek>(
    reader: R,
    start: u64,
    end: u64,
    max_line: usize,
    mut each: impl FnMut(&[u8]) -> bool,
) -> std::io::Result<ReadResult> {
    let mut r = std::io::BufReader::with_capacity(1 << 20, reader);
    r.seek(std::io::SeekFrom::Start(start))?;
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut pos = start;
    let mut committed = start;
    let mut discarding = false;
    let mut oversized = 0i64;
    loop {
        let avail = r.fill_buf()?;
        let avail = &avail[..avail.len().min(end.saturating_sub(pos) as usize)];
        if avail.is_empty() {
            break;
        }
        match memchr::memchr(b'\n', avail) {
            Some(i) => {
                let mut keep_going = true;
                if discarding {
                    oversized += 1;
                } else if buf.len() + i > max_line {
                    oversized += 1;
                } else {
                    buf.extend_from_slice(&avail[..i]);
                    keep_going = each(&buf);
                }
                r.consume(i + 1);
                pos += (i + 1) as u64;
                committed = pos;
                discarding = false;
                buf.clear();
                if buf.capacity() > 4 << 20 {
                    buf.shrink_to(64 * 1024);
                }
                if !keep_going {
                    break;
                }
            }
            None => {
                let n = avail.len();
                if !discarding {
                    if buf.len() + n > max_line {
                        discarding = true;
                        buf = Vec::with_capacity(64 * 1024);
                    } else {
                        buf.extend_from_slice(avail);
                    }
                }
                r.consume(n);
                pos += n as u64;
            }
        }
    }
    Ok(ReadResult { offset: committed, oversized })
}

/// What the parser thread sends to the writer.
enum Msg {
    /// The file will be read; `replace` = delete its observations first.
    Begin { replace: bool },
    Batch(Vec<Record>),
}

/// Final state of a file that was read.
struct Outcome {
    meta: FileMeta,
    start: u64,
    offset: u64,
    carry: String,
    fingerprint: Fingerprint,
    first_ts_ms: Option<i64>,
    session_id: Option<String>,
    subagent_id: Option<String>,
    malformed: i64,
    oversized: i64,
    /// Malformed/oversized lines counted before `start` (append plan).
    prior_malformed: i64,
    prior_oversized: i64,
}

/// The metadata a refresh compares with the stored state: identity
/// (device, inode), size, and nanosecond modification and status-change
/// times. `ctime` cannot be set by a program: any write or `utimes` call
/// moves it, so a rewrite that restores the size and the mtime still
/// changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub dev: i64,
    pub ino: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl FileMeta {
    pub fn of(m: &std::fs::Metadata) -> Self {
        let ns = |secs: i64, nsec: i64| secs.saturating_mul(1_000_000_000).saturating_add(nsec);
        FileMeta {
            dev: m.dev() as i64,
            ino: m.ino() as i64,
            size: m.len() as i64,
            mtime_ns: ns(m.mtime(), m.mtime_nsec()),
            ctime_ns: ns(m.ctime(), m.ctime_nsec()),
        }
    }
}

/// Whether a read that started with metadata `before` and ended with
/// `after` saw consistent content: nothing changed, or the file only grew
/// (an append by the writer; the read stopped at `before.size`). Any other
/// change is an in-place write during the read.
pub fn consistent_read(before: &FileMeta, after: &FileMeta) -> bool {
    before == after || (before.dev, before.ino) == (after.dev, after.ino) && after.size > before.size
}

/// Whether the stored state proves the file is unchanged from metadata
/// alone (no read): device, inode, size, mtime and ctime all equal, to the
/// nanosecond. Anything else re-verifies the content fingerprint.
pub fn unchanged(prev: &FileState, m: &FileMeta) -> bool {
    prev.dev == m.dev
        && prev.ino == m.ino
        && prev.size == m.size
        && prev.mtime_ns == m.mtime_ns
        && prev.ctime_ns == m.ctime_ns
}

/// Parser-thread body: plan, read, and stream records. Returns `None` when
/// the file turned out to need nothing (it never sent `Begin`).
fn parse_file(
    src: Source,
    f: std::fs::File,
    meta: FileMeta,
    prev: Option<FileState>,
    full: bool,
    send: tokio::sync::mpsc::Sender<Msg>,
) -> std::io::Result<Option<Outcome>> {

    let append_from = match &prev {
        Some(p) if !full && p.dev == meta.dev && p.ino == meta.ino => {
            if unchanged(p, &meta) {
                return Ok(None);
            }
            // Only growth continues from the stored offset. A file that did
            // not grow but whose times moved was written in place; the
            // fingerprint cannot see an edit between its windows, so the
            // whole file is re-read.
            let grown = meta.size > p.size.max(p.offset);
            (grown && fingerprint(&f, p.offset as u64)? == p.fingerprint).then_some(p)
        }
        _ => None,
    };
    let (start, carry, prior_malformed, prior_oversized) = match append_from {
        Some(p) => (p.offset as u64, p.carry.clone(), p.malformed_lines, p.oversized_lines),
        None => (0, String::new(), 0, 0),
    };
    let closed = || std::io::Error::other("usage writer stopped");
    send.blocking_send(Msg::Begin { replace: start == 0 }).map_err(|_| closed())?;

    let mut records: Vec<Record> = Vec::new();
    let mut malformed = 0i64;
    let mut send_failed = false;
    let flush = |records: &mut Vec<Record>, force: bool| -> bool {
        if records.is_empty() || (!force && records.len() < BATCH_RECORDS) {
            return true;
        }
        let batch = claude::dedupe_batch(std::mem::take(records));
        send.blocking_send(Msg::Batch(batch)).is_ok()
    };

    let (read, carry_out, first_ts, session_id, subagent_id) = match src.vendor {
        Vendor::Claude => {
            let ctx = src.claude_ctx.clone().expect("claude source has a ctx");
            let mut c: claude::Carry = serde_json::from_str(&carry).unwrap_or_default();
            let read = read_lines(&f, start, meta.size as u64, MAX_LINE_BYTES, |line| {
                if claude::parse_line(line, &ctx, &mut c, &mut records) == LineStatus::Malformed {
                    malformed += 1;
                }
                if !flush(&mut records, false) {
                    send_failed = true;
                    return false;
                }
                true
            })?;
            let first = c.first_ts_ms;
            (read, serde_json::to_string(&c).unwrap_or_default(), first, Some(ctx.session_id), ctx.subagent_id)
        }
        Vendor::Codex => {
            let mut c: codex::Carry = serde_json::from_str(&carry).unwrap_or_default();
            let read = read_lines(&f, start, meta.size as u64, MAX_LINE_BYTES, |line| {
                if codex::parse_line(line, &mut c, &mut records) == LineStatus::Malformed {
                    malformed += 1;
                }
                if !flush(&mut records, false) {
                    send_failed = true;
                    return false;
                }
                true
            })?;
            let sub = (c.thread_id != c.session_id).then(|| c.thread_id.clone()).flatten();
            let first = c.first_ts_ms;
            let sid = c.session_id.clone();
            (read, serde_json::to_string(&c).unwrap_or_default(), first, sid, sub)
        }
    };
    if send_failed || !flush(&mut records, true) {
        return Err(closed());
    }
    let fingerprint = fingerprint(&f, read.offset)?;
    if !consistent_read(&meta, &FileMeta::of(&f.metadata()?)) {
        // The batch is discarded: the writer rolls the transaction back and
        // the stored state keeps the old metadata, so the next refresh
        // plans this file again.
        return Err(std::io::Error::other("changed in place while being read; read again on the next refresh"));
    }
    Ok(Some(Outcome {
        meta,
        start,
        offset: read.offset,
        carry: carry_out,
        fingerprint,
        first_ts_ms: first_ts,
        session_id,
        subagent_id,
        malformed,
        oversized: read.oversized,
        prior_malformed,
        prior_oversized,
    }))
}

/// What happened to one file.
#[derive(Debug, Default, Clone)]
pub struct FileReport {
    pub read: bool,
    pub bytes: u64,
    pub records: usize,
    pub malformed: i64,
    pub oversized: i64,
}

/// Read one file and commit its records and state in one transaction.
/// `Err` of the outer result = database failure (fatal for the refresh);
/// `Ok(Err)` = the file could not be read (the refresh continues and
/// reports it).
pub async fn ingest_file(
    pool: &SqlitePool,
    local: Local,
    src: &Source,
    full: bool,
) -> Result<std::io::Result<FileReport>> {
    let path_str = src.path.to_string_lossy().into_owned();
    let prev = store::load_file_state(pool, &path_str).await?;
    let (f, meta) = match open_source(src) {
        Ok(o) => o,
        Err(e) => return Ok(Err(e)),
    };
    if !full && prev.as_ref().is_some_and(|p| unchanged(p, &meta)) {
        return Ok(Ok(FileReport::default()));
    }

    let (send, mut recv) = tokio::sync::mpsc::channel::<Msg>(2);
    let task_src = src.clone();
    let parser = tokio::task::spawn_blocking(move || parse_file(task_src, f, meta, prev, full, send));

    let mut tx: Option<sqlx::Transaction<'_, sqlx::Sqlite>> = None;
    let mut records = 0usize;
    while let Some(msg) = recv.recv().await {
        match msg {
            Msg::Begin { replace } => {
                let mut t = pool.begin().await?;
                if replace {
                    store::forget_source(&mut t, &path_str).await?;
                }
                tx = Some(t);
            }
            Msg::Batch(batch) => {
                let t = tx.as_mut().context("batch before begin")?;
                records += store::write_records(t, local, &path_str, src.vendor, &batch).await?;
            }
        }
    }
    let outcome = match parser.await.context("parser task")? {
        Ok(Some(o)) => o,
        Ok(None) => return Ok(Ok(FileReport::default())),
        // The transaction (if any) rolls back on drop.
        Err(e) => return Ok(Err(e)),
    };
    let mut tx = tx.context("file read without a transaction")?;
    let state = FileState {
        path: path_str.clone(),
        vendor: src.vendor,
        session_id: outcome.session_id.clone(),
        subagent_id: outcome.subagent_id.clone(),
        dev: outcome.meta.dev,
        ino: outcome.meta.ino,
        size: outcome.meta.size,
        mtime_ns: outcome.meta.mtime_ns,
        ctime_ns: outcome.meta.ctime_ns,
        offset: outcome.offset as i64,
        carry: outcome.carry,
        fingerprint: outcome.fingerprint,
        first_ts_ms: outcome.first_ts_ms,
        malformed_lines: outcome.prior_malformed + outcome.malformed,
        oversized_lines: outcome.prior_oversized + outcome.oversized,
    };
    let ensure = outcome.session_id.as_deref();
    let facts = store::FileFacts {
        transcript_path: (src.main && src.vendor == Vendor::Claude
            || src.vendor == Vendor::Codex && outcome.subagent_id.is_none())
        .then_some(path_str.as_str()),
        ensure_session: ensure,
        subagent: match (&outcome.subagent_id, ensure) {
            (Some(sub), Some(parent)) => Some((sub.as_str(), parent, src.agent_type.as_deref())),
            _ => None,
        },
    };
    store::finish_file(&mut tx, &state, &facts).await?;
    tx.commit().await?;
    Ok(Ok(FileReport {
        read: true,
        bytes: outcome.offset.saturating_sub(outcome.start),
        records,
        malformed: outcome.malformed,
        oversized: outcome.oversized,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn lines_of(data: &[u8], start: u64, max: usize) -> (Vec<String>, ReadResult) {
        let mut out = Vec::new();
        let r = read_lines(Cursor::new(data.to_vec()), start, u64::MAX, max, |l| {
            out.push(String::from_utf8_lossy(l).into_owned());
            true
        })
        .unwrap();
        (out, r)
    }

    #[test]
    fn partial_last_line_is_left_for_later() {
        let (l, r) = lines_of(b"a\nbb\nccc", 0, 100);
        assert_eq!(l, vec!["a", "bb"]);
        assert_eq!(r.offset, 5);
    }

    #[test]
    fn oversized_lines_are_skipped_and_counted_without_buffering() {
        let mut data = b"short\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', 5000));
        data.extend(b"\nafter\n");
        // The reader's internal buffer is 1 MiB; a 5000-byte line against a
        // 100-byte limit exercises both the in-buffer and discard paths.
        let (l, r) = lines_of(&data, 0, 100);
        assert_eq!(l, vec!["short", "after"]);
        assert_eq!(r.oversized, 1);
        assert_eq!(r.offset, data.len() as u64);
    }

    #[test]
    fn oversized_line_spanning_reads_is_discarded() {
        // Larger than the 1 MiB BufReader capacity: arrives in several reads.
        let mut data = vec![b'y'; (1 << 20) * 2 + 17];
        data.push(b'\n');
        data.extend(b"ok\n");
        let (l, r) = lines_of(&data, 0, 1024);
        assert_eq!(l, vec!["ok"]);
        assert_eq!(r.oversized, 1);
    }

    #[test]
    fn a_read_stops_at_the_size_seen_before_it() {
        let data = b"a\nbb\nccc\n";
        let mut out = Vec::new();
        let r = read_lines(Cursor::new(data.to_vec()), 0, 6, 100, |l| {
            out.push(String::from_utf8_lossy(l).into_owned());
            true
        })
        .unwrap();
        assert_eq!(out, vec!["a", "bb"], "bytes past the end are left for the next refresh");
        assert_eq!(r.offset, 5);
    }

    #[test]
    fn a_read_is_consistent_only_when_unchanged_or_grown() {
        let before = FileMeta { dev: 1, ino: 2, size: 1000, mtime_ns: 10, ctime_ns: 10 };
        assert!(consistent_read(&before, &before));
        let grown = FileMeta { size: 1200, mtime_ns: 11, ctime_ns: 11, ..before };
        assert!(consistent_read(&before, &grown), "an append during the read is kept");
        let rewritten = FileMeta { mtime_ns: 11, ctime_ns: 11, ..before };
        assert!(!consistent_read(&before, &rewritten), "same size, times moved: in-place write");
        let ctime_only = FileMeta { ctime_ns: 11, ..before };
        assert!(!consistent_read(&before, &ctime_only));
        let shrunk = FileMeta { size: 900, mtime_ns: 11, ctime_ns: 11, ..before };
        assert!(!consistent_read(&before, &shrunk));
        let replaced = FileMeta { ino: 3, size: 1200, ..before };
        assert!(!consistent_read(&before, &replaced));
    }

    #[test]
    fn fingerprint_detects_changes_in_either_window() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, vec![b'a'; 200_000]).unwrap();
        let f = std::fs::File::open(&p).unwrap();
        let before = fingerprint(&f, 200_000).unwrap();
        drop(f);
        let mut data = vec![b'a'; 200_000];
        data[199_000] = b'b';
        std::fs::write(&p, &data).unwrap();
        let f = std::fs::File::open(&p).unwrap();
        assert_ne!(fingerprint(&f, 200_000).unwrap(), before, "tail change");
        data[199_000] = b'a';
        data[10] = b'b';
        std::fs::write(&p, &data).unwrap();
        let f = std::fs::File::open(&p).unwrap();
        assert_ne!(fingerprint(&f, 200_000).unwrap(), before, "head change");
    }

    #[test]
    fn discovery_skips_symlinks_and_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        let day = root.join("2026/09/01");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-a.jsonl"), "{}\n").unwrap();
        // A cycle back to the root and a link to a file outside it.
        std::os::unix::fs::symlink(&root, day.join("loop")).unwrap();
        let outside = dir.path().join("outside.jsonl");
        std::fs::write(&outside, "{}\n").unwrap();
        std::os::unix::fs::symlink(&outside, day.join("escaped.jsonl")).unwrap();
        let mut out = Vec::new();
        codex_sources(&root, &mut out);
        let names: Vec<_> = out.iter().map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["rollout-a.jsonl"]);

        // Claude: a symlinked project dir and a symlinked transcript are skipped.
        let croot = dir.path().join("claude");
        let proj = croot.join("-work-repo");
        std::fs::create_dir_all(proj.join("s1/subagents")).unwrap();
        std::fs::write(proj.join("s1.jsonl"), "{}\n").unwrap();
        std::fs::write(proj.join("s1/subagents/agent-a.jsonl"), "{}\n").unwrap();
        std::os::unix::fs::symlink(&outside, proj.join("s2.jsonl")).unwrap();
        std::os::unix::fs::symlink(&croot, croot.join("-loop")).unwrap();
        let mut got: Vec<_> = claude_sources(&croot).into_iter().map(|s| s.path).collect();
        got.sort();
        let mut want = vec![proj.join("s1.jsonl"), proj.join("s1/subagents/agent-a.jsonl")];
        want.sort();
        assert_eq!(got, want);
    }
}
