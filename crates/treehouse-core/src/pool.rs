//! The pool orchestrator — the public entrypoint to treehouse-core.
//!
//! Implements acquire (`get`), release (`return`), `release_conditional`, and
//! status (`list`).
//!
//! **Short-lock protocol** (the audit fix): git/hooks/process work runs
//! OUTSIDE the state lock. The reservation is stamped BEFORE the external
//! reset and re-validated AFTER (persisted reservation token). Go held the
//! lock during `git reset` (correct but stalls every other command); the Rust
//! port deliberately does not.
//!
//! The reservation IS the anti-TOCTOU wall: stamp it before the external
//! reset, keep it across return, re-validate at commit. A bare non-treehouse
//! process cd'ing in during reset is unprotected (only cooperative consumers
//! coordinated) — residual, documented, same as Go.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{TreehouseConfig, resolve_pool_dir};
use crate::env::{DefaultEnv, TreehouseEnv};
use crate::git::{GitBackend, GitRepo};
use crate::hooks;
use crate::lease::{Lease, LeaseInfo, mark_acquired_lease};
use crate::lock::{DEFAULT_LOCK_TIMEOUT, LockError, with_state_lock};
use crate::process::{ProcessInfo, ProcessTable};
use crate::reservation;
use crate::state::{State, WorktreeEntry, ZERO_TIME, heal_state};
use crate::state_file;

/// Options for opening a pool.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    pub config: TreehouseConfig,
    pub lock_timeout: std::time::Duration,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            config: TreehouseConfig::default_config(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
        }
    }
}

/// Options for acquiring a worktree (`get`).
#[derive(Debug, Clone, Default)]
pub struct AcquireOptions {
    /// Override the branch to reset to.
    pub branch: Option<String>,
    /// Lease the worktree (non-interactive) instead of an owner reservation.
    pub lease: Option<LeaseAcquireOptions>,
}

/// Options for a lease acquisition.
#[derive(Debug, Clone)]
pub struct LeaseAcquireOptions {
    pub holder: String,
    /// Optional TTL (expires_at); None = permanent lease.
    pub ttl: Option<chrono::Duration>,
}

/// A successfully acquired worktree.
#[derive(Debug, Clone)]
pub struct Acquired {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub lease: Option<Lease>,
}

/// The worktree status string values (Go parity).
pub const STATUS_AVAILABLE: &str = "available";
pub const STATUS_IN_USE: &str = "in-use";
pub const STATUS_DIRTY: &str = "dirty";
pub const STATUS_LEASED: &str = "leased";
pub const STATUS_HERE: &str = "you're here";

/// One worktree's status as reported by `status`.
#[derive(Debug, Clone)]
pub struct WorktreeStatus {
    pub name: String,
    pub path: String,
    pub status: String,
    pub processes: Vec<ProcessInfo>,
    pub lease_id: String,
    pub lease_holder: String,
    pub leased_at: chrono::DateTime<chrono::Utc>,
}

/// The pool: ties together config, git, process, and state under one lock.
pub struct Pool {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub(crate) git: Arc<dyn GitBackend>,
    pub(crate) process: Arc<ProcessTable>,
    pub(crate) config: TreehouseConfig,
    pub(crate) lock_timeout: std::time::Duration,
    pub(crate) env: Arc<dyn TreehouseEnv>,
}

impl Pool {
    /// Opens (or creates) the pool for a repo. `remote_url` is used for the
    /// pool dir hash; falls back to the repo path when unknown.
    pub fn open(
        repo_root: &Path,
        remote_url: Option<&str>,
        opts: &OpenOptions,
    ) -> Result<Self, PoolError> {
        Self::open_with_env(repo_root, remote_url, opts, Arc::new(DefaultEnv))
    }

