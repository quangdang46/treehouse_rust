//! Prune: removes only stale idle managed worktrees that are clean and whose
//! HEAD is merged into the default ref. Dry-run is the default.
//!
//! Port of Go `internal/pool/prune.go`. Prune NEVER deletes a leased worktree,
//! an in-use worktree, an unmerged/dirty one, or an origin-unreachable one.
//! Backing-repository-missing orphans are skipped unless `--prune-orphans`.
//!
//! Two-phase execution reuses the same engine as destroy (reserve `Destroying`
//! + owner under lock, hooks outside, re-verify `sameDestroyReservation`).

use std::path::Path;

use crate::pool::{Pool, PoolError};
use crate::reservation::Reservation;
use crate::state::{State, WorktreeEntry, heal_state};
use crate::state_file;

/// A stale or explicitly-selected orphaned worktree that prune can remove.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PruneWorktree {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub orphaned: bool,
    pub warning: String,
}

/// A worktree prune left in place for safety.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PruneSkipped {
    pub name: String,
    pub path: String,
    pub category: String,
    pub reason: String,
    pub detail: String,
}

/// A physical cleanup failure that left the state entry intact.
///
/// When `git worktree remove` or `remove_dir_all` fails, the worktree entry
/// is **retained** in state so it remains eligible for retry. This struct
/// records what failed and why.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CleanupError {
    pub name: String,
    pub path: String,
    /// Which phase failed: `"git_worktree_remove"` or `"filesystem_remove"`.
    pub phase: String,
    pub detail: String,
}

/// Dry-run candidates, removed worktrees, skipped worktrees, byte counts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PruneResult {
    pub dry_run: bool,
    pub candidates: Vec<PruneWorktree>,
    pub pruned: Vec<PruneWorktree>,
    pub skipped: Vec<PruneSkipped>,
    /// Worktrees whose state entry was retained because physical cleanup failed.
    /// These remain eligible for retry on the next prune run.
    #[serde(default)]
    pub errors: Vec<CleanupError>,
    pub reclaimable_bytes: u64,
    pub freed_bytes: u64,
}

/// Options controlling dry-run, orphan, and hook behavior.
#[derive(Debug, Clone)]
pub struct PruneOptions {
    pub dry_run: bool,
    pub prune_orphans: bool,
    pub pre_destroy: Vec<String>,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            prune_orphans: false,
            pre_destroy: Vec::new(),
        }
    }
}

// Skip category strings (byte-exact, scripts match these).
pub const PRUNE_SKIP_UNCOMMITTED: &str = "uncommitted changes";
pub const PRUNE_SKIP_UNMERGED: &str = "unmerged";
pub const PRUNE_SKIP_ORPHANED: &str = "orphaned (backing repository missing)";
pub const PRUNE_SKIP_ORIGIN_UNREACHABLE: &str = "origin unreachable (cannot verify)";
pub const PRUNE_SKIP_CANNOT_VERIFY: &str = "cannot verify worktree";
pub const PRUNE_SKIP_CANNOT_CHECK_PROCESSES: &str = "cannot check processes";
pub const PRUNE_SKIP_CANNOT_MEASURE_SIZE: &str = "cannot measure size";
pub const PRUNE_SKIP_CLEANUP_FAILED: &str = "cleanup failed";
pub const PRUNE_SKIP_REMOVE_FAILED: &str = "remove failed";
pub const PRUNE_SKIP_IN_USE: &str = "in use";
pub const PRUNE_ORPHAN_WARNING: &str = "content could not be verified";

impl Pool {
    /// Finds stale idle worktrees and optionally deletes them (Go `Prune`).
    pub fn prune(&self, opts: &PruneOptions) -> Result<PruneResult, PoolError> {
        // Snapshot under one lock: read + heal + write.
        let entries = with_pool_snapshot(self)?;

        // Resolve the default ref (fetch origin first).
        let repo_root = self
            .git
            .main_repo_root(&self.root)
            .unwrap_or_else(|_| self.root.clone());
        let default_ref = self.resolve_prune_default_ref(&repo_root);

        let mut result = PruneResult {
            dry_run: opts.dry_run,
            ..Default::default()
        };
        let mut planned: Vec<(PruneWorktree, String)> = Vec::new(); // (worktree, context_ref)

        for wt in &entries {
            if wt.destroying || wt.leased || crate::reservation::owner_alive(wt, &self.process) {
                continue; // leased / in-use silently skipped (never candidates)
            }
            let in_use = self
                .process
                .is_worktree_in_use(Path::new(&wt.path))
                .unwrap_or(true);
            if in_use {
                continue;
            }
            let (worktree, skipped, stale) =
                self.analyze_idle_worktree(wt, default_ref.as_ref().ok().map(|s| s.as_str()), opts);
            if !stale {
                continue;
            }
            if !skipped.reason.is_empty() {
                result.skipped.push(skipped);
                continue;
            }
            result.candidates.push(worktree.clone());
            result.reclaimable_bytes += worktree.bytes;
            planned.push((worktree, default_ref.clone().unwrap_or_default()));
        }

        if opts.dry_run || planned.is_empty() {
            return Ok(result);
        }

        // Execute (two-phase, reusing destroy's engine).
        let (pruned, errors) = self.execute_prune(&planned, &repo_root)?;
        result.pruned = pruned.clone();
        result.errors = errors;
        result.freed_bytes = pruned.iter().map(|w| w.bytes).sum();
        Ok(result)
    }

