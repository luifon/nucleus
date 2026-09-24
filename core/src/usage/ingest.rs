//! Reading transcript files into the store (ADR-034 §2).
//!
//! Per file, a refresh decides between three plans from the stored read
//! state and the file on disk:
//!
//! - **skip** — same identity (device, inode), size and mtime as stored;
//! - **append** — same identity, not shorter than the stored offset, and the
//!   content fingerprint of the part already read still matches: read from
//!   the stored offset with the stored parser carry;
//! - **re-read** — anything else (new file, `--full`, different identity,
//!   truncated below the offset, or a changed fingerprint = rewritten in
//!   place): delete every observation of the path and read from byte 0 with
//!   an empty carry, in the same transaction.
//!
//! The fingerprint covers the first [`FINGERPRINT_BYTES`] bytes and the
//! [`FINGERPRINT_BYTES`] bytes that end at the stored offset. An in-place
//! edit that leaves the size unchanged and touches neither window is not
//! detected; `nucleus usage refresh --full` re-reads everything.
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
                    if !s.file_type().is_ok_and(|t| t.is_file()) || !is_jsonl(&sp) {
                        continue;
                    }
                    let stem = sp.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let id = stem.strip_prefix("agent-").unwrap_or(&stem).to_string();
                    let agent_type = std::fs::read_to_string(sp.with_extension("meta.json"))
                        .ok()
                        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                        .and_then(|v| v.get("agentType").and_then(|a| a.as_str()).map(String::from));
                    out.push(Source {
                        path: sp,
                        vendor: Vendor::Claude,
                        claude_ctx: Some(claude::FileCtx { session_id: parent_sid.clone(), subagent_id: Some(id) }),
                        main: false,
                        agent_type,
                    });
                }
            } else if ft.is_file() && is_jsonl(&p) {
                let sid = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                out.push(Source {
                    path: p,
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
    walk_codex(root, 0, out);
}

fn walk_codex(dir: &Path, depth: usize, out: &mut Vec<Source>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk_codex(&p, depth + 1, out);
        } else if ft.is_file() && is_jsonl(&p) {
            out.push(Source { path: p, vendor: Vendor::Codex, claude_ctx: None, main: true, agent_type: None });
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

/// Read complete lines from `start`, handing each to `each`. A trailing line
/// without its newline is left for the next refresh. A line longer than
/// `max_line` is not buffered: its bytes are discarded up to its newline and
/// it is counted in `oversized`. `each` returns `false` to stop early.
pub fn read_lines<R: Read + Seek>(
    reader: R,
    start: u64,
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
    dev: i64,
    ino: i64,
    size: i64,
    mtime: i64,
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

fn mtime_secs(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether the stored state proves the file is unchanged, from metadata
/// alone (no read).
pub fn unchanged(prev: &FileState, meta: &std::fs::Metadata) -> bool {
    prev.dev == meta.dev() as i64
        && prev.ino == meta.ino() as i64
        && prev.size == meta.len() as i64
        && prev.mtime == mtime_secs(meta)
}

/// Parser-thread body: plan, read, and stream records. Returns `None` when
/// the file turned out to need nothing (it never sent `Begin`).
fn parse_file(
    src: Source,
    prev: Option<FileState>,
    full: bool,
    send: tokio::sync::mpsc::Sender<Msg>,
) -> std::io::Result<Option<Outcome>> {
    let f = std::fs::File::open(&src.path)?;
    let meta = f.metadata()?;
    let (dev, ino, size, mtime) = (meta.dev() as i64, meta.ino() as i64, meta.len() as i64, mtime_secs(&meta));

    let append_from = match &prev {
        Some(p) if !full && p.dev == dev && p.ino == ino && size >= p.offset => {
            if p.size == size && p.mtime == mtime {
                return Ok(None);
            }
            (fingerprint(&f, p.offset as u64)? == p.fingerprint).then_some(p)
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
            let read = read_lines(&f, start, MAX_LINE_BYTES, |line| {
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
            let read = read_lines(&f, start, MAX_LINE_BYTES, |line| {
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
    Ok(Some(Outcome {
        dev,
        ino,
        size,
        mtime,
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
    if !full {
        if let (Some(p), Ok(m)) = (&prev, std::fs::metadata(&src.path)) {
            if unchanged(p, &m) {
                return Ok(Ok(FileReport::default()));
            }
        }
    }

    let (send, mut recv) = tokio::sync::mpsc::channel::<Msg>(2);
    let task_src = src.clone();
    let parser = tokio::task::spawn_blocking(move || parse_file(task_src, prev, full, send));

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
        dev: outcome.dev,
        ino: outcome.ino,
        size: outcome.size,
        mtime: outcome.mtime,
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
        let r = read_lines(Cursor::new(data.to_vec()), start, max, |l| {
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