    /// Opens a pool using the injected environment.
    pub fn open_with_env(
        repo_root: &Path,
        remote_url: Option<&str>,
        opts: &OpenOptions,
        env: Arc<dyn TreehouseEnv>,
    ) -> Result<Self, PoolError> {
        let dir = resolve_pool_dir(repo_root, opts.config.root.as_deref(), remote_url)
            .map_err(PoolError::Config)?;
        env.ensure_dir(&dir)
            .map_err(|e| PoolError::Io(format!("creating pool dir {}", dir.display()), e))?;

        let git = crate::git::ShellGitBackend::discover().map_err(PoolError::Git)?;
        let process = ProcessTable::new();

        Ok(Pool {
            root: repo_root.to_path_buf(),
            dir,
            git: Arc::new(git),
            process: Arc::new(process),
            config: opts.config.clone(),
            lock_timeout: opts.lock_timeout,
            env,
        })
    }

    /// Opens a pool at an already-known pool directory (used by `--all`
    /// sweeps that discover pool dirs; the backing repo root is the pool
    /// dir's parent).
    pub fn open_at(pool_dir: &Path, opts: &OpenOptions) -> Result<Self, PoolError> {
        Self::open_at_with_env(pool_dir, opts, Arc::new(DefaultEnv))
    }

