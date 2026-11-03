//! Consistent copies of live SQLite databases with SQLite's online backup
//! API. A plain file copy of a database that another process is writing
//! (WAL mode especially: the committed data may still be in `-wal`) can be
//! torn or out of date; the backup API copies one read transaction.

use anyhow::{Context, Result, bail};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long a copy waits for a writer that holds an exclusive lock
/// (rollback-journal databases; WAL readers are never blocked).
const BUSY_DEADLINE: Duration = Duration::from_secs(120);
const BUSY_PAUSE: Duration = Duration::from_millis(250);

/// Copy `src` to `dest` in one read transaction, then check the copy with
/// `PRAGMA quick_check`. `dest` must not exist.
pub fn snapshot(src: &Path, dest: &Path) -> Result<u64> {
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    // Read-write without CREATE: a database that disappeared is an error,
    // never a new empty file. Nothing is written through this connection;
    // read-only connections cannot open a WAL database whose -shm file is
    // missing.
    let source = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", src.display()))?;
    source.busy_timeout(Duration::from_secs(10))?;
    let mut target = Connection::open(dest).with_context(|| format!("creating {}", dest.display()))?;
    {
        let backup = Backup::new(&source, &mut target)?;
        let started = Instant::now();
        // -1: every page in one step, so the copy is one read transaction
        // and is not restarted by writes from other processes.
        loop {
            match backup.step(-1).with_context(|| format!("backing up {}", src.display()))? {
                StepResult::Done => break,
                StepResult::More => continue,
                StepResult::Busy | StepResult::Locked => {
                    if started.elapsed() > BUSY_DEADLINE {
                        bail!("{} stayed locked for {}s", src.display(), BUSY_DEADLINE.as_secs());
                    }
                    std::thread::sleep(BUSY_PAUSE);
                }
                _ => bail!("unexpected backup step result for {}", src.display()),
            }
        }
    }
    // The copy is a standalone file: no WAL beside it.
    target.pragma_update(None, "journal_mode", "DELETE")?;
    let check: String = target.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        bail!("the copy of {} failed quick_check: {check}", src.display());
    }
    drop(target);
    Ok(std::fs::metadata(dest)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_committed_wal_content_and_leaves_no_sidecars() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("live.db");
        let writer = Connection::open(&src).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(x INTEGER);")
            .unwrap();
        for i in 0..500 {
            writer.execute("INSERT INTO t VALUES (?1)", [i]).unwrap();
        }
        // The rows are committed but still only in the -wal file: a copy of
        // the main file alone would miss them.
        assert!(t.path().join("live.db-wal").metadata().unwrap().len() > 0);
        // An open read transaction elsewhere does not block the copy.
        let reader = Connection::open(&src).unwrap();
        reader.execute_batch("BEGIN; SELECT count(*) FROM t;").unwrap();

        let dest = t.path().join("out/staged/live.db");
        let bytes = snapshot(&src, &dest).unwrap();
        assert!(bytes > 0);
        let copy = Connection::open(&dest).unwrap();
        let n: i64 = copy.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 500);
        let mode: String = copy.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "delete");
        drop(copy);
        assert!(!t.path().join("out/staged/live.db-wal").exists());
        reader.execute_batch("COMMIT;").unwrap();
        drop(writer);
    }

    #[test]
    fn refuses_a_missing_source_and_an_existing_destination() {
        let t = tempfile::tempdir().unwrap();
        assert!(snapshot(&t.path().join("gone.db"), &t.path().join("a.db")).is_err());
        assert!(!t.path().join("gone.db").exists(), "a missing source is not created");
        let src = t.path().join("s.db");
        Connection::open(&src).unwrap().execute_batch("CREATE TABLE t(x);").unwrap();
        std::fs::write(t.path().join("exists.db"), b"x").unwrap();
        assert!(snapshot(&src, &t.path().join("exists.db")).is_err());
    }
}