    /// Analyzes an idle worktree (Go `analyzeIdleWorktree`): orphan detection,
    /// dirty check, merge check, size.
    #[allow(clippy::result_large_err)]
    fn analyze_idle_worktree(
        &self,
        wt: &WorktreeEntry,
        default_ref: Option<&str>,
        opts: &PruneOptions,
    ) -> (PruneWorktree, PruneSkipped, bool) {
        let mut worktree = PruneWorktree {
            name: wt.name.clone(),
            path: wt.path.clone(),
            bytes: 0,
            orphaned: false,
            warning: String::new(),
        };
        let mut skipped = PruneSkipped {
            name: wt.name.clone(),
            path: wt.path.clone(),
            category: String::new(),
            reason: String::new(),
            detail: String::new(),
        };

        // Orphan: backing repo git metadata missing.
        if self.backing_repository_missing(&worktree.path) {
            if !opts.prune_orphans {
                skipped.category = PRUNE_SKIP_ORPHANED.to_string();
                skipped.reason = PRUNE_ORPHAN_WARNING.to_string();
                return (worktree, skipped, true);
            }
            let wt_path = Path::new(&worktree.path);
            let container = wt_path.parent().unwrap_or(wt_path);
            match dir_size(container) {
                Ok(bytes) => {
                    worktree.bytes = bytes;
                    worktree.orphaned = true;
                    worktree.warning = PRUNE_ORPHAN_WARNING.to_string();
                }
                Err(e) => {
                    skipped.category = PRUNE_SKIP_CANNOT_MEASURE_SIZE.to_string();
                    skipped.reason = "cannot measure size".to_string();
                    skipped.detail = e.to_string();
                    return (worktree, skipped, true);
                }
            }
            return (worktree, skipped, true);
        }

        // Dirty check.
        match self.git.is_dirty(Path::new(&worktree.path)) {
            Ok(true) => {
                skipped.category = PRUNE_SKIP_UNCOMMITTED.to_string();
                skipped.reason = PRUNE_SKIP_UNCOMMITTED.to_string();
                return (worktree, skipped, true);
            }
            Ok(false) => {}
            Err(e) => {
                if self.backing_repository_missing(&worktree.path) {
                    skipped.category = PRUNE_SKIP_ORPHANED.to_string();
                    skipped.reason = PRUNE_ORPHAN_WARNING.to_string();
                } else {
                    skipped.category = PRUNE_SKIP_CANNOT_VERIFY.to_string();
                    skipped.reason = "cannot check status".to_string();
                    skipped.detail = e.to_string();
                }
                return (worktree, skipped, true);
            }
        }

        // Merge check against the default ref.
        let Some(default_ref) = default_ref else {
            skipped.category = PRUNE_SKIP_ORIGIN_UNREACHABLE.to_string();
            skipped.reason = "cannot verify default branch".to_string();
            return (worktree, skipped, true);
        };
        match self
            .git
            .is_head_merged_into_ref(Path::new(&worktree.path), default_ref)
        {
            Ok(true) => {}
            Ok(false) => {
                skipped.category = PRUNE_SKIP_UNMERGED.to_string();
                skipped.reason = format!("HEAD not merged into {default_ref}");
                return (worktree, skipped, true);
            }
            Err(e) => {
                skipped.category = PRUNE_SKIP_CANNOT_VERIFY.to_string();
                skipped.reason = "cannot prove HEAD is merged into default branch".to_string();
                skipped.detail = e.to_string();
                return (worktree, skipped, true);
            }
        }

        // Size.
        let wt_path = Path::new(&worktree.path);
        let container = wt_path.parent().unwrap_or(wt_path);
        match dir_size(container) {
            Ok(bytes) => worktree.bytes = bytes,
            Err(e) => {
                skipped.category = PRUNE_SKIP_CANNOT_MEASURE_SIZE.to_string();
                skipped.reason = "cannot measure size".to_string();
                skipped.detail = e.to_string();
                return (worktree, skipped, true);
            }
        }
        (worktree, skipped, true)
    }