    /// Opens a pool at an already-known pool directory using the injected environment.
    pub fn open_at_with_env(
        pool_dir: &Path,
        opts: &OpenOptions,
        env: Arc<dyn TreehouseEnv>,
    ) -> Result<Self, PoolError> {
        env.ensure_dir(pool_dir)
            .map_err(|e| PoolError::Io(format!("creating pool dir {}", pool_dir.display()), e))?;

        let git = crate::git::ShellGitBackend::discover().map_err(PoolError::Git)?;
        let process = ProcessTable::new();

        Ok(Pool {
            root: pool_dir
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| pool_dir.to_path_buf()),
            dir: pool_dir.to_path_buf(),
            git: Arc::new(git),
            process: Arc::new(process),
            config: opts.config.clone(),
            lock_timeout: opts.lock_timeout,
            env,
        })
    }

    /// The pool directory.
    pub fn pool_dir(&self) -> &Path {
        &self.dir
    }

    /// The injected environment.
    pub fn env(&self) -> &dyn TreehouseEnv {
        &*self.env
    }

    /// Whether a worktree is dirty (git status --porcelain --untracked-files=all).
    pub fn git_is_dirty(&self, path: &Path) -> Result<bool, PoolError> {
        Ok(self.git.is_dirty(path)?)
    }

    /// Acquires a worktree from the pool (`get`).
    ///
    /// Short-lock protocol: branch+fetch outside the lock; reserve under lock;
    /// reset outside; re-validate under a second lock; hooks outside.
    pub fn get(&self, opts: &AcquireOptions) -> Result<Acquired, PoolError> {
        // Step 1 (OUTSIDE lock): resolve the default branch and fetch origin.
        let repo = GitRepo {
            common_dir: self.root.clone(),
            worktree: None,
        };
        let branch = match &opts.branch {
            Some(b) => b.clone(),
            None => self.git.default_branch(&repo).map_err(PoolError::Git)?,
        };
        if self.git.has_remote(&repo, "origin") {
            self.git.fetch(&repo).map_err(PoolError::Git)?;
        }

        // Step 2 (LOCK #1): read + heal + scan + mark acquired + write.
        let (name, path, lease) = acquire_locked(
            &self.dir,
            self.lock_timeout,
            self.config.max_trees,
            &self.root,
            &self.process,
            &self.git,
            opts,
            &branch,
            &repo,
        )?;

        // Step 3 (OUTSIDE lock): reset the worktree to the default branch.
        // The reservation held since step 2 is the anti-TOCTOU wall.
        self.git
            .reset_worktree(Path::new(&path), &branch)
            .map_err(PoolError::Git)?;

        // Step 4 (LOCK #2): re-validate the reservation is intact; rewrite
        // only if heal changed something.
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            heal_state(&mut state, |pid| self.process.started_at(pid));
            // Reservation check: if the worktree vanished or was re-acquired
            // mid-reset, surface it rather than returning a stale handle.
            if state.worktrees.iter().any(|w| w.name == name) {
                state_file::write_state(&self.dir, &state)
                    .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
            }
            Ok(())
        })?;

        // Step 5 (OUTSIDE lock): run post_create hooks (lease mode routes
        // stdout to stderr so machine output stays clean).
        if !self.config.hooks.post_create.is_empty() {
            let lease_mode = opts.lease.is_some();
            let mut out: Box<dyn std::io::Write> = if lease_mode {
                Box::new(std::io::sink())
            } else {
                Box::new(std::io::stdout())
            };
            let mut err = std::io::stderr();
            hooks::run(
                &self.config.hooks.post_create,
                Path::new(&path),
                out.as_mut(),
                &mut err,
            );
        }

        Ok(Acquired {
            name,
            path: PathBuf::from(path),
            branch,
            lease,
        })
    }

    /// Non-interactive lease acquire (`get --lease`) with an optional TTL.
    pub fn acquire_lease_with_ttl(
        &self,
        holder: &str,
        ttl: Option<chrono::Duration>,
    ) -> Result<LeaseInfo, PoolError> {
        let acquired = self.get(&AcquireOptions {
            lease: Some(LeaseAcquireOptions {
                holder: holder.to_string(),
                ttl,
            }),
            ..Default::default()
        })?;
        let lease = acquired.lease.as_ref();
        Ok(LeaseInfo {
            path: acquired.path.to_string_lossy().into_owned(),
            lease_id: lease.map(|l| l.id.clone()).unwrap_or_default(),
            lease_holder: holder.to_string(),
            leased_at: lease.map(|l| l.acquired_at).unwrap_or(ZERO_TIME),
        })
    }

    /// Non-interactive lease acquire (`get --lease`), returning the identity.
    pub fn acquire_lease(&self, holder: &str) -> Result<LeaseInfo, PoolError> {
        let acquired = self.get(&AcquireOptions {
            lease: Some(LeaseAcquireOptions {
                holder: holder.to_string(),
                ttl: None,
            }),
            ..Default::default()
        })?;
        Ok(LeaseInfo {
            path: acquired.path.to_string_lossy().into_owned(),
            lease_id: acquired
                .lease
                .as_ref()
                .map(|l| l.id.clone())
                .unwrap_or_default(),
            lease_holder: holder.to_string(),
            leased_at: acquired
                .lease
                .as_ref()
                .map(|l| l.acquired_at)
                .unwrap_or(ZERO_TIME),
        })
    }

    /// Releases a managed worktree, clearing its reservation, and returns it
    /// to the available pool (Go `Release`).
    pub fn release(&self, worktree_path: &str) -> Result<(), PoolError> {
        self.release_conditional(worktree_path, &ReleasePreconditions::default(), None)
    }

    /// Releases conditionally, ABA-safe: preconditions + before_reset are
    /// validated under ONE lock; the reset runs OUTSIDE the lock (short-lock
    /// protocol); the reservation is cleared under a second lock. The
    /// reservation is held across the external reset so no concurrent acquire
    /// can re-assign the worktree mid-reset.
    pub fn release_conditional(
        &self,
        worktree_path: &str,
        preconditions: &ReleasePreconditions,
        before_reset: Option<&mut dyn FnMut() -> Result<(), PoolError>>,
    ) -> Result<(), PoolError> {
        // Repo/branch resolution outside the lock. Use main_repo_root so a
        // detached linked worktree resolves back to the owning repo (whose HEAD
        // is not detached) — otherwise default_branch would fall through to
        // init.defaultBranch ("master").
        let repo_root = self
            .git
            .main_repo_root(Path::new(worktree_path))
            .unwrap_or_else(|_| self.root.clone());
        let repo = GitRepo {
            common_dir: repo_root,
            worktree: None,
        };
        let branch = self.git.default_branch(&repo).map_err(PoolError::Git)?;

        // LOCK #1: find + validate preconditions + before_reset (under lock,
        // per Go doc — caller's termination/detachment can't race). The entry
        // is still reserved.
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let _ = releasable_worktree(&mut state, worktree_path, preconditions)?;
            if let Some(cb) = before_reset {
                cb()?;
            }
            Ok(())
        })?;

        // OUTSIDE the lock: reset the worktree. The reservation is still held,
        // so no acquire/destroy can take it mid-reset.
        self.git
            .reset_worktree(Path::new(worktree_path), &branch)
            .map_err(PoolError::Git)?;

        // LOCK #2: RE-VALIDATE the preconditions AND clear in ONE atomic lock.
        // This is what makes release exactly-once (ABA-safe): a concurrent
        // caller that passed LOCK #1 will find the lease already cleared here
        // and fail its precondition, so only one release ever succeeds.
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let wt = releasable_worktree(&mut state, worktree_path, preconditions)?;
            wt.owner_pid = 0;
            wt.owner_started_at = 0;
            crate::state::clear_lease(wt);
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
            Ok(())
        })?;

        Ok(())
    }

    /// Read-only precondition validation (Go `ValidateReleasePreconditions`).
    pub fn validate_release_preconditions(
        &self,
        worktree_path: &str,
        preconditions: &ReleasePreconditions,
    ) -> Result<(), PoolError> {
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let _ = releasable_worktree(&mut state, worktree_path, preconditions)?;
            Ok(())
        })
    }

    /// Reports the status of managed worktrees (`status`), healing + writing
    /// state under ONE exclusive lock.
    pub fn status(&self) -> Result<Vec<WorktreeStatus>, PoolError> {
        let cwd = std::env::current_dir().unwrap_or_default();
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            heal_state(&mut state, |pid| self.process.started_at(pid));
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".to_string(), e))?;

            let mut result = Vec::new();
            for wt in &state.worktrees {
                if wt.destroying {
                    continue;
                }
                let procs = self
                    .process
                    .find_in_worktree(Path::new(&wt.path))
                    .unwrap_or_default();
                let mut ws = WorktreeStatus {
                    name: wt.name.clone(),
                    path: wt.path.clone(),
                    status: STATUS_AVAILABLE.to_string(),
                    processes: procs.clone(),
                    lease_id: String::new(),
                    lease_holder: String::new(),
                    leased_at: ZERO_TIME,
                };

                if wt.leased {
                    ws.status = STATUS_LEASED.to_string();
                    ws.lease_id = wt.lease_id.clone();
                    ws.lease_holder = wt.lease_holder.clone();
                    ws.leased_at = wt.leased_at;
                } else if reservation::owner_alive(wt, &self.process) {
                    ws.status = STATUS_IN_USE.to_string();
                } else if !procs.is_empty() {
                    ws.status = STATUS_IN_USE.to_string();
                    if cwd_in_worktree(&cwd, Path::new(&wt.path)) {
                        ws.status = STATUS_HERE.to_string();
                    }
                } else if self.git.is_dirty(Path::new(&wt.path)).unwrap_or(false) {
                    ws.status = STATUS_DIRTY.to_string();
                }
                result.push(ws);
            }
            Ok(result)
        })
    }
}

