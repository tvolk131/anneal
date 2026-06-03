//! Multi-process workspace lock (`docs/sandboxing.md` §5; multi-process hardening).
//!
//! The content-addressed stores (CAS, action cache, snapshot index) are already
//! multi-process-safe — atomic temp+rename over content-addressed data. What is *not*
//! safe across processes is the mutable working state that warm reuse made shared:
//! per-`snapshot_key` warm dirs and ephemeral sandboxes. Two concurrent `anneal`
//! processes building the same workspace would sync + build in the same warm dir and
//! collide on sandbox names.
//!
//! So a mutating command (`build`/`test`) takes a **coarse exclusive advisory lock** on
//! the `.anneal/` store for its whole run; a second process waits. Read-only commands
//! (`affected`/`why`) never acquire it — they read atomic/content-addressed state and are
//! safe to run alongside a build.
//!
//! The lock is `flock(LOCK_EX)` on `.anneal/lock`, held by keeping the file descriptor
//! open. `flock` is released on close **and on process death**, so a crash leaves no
//! stale lock (the reason to use it over a sentinel file). Advisory and Unix-only
//! (macOS + Linux; Windows is out of scope). Not reliable over NFS — fine for a local
//! `.anneal/`.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// An exclusive lock on a workspace's `.anneal/` store, held for the duration of a
/// mutating command. Released automatically when dropped (or when the process exits).
pub struct WorkspaceLock {
    // Held only to keep the file descriptor — and thus the `flock` — alive.
    _file: File,
}

impl WorkspaceLock {
    /// Acquire the exclusive lock on `<store_root>/lock`. Tries without blocking first; if
    /// another `anneal` process holds it, prints a one-line waiting message (with the
    /// holder PID, best-effort) and then blocks until the lock is free.
    pub fn acquire(store_root: &Path) -> io::Result<Self> {
        fs::create_dir_all(store_root)?;
        let path = store_root.join("lock");
        let file = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&path)?;
        let fd = file.as_raw_fd();

        // SAFETY: `fd` is a valid open descriptor owned by `file` for the call's duration.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err);
            }
            // Contended: report who holds it (best-effort) and block.
            match fs::read_to_string(&path).ok().filter(|s| !s.trim().is_empty()) {
                Some(holder) => eprintln!(
                    "Blocking: waiting for another anneal process (PID {}) on {}",
                    holder.trim(),
                    store_root.display()
                ),
                None => eprintln!(
                    "Blocking: waiting for another anneal process on {}",
                    store_root.display()
                ),
            }
            if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // We hold the lock — record our PID for the next waiter's diagnostics. Best-effort;
        // a separate open() doesn't disturb the flock held on `file`'s descriptor.
        let _ = fs::write(&path, format!("{}\n", std::process::id()));
        Ok(WorkspaceLock { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_lock_is_held_then_freed_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join(".anneal");
        let lock = WorkspaceLock::acquire(&store).unwrap();

        // A second descriptor to the same lock file: a non-blocking exclusive flock must
        // be denied while we hold the lock (flock treats independent descriptors as
        // independent, so this models another process).
        let other = OpenOptions::new().read(true).write(true).open(store.join("lock")).unwrap();
        let denied = unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_ne!(denied, 0, "a second exclusive lock must be denied while held");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EWOULDBLOCK));

        drop(lock);

        // Once the guard drops, the descriptor closes and the lock is free.
        let acquired = unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(acquired, 0, "the lock must be free after the guard drops");
    }

    #[test]
    fn records_holder_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join(".anneal");
        let _lock = WorkspaceLock::acquire(&store).unwrap();
        let recorded = fs::read_to_string(store.join("lock")).unwrap();
        assert_eq!(recorded.trim(), std::process::id().to_string());
    }
}
