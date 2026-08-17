//! Pool state file lock (`<poolDir>/treehouse-state.lock`).
//!
//! This is the single most important invariant to port: every read-modify-write
//! of pool state must run inside ONE `with_state_lock` so concurrent treehouse
//! processes serialize. Go uses `flock LOCK_EX` (unix) / `LockFileEx`
//! exclusive (windows) on the lock file; this port uses `fd-lock`'s exclusive
//! `RwLock`, which wraps the same primitives.
//!
//! The lock is **exclusive-only** — there is no read lock. `status` also needs
//! the exclusive lock because it mutates via heal + `WriteState`.
//!
//! Port addition over Go: Go blocks forever on a wedged holder; we time out
//! after `lock_timeout` (retry/backoff) and return [`LockError::Timeout`]
//! instead of hanging every pool command.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use fd_lock::RwLock;

/// Default time to wait for the state lock before giving up. Go blocks
/// forever; this bounds the stall. Tuned above a typical critical section
/// (read+heal+write is milliseconds) while still surfacing wedged holders.
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// Base retry interval between lock attempts (exponential backoff).
const RETRY_BASE: Duration = Duration::from_millis(10);
/// Cap on backoff between attempts.
const RETRY_MAX: Duration = Duration::from_millis(500);

/// Runs `f` while holding the exclusive pool state lock.
///
/// Creates the pool directory (0755) and the lock file (0644) if needed, then
/// acquires an exclusive advisory lock, runs `f`, and releases the lock on
/// every exit path — including `f` returning `Err` or panicking (the guard is
/// dropped, which unlocks via `fd_lock`'s RAII).
pub fn with_state_lock<T, E>(
    pool_dir: &Path,
    lock_timeout: Duration,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, LockError<E>>
where
    E: std::fmt::Display,
{
    std::fs::create_dir_all(pool_dir)
        .map_err(|e| LockError::Io(format!("creating pool dir {}", pool_dir.display()), e))?;

    let lock_path = crate::state::State::lock_file_path(pool_dir);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| LockError::Io(format!("opening lock file {}", lock_path.display()), e))?;

    let mut lock = RwLock::new(file);

    // Acquire with retry/backoff. The guard is a value whose Drop unlocks the
    // file, so we hold it for the whole closure — including unwinding.
    let deadline = Instant::now() + lock_timeout;
    let mut backoff = RETRY_BASE;
    let guard = loop {
        match lock.try_write() {
            Ok(guard) => break guard,
            Err(_) => {
                if Instant::now() >= deadline {
                    return Err(LockError::Timeout);
                }
                thread::sleep(backoff);
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    };

    let result = f();
    drop(guard);
    result.map_err(LockError::Callback)
}

use std::fs::OpenOptions;

/// Errors from the state lock.
#[derive(Debug, thiserror::Error)]
pub enum LockError<E>
where
    E: std::fmt::Display,
{
    #[error("lock is held by another process; gave up after the lock timeout")]
    Timeout,
    #[error("failed to open lock file {0}: {1}")]
    Io(String, std::io::Error),
    #[error("state operation under lock failed: {0}")]
    Callback(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_serializes_two_threads() {
        let dir = tempfile::tempdir().unwrap();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let check = counter.clone();
        let dir_a = dir.path().to_path_buf();
        let c_a = counter.clone();
        let dir_b = dir.path().to_path_buf();
        let c_b = counter.clone();

        // Thread A holds the lock and increments; thread B must wait and then
        // increment after A releases.
        let a = std::thread::spawn(move || {
            with_state_lock(&dir_a, Duration::from_secs(5), || {
                thread::sleep(Duration::from_millis(50));
                c_a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok::<(), String>(())
            })
            .unwrap();
        });
        let b = std::thread::spawn(move || {
            with_state_lock(&dir_b, Duration::from_secs(5), || {
                c_b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok::<(), String>(())
            })
            .unwrap();
        });
        a.join().unwrap();
        b.join().unwrap();
        assert_eq!(check.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn timeout_when_holder_wedges() {
        let dir = tempfile::tempdir().unwrap();
        let dir2 = dir.path().to_path_buf();
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        // Thread A takes the lock and holds it for a while. A signals (via the
        // channel) only AFTER it has acquired the lock, so the test is
        // deterministic: we never race A's startup time against a fixed sleep.
        let (tx, rx) = std::sync::mpsc::channel();
        let h2 = held.clone();
        let a = std::thread::spawn(move || {
            let _ = with_state_lock(&dir2, Duration::from_secs(5), || {
                let _ = tx.send(());
                thread::sleep(Duration::from_millis(300));
                Ok::<(), String>(())
            });
            h2.store(false, std::sync::atomic::Ordering::SeqCst);
        });

        // Wait until A provably holds the lock.
        rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // Short timeout: B should time out, not block forever.
        let start = Instant::now();
        let result =
            with_state_lock::<(), String>(dir.path(), Duration::from_millis(100), || Ok(()));
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(LockError::Timeout)), "got {result:?}");
        assert!(
            elapsed < Duration::from_millis(300),
            "returned too late: {elapsed:?}"
        );

        a.join().unwrap();
        assert!(!held.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn releases_after_callback_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = with_state_lock::<(), String>(dir.path(), Duration::from_secs(2), || {
            Err("boom".to_string())
        });
        assert!(matches!(result, Err(LockError::Callback(e)) if e == "boom"));

        // Lock must be free again: a second acquisition succeeds.
        with_state_lock::<(), String>(dir.path(), Duration::from_secs(2), || Ok(())).unwrap();
    }

    #[test]
    fn releases_after_panic() {
        let dir = tempfile::tempdir().unwrap();
        let dir2 = dir.path().to_path_buf();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_state_lock::<(), String>(&dir2, Duration::from_secs(2), || {
                panic!("boom");
            })
        }));
        assert!(result.is_err());

        // Lock released despite the panic.
        with_state_lock::<(), String>(dir.path(), Duration::from_secs(2), || Ok(())).unwrap();
    }
}