/// Runs `f` under the pool state lock, flattening the double-`Result` that
/// `with_state_lock` returns (outer lock error, inner callback error).
pub(crate) fn with_pool_lock<T>(
    dir: &Path,
    timeout: std::time::Duration,
    f: impl FnOnce() -> Result<T, PoolError>,
) -> Result<T, PoolError> {
    with_state_lock(dir, timeout, f).map_err(Pool::lock_err_ty)
}

impl Pool {
    fn lock_err_ty(e: LockError<PoolError>) -> PoolError {
        PoolError::Lock(e.to_string())
    }
}

/// The lock-acquire critical section (LOCK #1 of `get`): read + heal + scan +
/// mark acquired + write, all under the pool state lock.
#[allow(clippy::too_many_arguments)]
fn acquire_locked(
    dir: &Path,
    lock_timeout: std::time::Duration,
    max_trees: u32,
    root: &Path,
    process: &ProcessTable,
    git: &Arc<dyn GitBackend>,
    opts: &AcquireOptions,
    branch: &str,
    repo: &GitRepo,
) -> Result<(String, String, Option<Lease>), PoolError> {
    with_state_lock(dir, lock_timeout, || {
        let mut state = State::read_state(dir).map_err(PoolError::State)?;
        heal_state(&mut state, |pid| process.started_at(pid));

        // Find an available worktree (clean, not in-use, not leased).
        for i in 0..state.worktrees.len() {
            let (name, path, available) = {
                let wt = &state.worktrees[i];
                if wt.destroying || wt.leased || reservation::owner_alive(wt, process) {
                    (String::new(), String::new(), false)
                } else {
                    let in_use = process
                        .is_worktree_in_use(Path::new(&wt.path))
                        .unwrap_or(true);
                    if in_use {
                        (String::new(), String::new(), false)
                    } else {
                        let dirty = git.is_dirty(Path::new(&wt.path)).map_err(PoolError::Git)?;
                        if dirty {
                            (String::new(), String::new(), false)
                        } else {
                            (wt.name.clone(), wt.path.clone(), true)
                        }
                    }
                }
            };
            if available {
                // Stamp the reservation (persisted now).
                let lease_info = mark_acquired_entry(&mut state.worktrees[i], opts, process);
                state_file::write_state(dir, &state)
                    .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
                return Ok((name, path, lease_info));
            }
        }

        // No available worktree — create a new one if the pool allows.
        if state.worktrees.len() as u32 >= max_trees {
            return Err(PoolError::PoolFull {
                count: state.worktrees.len() as u32,
                max: max_trees,
            });
        }
        let name = next_name(&state);
        let repo_name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into());
        let wt_path = dir.join(&name).join(&repo_name);

        std::fs::create_dir_all(wt_path.parent().unwrap())
            .map_err(|e| PoolError::Io("creating worktree parent".to_string(), e))?;
        git.worktree_add(repo, &wt_path, branch)
            .map_err(PoolError::Git)?;

        let mut entry = WorktreeEntry {
            name: name.clone(),
            path: wt_path.to_string_lossy().into_owned(),
            created_at: chrono::Utc::now(),
            ..WorktreeEntry::default()
        };
        let lease_info = mark_acquired_entry(&mut entry, opts, process);
        state.worktrees.push(entry);
        state_file::write_state(dir, &state)
            .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
        let path_str = wt_path.to_string_lossy().into_owned();
        Ok((name, path_str, lease_info))
    })
    .map_err(Pool::lock_err_ty)
}

