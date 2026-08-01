//! A whole-work-directory lock, so two runs cannot share one work directory.
//!
//! ferroday-cage locks each build root it provisions, but the pool, the output
//! tree, and the source checkouts a run shares are otherwise unprotected: two
//! concurrent runs against the same `--work` would interleave pool writes and
//! source checkouts. [`WorkLock`] takes one exclusive lock on the work directory
//! for the whole run, so a second run is cleanly rejected rather than corrupting
//! the first.
//!
//! The lock is a lockfile created with `O_CREAT | O_EXCL`, whose presence is the
//! lock. A [`WorkLock`] removes it on drop, so a run that finishes or errors
//! normally releases it — including a cancelled one, which unwinds through the
//! same path.
//!
//! A run that never gets to unwind leaves the file behind: `SIGKILL`, power loss,
//! and the second Ctrl-C, which exits the process immediately so that a graceful
//! stop that is itself stuck stays escapable. That last one is a deliberate
//! escape hatch rather than a crash, so a leftover lock is a normal thing to
//! meet. The rejection message names the file, and it records the process that
//! held it, so a stale lock can be told from a live one and removed by hand.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result, io_error};

/// The lockfile name within the work directory.
const LOCK_FILE: &str = ".lock";

/// An exclusive lock on a work directory, held for the duration of a run and
/// released when dropped.
#[derive(Debug)]
pub struct WorkLock {
    path: PathBuf,
}

impl WorkLock {
    /// Acquires the lock on `work_dir`, which must already exist.
    ///
    /// Returns [`Error::WorkLocked`] when the lockfile is already present —
    /// another run holds it, or a run that never unwound left it behind. The
    /// error names the process that took it, so the two can be told apart.
    pub fn acquire(work_dir: &Path) -> Result<WorkLock> {
        let path = work_dir.join(LOCK_FILE);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                // Record the holding process so a stale lock can be traced; a
                // write failure here is immaterial to holding the lock.
                let _ = writeln!(file, "{}", std::process::id());
                Ok(WorkLock { path })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = holder(&path);
                Err(Error::WorkLocked { path, holder })
            }
            Err(err) => Err(io_error("locking the work directory", &path, err)),
        }
    }
}

/// The process id a lockfile records, or `None` when it holds nothing readable
/// as one.
///
/// Only ever used to make a rejection more useful, so every failure to read it —
/// an unreadable file, a truncated write from a run killed mid-`writeln`, a
/// lockfile from some older shape of this — is the same "cannot say".
fn holder(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

impl Drop for WorkLock {
    fn drop(&mut self) {
        // Best-effort release: the run is over, and a failure to remove the
        // lockfile only leaves a stale lock the next run's message explains.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn work_dir() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("src2deb-lock-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_second_acquire_is_rejected_while_the_first_is_held() {
        let dir = work_dir();
        let first = WorkLock::acquire(&dir).expect("first lock");
        let err = WorkLock::acquire(&dir).unwrap_err();
        // The rejection names the lockfile and the process holding it, so a
        // lock left behind by a run that never unwound can be told from a live
        // one without guessing.
        let this_process = std::process::id();
        assert!(matches!(err, Error::WorkLocked { holder, .. } if holder == Some(this_process)));
        let message = format!("{err}");
        assert!(message.contains(".lock"), "{message}");
        assert!(message.contains(&this_process.to_string()), "{message}");
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lockfile_with_no_readable_process_id_still_rejects() {
        // A run killed mid-write, or a lockfile from some older shape of this,
        // leaves a file that holds no process id. Its presence is the lock
        // regardless; the rejection just has less to say.
        let dir = work_dir();
        std::fs::write(dir.join(LOCK_FILE), "not a pid").unwrap();
        let err = WorkLock::acquire(&dir).unwrap_err();
        assert!(matches!(err, Error::WorkLocked { holder: None, .. }));
        assert!(format!("{err}").contains("another run"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lock_is_released_on_drop_so_a_later_run_may_acquire() {
        let dir = work_dir();
        drop(WorkLock::acquire(&dir).expect("first lock"));
        // The lockfile is gone, so a fresh acquire succeeds.
        WorkLock::acquire(&dir).expect("re-acquire after release");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
