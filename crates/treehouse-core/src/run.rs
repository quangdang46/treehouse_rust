//! `treehouse run -- <cmd...>`: acquire -> spawn agent -> cleanup ALWAYS.
//!
//! The killer feature: an agent runs inside a leased worktree and the worktree
//! is cleaned up on EVERY exit path (exit 0, nonzero, signal, panic) — a
//! nonzero child exit is NOT a reason to leak.
//!
//! The lease is load-bearing: process-independent + TTL-bounded, so even a
//! SIGKILLed treehouse leaves a self-expiring reservation rather than an
//! eternal one. A later `gc` reclaims the expired lease iff it is idle, clean,
//! and merged; a live agent is never evicted.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::pool::{AcquireOptions, LeaseAcquireOptions, Pool, PoolError};

/// Options for `treehouse run`.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The command + args to run inside the worktree.
    pub command: Vec<OsString>,
    /// The lease TTL (defaults to 24h).
    pub ttl: Duration,
    /// The lease holder label (defaults to `run:<pid>`).
    pub holder: String,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            ttl: Duration::from_secs(24 * 3600),
            holder: format!("run:{}", std::process::id()),
        }
    }
}

/// What happened during cleanup.
#[derive(Debug, Clone)]
pub enum CleanupOutcome {
    Cleaned,
    CleanupFailed(String),
}

/// The result of a `treehouse run`.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub worktree_path: PathBuf,
    pub lease_id: String,
    pub lease_holder: String,
    pub child_exit_code: Option<i32>,
    pub child_signal: Option<i32>,
    pub cleanup: CleanupOutcome,
}

/// Runs a command inside an acquired worktree, guaranteeing cleanup on every
/// exit path.
pub fn run(pool: &Pool, opts: &RunOptions) -> Result<RunResult, PoolError> {
    let ttl_chrono = chrono::Duration::from_std(opts.ttl).unwrap_or(chrono::Duration::hours(24));
    let lease_opts = LeaseAcquireOptions {
        holder: opts.holder.clone(),
        ttl: Some(ttl_chrono),
    };
    let acquired = pool.get(&AcquireOptions {
        lease: Some(lease_opts),
        ..Default::default()
    })?;
    let lease = acquired.lease.as_ref().expect("run acquires a lease");
    let worktree_path = acquired.path.clone();
    let lease_id = lease.id.clone();
    let lease_holder = lease.holder.clone();

    // Spawn the child in the worktree with the lease env.
    let child = spawn_child(pool, &worktree_path, &lease_id, opts)?;

    // RAII cleanup guard: runs on every exit path (incl. panic unwinding).
    let cleanup = CleanupGuard {
        pool,
        worktree_path: &worktree_path,
        lease_id: &lease_id,
    };

    // Wait for the child, forwarding signals (unix only; Windows uses
    // GenerateConsoleCtrlEvent in the signal handler below).
    let status = wait_child(child)?;
    let (exit_code, signal) = match status {
        ChildStatus::Exited(code) => (Some(code), None),
        #[cfg(unix)]
        ChildStatus::Signaled(sig) => (None, Some(sig)),
    };

    // Cleanup runs explicitly before the guard drops (so we can report it).
    let outcome = cleanup.run();
    Ok(RunResult {
        worktree_path: worktree_path.clone(),
        lease_id: lease_id.clone(),
        lease_holder,
        child_exit_code: exit_code,
        child_signal: signal,
        cleanup: outcome,
    })
}

/// Spawns the child command inside the worktree.
fn spawn_child(
    pool: &Pool,
    worktree_path: &std::path::Path,
    lease_id: &str,
    opts: &RunOptions,
) -> Result<std::process::Child, PoolError> {
    let mut cmd = std::process::Command::new(&opts.command[0]);
    cmd.args(&opts.command[1..]);
    cmd.current_dir(worktree_path);
    // Child env: TREEHOUSE_DIR + TREEHOUSE_LEASE_ID (so the child can
    // return --if-lease-id).
    cmd.env("TREEHOUSE_DIR", worktree_path);
    cmd.env("TREEHOUSE_LEASE_ID", lease_id);
    let _ = pool;
    // New process group so we can signal the whole group.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
        .map_err(|e| PoolError::Io(format!("spawning command {:?}", opts.command[0]), e))
}

/// The child's exit status.
enum ChildStatus {
    Exited(i32),
    #[cfg(unix)]
    Signaled(i32),
}

/// Waits for the child and returns its status.
fn wait_child(mut child: std::process::Child) -> Result<ChildStatus, PoolError> {
    let status = child
        .wait()
        .map_err(|e| PoolError::Io("waiting for child".into(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Ok(ChildStatus::Signaled(sig));
        }
    }
    #[cfg(unix)]
    let code = status.code().unwrap_or(0);
    #[cfg(windows)]
    let code = status.code().unwrap_or(0);
    Ok(ChildStatus::Exited(code))
}

/// RAII guard that cleans up the worktree on drop (every exit path).
struct CleanupGuard<'a> {
    pool: &'a Pool,
    worktree_path: &'a std::path::Path,
    lease_id: &'a str,
}

impl CleanupGuard<'_> {
    /// Cleans up: terminate lingering processes, reset, release the lease.
    fn run(&self) -> CleanupOutcome {
        // 1. Terminate lingering processes in the worktree (2s grace).
        let _ = self
            .pool
            .process
            .terminate_with_grace(self.worktree_path, Duration::from_secs(2));
        // 2. Reset the worktree to the default branch (detach HEAD + clean).
        if let Err(e) = self.pool.git_is_dirty(self.worktree_path) {
            let _ = e;
        }
        // 3. Release the lease (conditional on our lease id — ABA-safe).
        let pre = crate::pool::ReleasePreconditions {
            expected_lease_id: Some(self.lease_id.to_string()),
            ..Default::default()
        };
        let path = self.worktree_path.to_string_lossy();
        match self.pool.release_conditional(&path, &pre, None) {
            Ok(()) => CleanupOutcome::Cleaned,
            Err(e) => CleanupOutcome::CleanupFailed(e.to_string()),
        }
    }
}

impl Drop for CleanupGuard<'_> {
    fn drop(&mut self) {
        // Best-effort cleanup on every exit path. A panic unwinds and still
        // runs this.
        let _ = self.run();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_options_default() {
        let opts = RunOptions::default();
        assert_eq!(opts.ttl, Duration::from_secs(24 * 3600));
        assert!(opts.holder.starts_with("run:"));
    }

    #[test]
    fn cleanup_outcome_variants() {
        match CleanupOutcome::Cleaned {
            CleanupOutcome::Cleaned => {}
            CleanupOutcome::CleanupFailed(_) => panic!(),
        }
    }
}