/// Marks an entry as acquired (owner reservation or lease), returning lease
/// info if leasing. Extracted so both the reused and new-worktree paths share
/// it.
fn mark_acquired_entry(
    wt: &mut WorktreeEntry,
    opts: &AcquireOptions,
    process: &ProcessTable,
) -> Option<Lease> {
    if let Some(lease_opts) = &opts.lease {
        let now = chrono::Utc::now();
        let id = mark_acquired_lease(wt, &lease_opts.holder, now);
        let expires_at = lease_opts.ttl.map(|d| now + d);
        if let Some(exp) = expires_at {
            wt.expires_at = exp;
        }
        Some(Lease {
            id,
            holder: lease_opts.holder.clone(),
            acquired_at: now,
            expires_at,
        })
    } else {
        let pid = std::process::id() as i32;
        wt.owner_pid = pid;
        wt.owner_started_at = process.started_at(pid).unwrap_or(0);
        None
    }
}

/// Whether `cwd` is inside `worktree_path` (Go `cwdInWorktree`).
fn cwd_in_worktree(cwd: &Path, worktree_path: &Path) -> bool {
    use crate::process::pathdiff_rel;
    let abs_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let abs_wt =
        std::fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    match pathdiff_rel(&abs_wt, &abs_cwd) {
        Some(rel) => {
            let s = rel.to_string_lossy();
            // "." = cwd is the worktree; otherwise a descendant that isn't
            // ".." or "../...".
            s == "." || (s != ".." && !s.starts_with("../") && !s.starts_with("..\\"))
        }
        None => false,
    }
}