    /// Resolves the default merge ref, fetching origin first (Go
    /// `resolvePruneDefaultRef`). A failure is a categorized skip, never a
    /// deletion.
    pub(crate) fn resolve_prune_default_ref(&self, repo_root: &Path) -> Result<String, String> {
        let repo = crate::git::GitRepo {
            common_dir: repo_root.to_path_buf(),
            worktree: None,
        };
        // Fetch origin first (no-op without origin).
        if let Err(e) = self.git.fetch(&repo) {
            return Err(format!("origin unreachable (cannot verify): {e}"));
        }
        match self.git.default_branch_merge_ref(&repo) {
            Ok(ref_) => Ok(ref_),
            Err(e) => {
                let has_origin = self.git.has_remote(&repo, "origin");
                let category = if has_origin {
                    "origin unreachable (cannot verify)".to_string()
                } else {
                    "cannot verify worktree".to_string()
                };
                Err(format!("{category}: {e}"))
            }
        }
    }

    /// Executes the two-phase prune (Go `executePrune`).
    ///
    /// Returns pruned worktrees and any cleanup errors. A worktree whose
    /// physical cleanup fails is **retained** in state so it remains eligible
    /// for retry on the next prune run.
    fn execute_prune(
        &self,
        planned: &[(PruneWorktree, String)],
        _repo_root: &Path,
    ) -> Result<(Vec<PruneWorktree>, Vec<CleanupError>), PoolError> {
        // Phase 1: reserve Destroying + fresh owner under the lock.
        let reserved: Vec<(Reservation, PruneWorktree)> =
            crate::pool::with_pool_lock(&self.dir, self.lock_timeout, || {
                let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
                heal_state(&mut state, |pid| self.process.started_at(pid));
                let mut reserved = Vec::new();
                for (worktree, _) in planned {
                    let Some(idx) = state.worktrees.iter().position(|w| w.path == worktree.path)
                    else {
                        continue;
                    };
                    let path = state.worktrees[idx].path.clone();
                    let reservation = Reservation::reserve_destroy(
                        &path,
                        &mut state.worktrees[idx],
                        &self.process,
                    )
                    .map_err(|e| {
                        PoolError::Git(crate::git::GitError::new(
                            "reserve prune",
                            e.to_string(),
                            crate::git::GitErrorKind::Other,
                        ))
                    })?;
                    reserved.push((reservation, worktree.clone()));
                }
                state_file::write_state(&self.dir, &state)
                    .map_err(|e| PoolError::Io("writing state".into(), e))?;
                Ok::<_, PoolError>(reserved)
            })?;

        // Hooks OUTSIDE all locks (non-fatal).
        if !planned.is_empty() && !self.config.hooks.pre_destroy.is_empty() {
            for (reservation, _) in &reserved {
                let mut out = std::io::stdout();
                let mut err = std::io::stderr();
                crate::hooks::run(
                    &self.config.hooks.pre_destroy,
                    Path::new(&reservation.worktree),
                    &mut out,
                    &mut err,
                );
            }
        }

        // Phase 2: re-verify + physical cleanup + state commit.
        //
        // Invariant: a worktree is removed from state **only** when both
        // `git worktree remove` and `remove_dir_all` succeed. On failure the
        // entry is retained so the next prune run can retry.
        crate::pool::with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let mut pruned = Vec::new();
            let mut removed = std::collections::HashSet::new();
            let mut errors = Vec::new();

            for (reservation, worktree) in &reserved {
                let Some(idx) = state
                    .worktrees
                    .iter()
                    .position(|w| w.path == reservation.worktree)
                else {
                    continue;
                };
                if !reservation.matches(&state.worktrees[idx]) {
                    continue; // re-acquired mid-hook; never remove
                }
                let path = state.worktrees[idx].path.clone();
                let repo = crate::git::GitRepo {
                    common_dir: self.root.clone(),
                    worktree: None,
                };

                // A: git worktree remove.
                let mut cleanup_ok = true;
                if let Err(e) = self.git.worktree_remove(&repo, Path::new(&path)) {
                    errors.push(CleanupError {
                        name: worktree.name.clone(),
                        path: path.clone(),
                        phase: "git_worktree_remove".into(),
                        detail: e.to_string(),
                    });
                    cleanup_ok = false;
                }

                // B: filesystem removal (skip if git remove already failed).
                if cleanup_ok {
                    match std::fs::remove_dir_all(Path::new(&path)) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            // Path already gone (git removed it) — not an error.
                        }
                        Err(e) => {
                            errors.push(CleanupError {
                                name: worktree.name.clone(),
                                path: path.clone(),
                                phase: "filesystem_remove".into(),
                                detail: e.to_string(),
                            });
                            cleanup_ok = false;
                        }
                    }
                }

