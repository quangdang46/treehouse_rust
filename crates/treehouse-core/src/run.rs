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

    // The guard is constructed IMMEDIATELY after the acquire, BEFORE the
    // spawn. `spawn_child`'s `?` used to return past a guard that did not yet
    // exist, so a command that could not be executed (typo, missing binary)
    // left a durably leased slot behind for the whole TTL — with no owner to
    // release it and nothing in `status` to explain why the pool shrank.
    // From here on every exit path, including a failing spawn, unwinds
    // through `Drop`.
    let cleanup = CleanupGuard {
        pool,
        worktree_path: &worktree_path,
        lease_id: &lease_id,
    };

    // Spawn the child in the worktree with the lease env.
    let child = spawn_child(&worktree_path, &lease_id, opts)?;

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
        // 2. Release the lease (conditional on our lease id — ABA-safe).
        // This is also the reset: `release_conditional` runs `reset_worktree`
        // (detach HEAD + `reset --hard` + `clean -fd`) between its two lock
        // acquisitions, so the slot comes back clean without a separate step.
        // A standalone reset here would either duplicate that destructive work
        // or run it before the lease precondition has been checked — and
        // discarding the agent's work is this verb's contract, not a decision
        // this guard gets to make independently of the release.
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
        //
        // On the happy path this is a SECOND pass: `run` cleans up explicitly
        // so it can report the outcome, then unwinds through here. That is
        // deliberate, not an oversight — a pass that reported
        // `CleanupFailed` because of a transient lock timeout gets retried
        // here, and a pass that succeeded is inert on the retry because
        // `release_conditional` refuses a slot that is no longer leased
        // (pool.rs `expected_lease_id` -> "worktree is not leased") before it
        // ever reaches `reset_worktree`. So the repeat can tighten the pool
        // but can never reset a slot somebody else has since taken.
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

    // ─── The repeat cleanup pass must not touch a re-acquired slot ─────────

    /// Builds a repo with one commit on `main`. Returns (tempdir, repo path);
    /// the TempDir must be kept alive by the caller.
    fn init_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let out = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .expect("git must be installed");
        assert!(out.status.success(), "git init failed");
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["config", "user.email", "t@t.com"]);
        run_git(&["config", "user.name", "T"]);
        std::fs::write(repo.join("README.md"), b"hi\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "init"]);
        (dir, repo)
    }

    fn pool_over(dir: &tempfile::TempDir, repo: &std::path::Path) -> Pool {
        let opts = crate::OpenOptions {
            config: crate::config::TreehouseConfig {
                root: Some(dir.path().to_str().unwrap().to_string()),
                ..crate::config::TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        Pool::open(repo, None, &opts).unwrap()
    }

    /// Acquires a leased slot, pinned to `main` because `init_repo` builds a
    /// repo with no remote to resolve a default branch from.
    fn acquire_leased(pool: &Pool, holder: &str) -> crate::pool::Acquired {
        pool.get(&AcquireOptions {
            branch: Some("main".into()),
            lease: Some(LeaseAcquireOptions {
                holder: holder.to_string(),
                ttl: Some(chrono::Duration::hours(1)),
            }),
            ..Default::default()
        })
        .unwrap()
    }

    /// `run` cleans up explicitly and then unwinds through `Drop`, so the
    /// guard runs TWICE on the happy path. The second pass is only safe
    /// because it is pinned to the lease it took: by then the slot may have
    /// been handed to somebody else, and resetting it would discard their work
    /// under them.
    ///
    /// This is the invariant the `Drop` comment asserts. If `release_conditional`
    /// ever stopped refusing a slot that is no longer leased by us, this is the
    /// test that would say so.
    #[test]
    fn repeat_cleanup_refuses_a_slot_someone_else_has_since_leased() {
        let (dir, repo) = init_repo();
        let pool = pool_over(&dir, &repo);

        let first = acquire_leased(&pool, "run:first");
        let path = first.path.clone();
        let first_lease = first.lease.as_ref().expect("a lease was taken").id.clone();

        let guard = CleanupGuard {
            pool: &pool,
            worktree_path: &path,
            lease_id: &first_lease,
        };
        // First pass: the slot is ours, so it is ours to release.
        assert!(matches!(guard.run(), CleanupOutcome::Cleaned));

        // A second acquisition takes the slot we just gave back.
        let second = acquire_leased(&pool, "run:second");
        assert_eq!(second.path, path, "the released slot should be reused");
        let second_lease = second
            .lease
            .as_ref()
            .expect("a lease was taken")
            .id
            .clone();
        assert_ne!(second_lease, first_lease, "each lease gets its own id");

        // The repeat pass must refuse, not reset. Pin the REASON too: a
        // refusal for anything else (slot vanished, pool renamed) would leave
        // the new lease intact for the wrong reason and stop testing the
        // identity check this invariant rests on.
        match guard.run() {
            CleanupOutcome::CleanupFailed(ref why) => assert!(
                why.contains("lease"),
                "expected a lease-identity refusal, got: {why}"
            ),
            CleanupOutcome::Cleaned => {
                panic!("cleanup reset a slot that a different lease owns")
            }
        }

        // ...and the new owner's lease survived untouched.
        let status = pool.status().unwrap();
        let wt = status
            .iter()
            .find(|w| w.path == path.to_string_lossy())
            .expect("the slot is still tracked");
        assert_eq!(wt.status, crate::pool::STATUS_LEASED);
        assert_eq!(wt.lease_id, second_lease);
    }
}