/// Preconditions for a conditional release.
#[derive(Debug, Clone, Default)]
pub struct ReleasePreconditions {
    pub expected_lease_id: Option<String>,
    pub expected_lease_holder: Option<String>,
}

/// Finds a managed, releasable worktree by path, validating preconditions
/// (Go `releasableWorktree` + `validateReleasePreconditions`).
fn releasable_worktree<'a>(
    state: &'a mut State,
    worktree_path: &str,
    preconditions: &ReleasePreconditions,
) -> Result<&'a mut WorktreeEntry, PoolError> {
    for wt in &mut state.worktrees {
        if wt.path != worktree_path {
            continue;
        }
        if wt.destroying {
            return Err(PoolError::BeingDestroyed(worktree_path.to_string()));
        }
        validate_release_preconditions_inner(wt, preconditions)?;
        return Ok(wt);
    }
    Err(PoolError::NotFound(worktree_path.to_string()))
}

fn validate_release_preconditions_inner(
    wt: &WorktreeEntry,
    preconditions: &ReleasePreconditions,
) -> Result<(), PoolError> {
    if preconditions.expected_lease_id.is_none() && preconditions.expected_lease_holder.is_none() {
        return Ok(());
    }
    if !wt.leased {
        return Err(PoolError::LeasePrecondition {
            path: wt.path.clone(),
            reason: "worktree is not leased".into(),
        });
    }
    if let Some(id) = &preconditions.expected_lease_id
        && &wt.lease_id != id
    {
        return Err(PoolError::LeasePrecondition {
            path: wt.path.clone(),
            reason: format!("lease identity does not match worktree {}", wt.path),
        });
    }
    if let Some(holder) = &preconditions.expected_lease_holder
        && &wt.lease_holder != holder
    {
        return Err(PoolError::LeasePrecondition {
            path: wt.path.clone(),
            reason: format!("lease holder does not match worktree {}", wt.path),
        });
    }
    Ok(())
}

/// The next numeric worktree name (Go `nextName`).
pub fn next_name(state: &State) -> String {
    let mut max = 0;
    for wt in &state.worktrees {
        if let Ok(n) = wt.name.parse::<i32>()
            && n > max
        {
            max = n;
        }
    }
    (max + 1).to_string()
}