                if cleanup_ok {
                    removed.insert(path.clone());
                    pruned.push(worktree.clone());
                }
                // !cleanup_ok → entry stays in state, eligible for retry
            }

            state.worktrees.retain(|w| !removed.contains(&w.path));
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".into(), e))?;
            Ok::<_, PoolError>((pruned, errors))
        })
    }
}

/// Snapshot: read + heal + write under one lock.
pub(crate) fn with_pool_snapshot(pool: &Pool) -> Result<Vec<WorktreeEntry>, PoolError> {
    crate::pool::with_pool_lock(&pool.dir, pool.lock_timeout, || {
        let mut state = State::read_state(&pool.dir).map_err(PoolError::State)?;
        heal_state(&mut state, |pid| pool.process.started_at(pid));
        state_file::write_state(&pool.dir, &state)
            .map_err(|e| PoolError::Io("writing state".into(), e))?;
        Ok(state.worktrees.clone())
    })
}

/// Recursively measures a directory's size (Go `dirSize`).
fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            total += dir_size(&p)?;
        } else {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

/// Formats bytes the Go way (Go `formatBytes`): `N B`, else one-decimal
/// KiB/MiB/GiB/TiB with trailing zeros/dot trimmed.
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in &units {
        value /= 1024.0;
        unit = next;
        if value < 1024.0 {
            break;
        }
    }
    // Match Go exactly: TrimSuffix once for '0', then once for '.'.
    let mut formatted = format!("{value:.1}");
    if formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.pop();
    }
    format!("{formatted} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_golden() {
        // Mirrors Go's formatBytes: trailing zero + dot trimmed.
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        // 1024 / 1024 = 1.0 -> "1.0" -> trim '0' -> "1." -> trim '.' -> "1".
        assert_eq!(format_bytes(1024), "1 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1 GiB");
        // 1.10 -> trim one '0' -> "1.1".
        assert_eq!(format_bytes((1.1 * 1024.0) as u64), "1.1 KiB");
    }

    #[test]
    fn prune_options_default_dry_run() {
        let opts = PruneOptions::default();
        assert!(opts.dry_run);
        assert!(!opts.prune_orphans);
    }

    #[test]
    fn dir_size_counts_nested() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![1; 100]).unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), vec![2; 50]).unwrap();
        assert_eq!(dir_size(dir.path()).unwrap(), 150);
    }

    #[test]
    fn skip_categories_are_stable() {
        // Byte-exact strings scripts depend on.
        assert_eq!(PRUNE_SKIP_UNCOMMITTED, "uncommitted changes");
        assert_eq!(PRUNE_SKIP_UNMERGED, "unmerged");
        assert_eq!(PRUNE_SKIP_ORPHANED, "orphaned (backing repository missing)");
        assert_eq!(
            PRUNE_SKIP_ORIGIN_UNREACHABLE,
            "origin unreachable (cannot verify)"
        );
        assert_eq!(PRUNE_ORPHAN_WARNING, "content could not be verified");
    }

    // ─── Integration tests for cleanup error hardening ──────────────────

    use crate::config::TreehouseConfig;
    use crate::pool::{OpenOptions, Pool};

    /// Creates a real git repo + pool with one idle, clean, unleased worktree.
    /// Returns (pool, worktree_path, tmp_home_guard, tmp_repo_guard).
    fn setup_prune_test() -> (
        Pool,
        std::path::PathBuf,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let init = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(init.status.success());
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        run_git(&["config", "user.email", "t@t.com"]);
        run_git(&["config", "user.name", "T"]);
        std::fs::write(repo.join("README.md"), b"hi\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "init"]);

        let fake_home = tempfile::tempdir().unwrap();
        let opts = OpenOptions {
            config: TreehouseConfig {
                root: Some(fake_home.path().to_str().unwrap().to_string()),
                ..TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        let pool = Pool::open(&repo, None, &opts).unwrap();

        // Acquire then immediately release to leave an idle, clean worktree.
        let acquired = pool
            .get(&crate::pool::AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        let wt_path = acquired.path.clone();
        pool.release(wt_path.to_str().unwrap()).unwrap();

        // Sanity: worktree is available and idle.
        let status = pool.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status, "available");

        (pool, wt_path, fake_home, dir)
    }

    #[test]
    fn prune_retains_state_on_removal_failure() {
        let (pool, wt_path, _home, _repo) = setup_prune_test();

        // Make the worktree directory read-only. On Unix, git worktree remove
        // --force succeeds but remove_dir_all fails. On Windows, both phases
        // may fail because readonly prevents file deletion inside the dir.
        make_readonly(&wt_path);

        let result = pool
            .prune(&PruneOptions {
                dry_run: false,
                prune_orphans: false,
                pre_destroy: Vec::new(),
            })
            .unwrap();

        // Critical invariant: worktree must NOT be pruned on any failure.
        assert!(
            result.pruned.is_empty(),
            "worktree must not be pruned when physical cleanup fails"
        );
        // At least one error must be recorded.
        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].phase == "git_worktree_remove"
                || result.errors[0].phase == "filesystem_remove",
            "error phase must be git_worktree_remove or filesystem_remove, got: {}",
            result.errors[0].phase
        );

        // State must retain the entry (eligible for retry).
        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        assert!(
            state.worktrees.iter().any(|w| w.path == wt_path),
            "entry must remain in state after failed cleanup"
        );
    }

    #[test]
    fn prune_retry_succeeds_after_fixing_error() {
        let (pool, wt_path, _home, _repo) = setup_prune_test();

        // Make read-only → first prune fails.
        make_readonly(&wt_path);

        let first = pool
            .prune(&PruneOptions {
                dry_run: false,
                prune_orphans: false,
                pre_destroy: Vec::new(),
            })
            .unwrap();
        assert!(!first.errors.is_empty(), "first prune must record an error");
        assert!(first.pruned.is_empty());

        // State retains the entry (eligible for retry).
        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        assert!(
            state.worktrees.iter().any(|w| w.path == wt_path),
            "entry must remain in state after failed cleanup"
        );

        // Fix permissions → second prune succeeds.
        make_writable(&wt_path);

        let second = pool
            .prune(&PruneOptions {
                dry_run: false,
                prune_orphans: false,
                pre_destroy: Vec::new(),
            })
            .unwrap();
        assert!(
            second.errors.is_empty(),
            "retry must not produce errors after fixing the issue, got: {:?}",
            second.errors
        );
    }

    #[test]
    fn prune_happy_path_removes_state_entry() {
        let (pool, wt_path, _home, _repo) = setup_prune_test();

        let result = pool
            .prune(&PruneOptions {
                dry_run: false,
                prune_orphans: false,
                pre_destroy: Vec::new(),
            })
            .unwrap();

        assert_eq!(result.pruned.len(), 1);
        assert!(result.errors.is_empty());

        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        assert!(
            state.worktrees.iter().all(|w| w.path != wt_path),
            "state must not contain the pruned worktree"
        );
    }

    #[test]
    fn prune_not_found_directory_is_handled_gracefully() {
        let (pool, wt_path, _home, _repo) = setup_prune_test();

        // Pre-remove the worktree directory.
        std::fs::remove_dir_all(&wt_path).unwrap();
        assert!(!wt_path.exists());

        // heal_state drops entries with missing paths before prune analyzes.
        // Prune should complete without panicking or producing errors.
        let result = pool
            .prune(&PruneOptions {
                dry_run: false,
                prune_orphans: false,
                pre_destroy: Vec::new(),
            })
            .unwrap();

        assert!(
            result.pruned.is_empty(),
            "missing-directory worktree must not be pruned"
        );
    }

    // ─── Platform helpers ───────────────────────────────────────────────

    #[cfg(unix)]
    fn make_readonly(p: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o555)).unwrap();
    }

    #[cfg(unix)]
    fn make_writable(p: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    fn make_readonly(p: &std::path::Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_READONLY, SetFileAttributesW,
        };
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_READONLY);
        }
    }

    #[cfg(windows)]
    fn make_writable(p: &std::path::Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            // FILE_ATTRIBUTE_NORMAL = 0x80
            SetFileAttributesW(wide.as_ptr(), 0x80);
        }
    }
}