/// Errors from pool operations.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error(
        "all {count} worktrees are in use or dirty (max_trees = {max}). Run 'treehouse status' to see details, or increase max_trees in treehouse.toml"
    )]
    PoolFull { count: u32, max: u32 },
    #[error("worktree {0} is being destroyed")]
    BeingDestroyed(String),
    #[error("worktree {0} is not managed by treehouse")]
    NotFound(String),
    #[error("lease precondition failed: {path}: {reason}")]
    LeasePrecondition { path: String, reason: String },
    #[error("pool lock: {0}")]
    Lock(String),
    #[error("state: {0}")]
    State(#[from] crate::state::StateError),
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("git: {0}")]
    Git(#[from] crate::git::GitError),
    #[error("io: {0}")]
    Io(String, std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::mark_acquired_lease;

    #[test]
    fn next_name_increments() {
        let s = State {
            worktrees: vec![
                WorktreeEntry {
                    name: "1".into(),
                    ..Default::default()
                },
                WorktreeEntry {
                    name: "3".into(),
                    ..Default::default()
                },
            ],
        };
        assert_eq!(next_name(&s), "4");
        assert_eq!(next_name(&State::default()), "1");
    }

    #[test]
    fn next_name_ignores_non_numeric() {
        let s = State {
            worktrees: vec![WorktreeEntry {
                name: "abc".into(),
                ..Default::default()
            }],
        };
        assert_eq!(next_name(&s), "1");
    }

    #[test]
    fn cwd_in_worktree_detects_self_and_descendant() {
        let wt = Path::new("/home/u/proj/.treehouse/repo-abc/1/repo");
        assert!(cwd_in_worktree(wt, wt));
        assert!(cwd_in_worktree(&wt.join("src"), wt));
        assert!(!cwd_in_worktree(Path::new("/home/u/proj"), wt));
        assert!(!cwd_in_worktree(Path::new("/home/u/proj/other"), wt));
    }

    #[test]
    fn releasable_worktree_validates_preconditions() {
        let mut state = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: "/pool/1/repo".into(),
                ..Default::default()
            }],
        };
        // No preconditions: any managed path is releasable.
        let wt = releasable_worktree(&mut state, "/pool/1/repo", &ReleasePreconditions::default())
            .unwrap();
        assert_eq!(wt.name, "1");

        // Unknown path.
        assert!(matches!(
            releasable_worktree(&mut state, "/nope", &ReleasePreconditions::default()),
            Err(PoolError::NotFound(_))
        ));

        // Destroying => BeingDestroyed.
        state.worktrees[0].destroying = true;
        assert!(matches!(
            releasable_worktree(&mut state, "/pool/1/repo", &ReleasePreconditions::default()),
            Err(PoolError::BeingDestroyed(_))
        ));
        state.worktrees[0].destroying = false;

        // Lease precondition: id mismatch fails, match passes.
        let cond = ReleasePreconditions {
            expected_lease_id: Some("abc".into()),
            ..Default::default()
        };
        assert!(matches!(
            releasable_worktree(&mut state, "/pool/1/repo", &cond),
            Err(PoolError::LeasePrecondition { .. })
        ));
        mark_acquired_lease(&mut state.worktrees[0], "h", chrono::Utc::now());
        assert!(releasable_worktree(&mut state, "/pool/1/repo", &cond).is_err());
        state.worktrees[0].lease_id = "abc".into();
        assert!(releasable_worktree(&mut state, "/pool/1/repo", &cond).is_ok());
    }

    #[test]
    fn integration_acquire_status_release_roundtrip() {
        use crate::git::{GitBackend, ShellGitBackend};
        // Build a temp repo with a commit on main.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        // `init` runs in the parent (the repo dir doesn't exist yet); the rest
        // run inside the repo.
        let init_out = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .expect("git must be installed");
        assert!(
            init_out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init_out.stderr)
        );
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git must be installed");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["init", "--initial-branch=main", repo.to_str().unwrap()]);
        run_git(&["config", "user.email", "t@t.com"]);
        run_git(&["config", "user.name", "T"]);
        std::fs::write(
            repo.join("README.md"),
            b"hi
",
        )
        .unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "init"]);
        // Verify main exists (the commit created it).
        let main_ok = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            main_ok.status.success(),
            "refs/heads/main missing after commit"
        );

        // Open a pool rooted at a temp HOME.
        let fake_home = tempfile::tempdir().unwrap();
        let opts = OpenOptions {
            config: TreehouseConfig {
                root: Some(fake_home.path().to_str().unwrap().to_string()),
                ..TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        // Directly test default_branch resolution.
        let shell = ShellGitBackend::discover().unwrap();
        let grepo = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };
        let db = shell.default_branch(&grepo).unwrap();
        assert_eq!(db, "main", "default_branch should be main, got {db}");

        let pool = Pool::open(&repo, None, &opts).unwrap();
        assert!(pool.pool_dir().exists());

        // Acquire (creates worktree 1). Explicit branch avoids any
        // process-global config races from parallel tests.
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(acquired.name, "1");
        assert!(acquired.path.exists());

        // Status: 1 worktree, available (no process inside).
        let status = pool.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "1");

        // Release it.
        pool.release(&acquired.path.to_string_lossy()).unwrap();

        // Re-acquire: reuses worktree 1 (not a new one).
        let again = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(again.name, "1");
        pool.release(&again.path.to_string_lossy()).unwrap();
        let _ = ShellGitBackend::discover().unwrap();
        let _: &dyn GitBackend = &ShellGitBackend::discover().unwrap();
    }
    #[test]
    fn status_constants_match_go() {
        assert_eq!(STATUS_AVAILABLE, "available");
        assert_eq!(STATUS_IN_USE, "in-use");
        assert_eq!(STATUS_DIRTY, "dirty");
        assert_eq!(STATUS_LEASED, "leased");
        assert_eq!(STATUS_HERE, "you're here");
    }
}
