//! Prune: removes only stale idle managed worktrees that are clean and whose
//! HEAD is merged into the default ref. Dry-run is the default.
//!
//! Port of Go `internal/pool/prune.go`. Prune NEVER deletes a leased worktree,
//! an in-use worktree, an unmerged/dirty one, or an origin-unreachable one.
//! Backing-repository-missing orphans are skipped unless `--prune-orphans`.
//!
//! Two-phase execution reuses the same engine as destroy (reserve `Destroying`
//! + owner under lock, hooks outside, re-verify `sameDestroyReservation`).

use std::path::{Path, PathBuf};

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

/// The repository context one worktree is classified and removed against
/// (Go `pruneContext`).
///
/// A pool is keyed by ORIGIN URL, so two clones of the same origin share one
/// pool directory while every slot in it still belongs to exactly one physical
/// clone. Resolving a single root for the whole pool and applying it to every
/// slot runs `git worktree remove` from the wrong repository — which either
/// errors, leaving stale registrations to accumulate, or deregisters another
/// clone's bookkeeping. Under an `--all` sweep it is worse than wrong: the
/// pool is opened by directory, so `self.root` is the pool directory's PARENT
/// and not a repository at all, and nothing can ever be reclaimed.
///
/// Go's rule is stated at `resolvePoolRepoRoot` (destroy.go:614-620): callers
/// "must resolve every slot independently and must not apply one slot's root to
/// another".
#[derive(Debug, Clone)]
pub(crate) struct PruneContext {
    /// The repository that owns this worktree.
    pub repo_root: PathBuf,
    /// The ref HEAD must be merged into for the slot to be disposable. `None`
    /// when it could not be resolved, which every caller treats as
    /// "unverifiable" — never as "disposable".
    pub default_ref: Option<String>,
    /// Why `default_ref` is `None`, so the skip names the real cause instead of
    /// a bare "cannot verify".
    pub context_error: Option<String>,
}

/// What one `execute_prune` run did: what it removed, what it refused to
/// remove and why, and what it failed to clean up.
type PruneExecution = (Vec<PruneWorktree>, Vec<PruneSkipped>, Vec<CleanupError>);

/// Memoizing per-repository context resolver (Go
/// `worktreePruneContextResolver`, prune.go:401-418).
///
/// Resolution is per slot; the FETCH and default-ref lookup are per resolved
/// root, and a pool whose slots all belong to one clone — the ordinary case —
/// must not repeat that network round trip once per slot. Failures are cached
/// too, so a pool with a dozen unreachable slots retries once, not a dozen
/// times.
pub(crate) struct PruneContextResolver<'a> {
    pool: &'a Pool,
    by_root: std::collections::HashMap<PathBuf, Result<String, String>>,
}

impl<'a> PruneContextResolver<'a> {
    pub(crate) fn new(pool: &'a Pool) -> Self {
        Self {
            pool,
            by_root: std::collections::HashMap::new(),
        }
    }

    /// Resolves the context for one worktree, from its OWN path.
    ///
    /// `Err` is only for a repository root that cannot be resolved at all —
    /// there is then no context to report a skip against. A resolved root with
    /// a failed fetch still returns `Ok`, with `default_ref: None`, so the slot
    /// is skipped as unverifiable instead of vanishing from the run.
    pub(crate) fn context(&mut self, wt: &WorktreeEntry) -> Result<PruneContext, String> {
        let repo_root = self
            .pool
            .git
            .main_repo_root(Path::new(&wt.path))
            .map_err(|e| format!("resolve repository for worktree {}: {e}", wt.path))?;
        let resolved = self
            .by_root
            .entry(repo_root.clone())
            .or_insert_with(|| self.pool.resolve_prune_default_ref(&repo_root))
            .clone();
        let (default_ref, context_error) = match resolved {
            Ok(ref_) => (Some(ref_), None),
            Err(e) => (None, Some(e)),
        };
        Ok(PruneContext {
            repo_root,
            default_ref,
            context_error,
        })
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

        // Each slot is resolved against its OWN repository (M-020); the
        // fetch + default-ref lookup is memoized per resolved root.
        let mut resolver = PruneContextResolver::new(self);

        let mut result = PruneResult {
            dry_run: opts.dry_run,
            ..Default::default()
        };
        let mut planned: Vec<(PruneWorktree, PruneContext)> = Vec::new();

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
            // An orphan has no live repository behind it to resolve a ref in,
            // and asking anyway costs a failed `rev-parse` per orphan on every
            // run. `analyze_idle_worktree` returns on the orphan branch before
            // it ever reads the default ref, so an empty context is honest
            // there.
            let context = if self.backing_repository_missing(&wt.path) {
                PruneContext {
                    repo_root: PathBuf::new(),
                    default_ref: None,
                    context_error: None,
                }
            } else {
                match resolver.context(wt) {
                    Ok(c) => c,
                    Err(reason) => {
                        result.skipped.push(PruneSkipped {
                            name: wt.name.clone(),
                            path: wt.path.clone(),
                            category: PRUNE_SKIP_CANNOT_VERIFY.to_string(),
                            reason: "cannot verify worktree".to_string(),
                            detail: reason,
                        });
                        continue;
                    }
                }
            };
            let (worktree, skipped, stale) = self.analyze_idle_worktree(
                wt,
                context.default_ref.as_deref(),
                opts,
                context.context_error.as_deref(),
            );
            if !stale {
                continue;
            }
            if !skipped.reason.is_empty() {
                result.skipped.push(skipped);
                continue;
            }
            result.candidates.push(worktree.clone());
            result.reclaimable_bytes += worktree.bytes;
            planned.push((worktree, context));
        }

        if opts.dry_run || planned.is_empty() {
            return Ok(result);
        }

        // Execute (two-phase, reusing destroy's engine).
        let (pruned, exec_skipped, errors) = self.execute_prune(&planned, opts)?;
        result.pruned = pruned.clone();
        result.skipped.extend(exec_skipped);
        result.errors = errors;
        result.freed_bytes = pruned.iter().map(|w| w.bytes).sum();
        Ok(result)
    }

    /// Analyzes an idle worktree (Go `analyzeIdleWorktree`): orphan detection,
    /// dirty check, merge check, size.
    ///
    /// `context_error` is the reason the default ref could not be resolved; it
    /// is carried into the skip's `detail` so "origin unreachable" is not
    /// reported as a bare "cannot verify".
    #[allow(clippy::result_large_err)]
    fn analyze_idle_worktree(
        &self,
        wt: &WorktreeEntry,
        default_ref: Option<&str>,
        opts: &PruneOptions,
        context_error: Option<&str>,
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
            skipped.category = if context_error.is_some_and(|e| e.contains("origin unreachable")) {
                PRUNE_SKIP_ORIGIN_UNREACHABLE.to_string()
            } else {
                PRUNE_SKIP_CANNOT_VERIFY.to_string()
            };
            skipped.reason = "cannot verify default branch".to_string();
            skipped.detail = context_error.unwrap_or_default().to_string();
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
    /// Returns pruned worktrees, the skips that stopped a planned removal, and
    /// any cleanup errors. A worktree whose physical cleanup fails is
    /// **retained** in state — with its reservation restored, so it is
    /// immediately eligible for retry rather than stuck behind a `destroying`
    /// flag naming a process that is still alive.
    fn execute_prune(
        &self,
        planned: &[(PruneWorktree, PruneContext)],
        opts: &PruneOptions,
    ) -> Result<PruneExecution, PoolError> {
        // Phase 1: reserve Destroying + fresh owner under the lock.
        let reserved: Vec<(Reservation, PruneWorktree, PruneContext)> =
            crate::pool::with_pool_lock(&self.dir, self.lock_timeout, || {
                let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
                heal_state(&mut state, |pid| self.process.started_at(pid));
                let mut reserved = Vec::new();
                for (worktree, context) in planned {
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
                    reserved.push((reservation, worktree.clone(), context.clone()));
                }
                state_file::write_state(&self.dir, &state)
                    .map_err(|e| PoolError::Io("writing state".into(), e))?;
                Ok::<_, PoolError>(reserved)
            })?;

        // Hooks OUTSIDE all locks (non-fatal).
        if !planned.is_empty() && !self.config.hooks.pre_destroy.is_empty() {
            for (reservation, _, _) in &reserved {
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

        // Phase 2: RE-CLASSIFY + physical cleanup + state commit.
        //
        // The plan's verdict is not trusted: between planning and here the
        // `pre_destroy` hooks have run with no lock held, so a slot can have
        // gone dirty, gained a process, or landed on a different HEAD. Go
        // re-runs the whole classification inside the deleting lock
        // (`finalPruneSafetyCheck`, prune.go:607-625) and clears the
        // reservation on a skip, which is what makes the two-phase contract
        // mean anything.
        //
        // Invariant: a worktree is removed from state **only** when cleanup
        // fully succeeds. On failure the entry is retained, reservation
        // restored, so the next prune run can retry.
        crate::pool::with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let mut pruned = Vec::new();
            let mut skipped = Vec::new();
            let mut removed = std::collections::HashSet::new();
            let mut errors = Vec::new();

            for (reservation, worktree, context) in &reserved {
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

                // Re-classify against the state we just read.
                if let Some(skip) = self.final_prune_safety_check(
                    &state.worktrees[idx],
                    context,
                    opts,
                    worktree.orphaned,
                ) {
                    reservation.restore_original(&mut state.worktrees[idx]);
                    skipped.push(skip);
                    continue;
                }

                let repo = crate::git::GitRepo {
                    common_dir: context.repo_root.clone(),
                    worktree: None,
                };
                let orphaned = worktree.orphaned;

                // A: VCS deregistration. `remove_clean_worktree` is the
                // NON-FORCED removal (Go `RemoveCleanWorktree`, prune.go:551):
                // prune only ever deletes worktrees it has just proven clean,
                // and a forced removal would happily discard content that
                // became uncommitted inside the hook window. An orphan has no
                // live repository to deregister from, so it takes the
                // filesystem route alone (Go prune.go:535-550).
                let mut cleanup_ok = true;
                if !orphaned && let Err(e) = self.git.remove_clean_worktree(&repo, Path::new(&path))
                {
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
                } else {
                    // Retain the entry, but drop OUR reservation from it. Left
                    // stamped, the slot reads as `destroying` and every
                    // subsequent prune skips it until this process exits, which
                    // turns one failed cleanup into a permanently unprunable
                    // slot.
                    reservation.restore_original(&mut state.worktrees[idx]);
                }
            }

            state.worktrees.retain(|w| !removed.contains(&w.path));
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".into(), e))?;
            Ok::<_, PoolError>((pruned, skipped, errors))
        })
    }

    /// The last word on disposability, run inside the deleting lock (Go
    /// `finalPruneSafetyCheck` / `finalOrphanPruneSafetyCheck`).
    ///
    /// Returns `Some(skip)` when the slot must be left alone, `None` when it is
    /// still disposable. Deliberately re-derives everything — in-use, dirty,
    /// merge state, and for orphans the backing repository still being gone —
    /// rather than trusting the plan, which was computed before an unlocked
    /// hook window.
    fn final_prune_safety_check(
        &self,
        wt: &WorktreeEntry,
        context: &PruneContext,
        opts: &PruneOptions,
        planned_as_orphan: bool,
    ) -> Option<PruneSkipped> {
        let skipped = PruneSkipped {
            name: wt.name.clone(),
            path: wt.path.clone(),
            category: String::new(),
            reason: String::new(),
            detail: String::new(),
        };

        match self.process.is_worktree_in_use(Path::new(&wt.path)) {
            Err(e) => {
                return Some(PruneSkipped {
                    name: skipped.name,
                    path: skipped.path,
                    category: PRUNE_SKIP_CANNOT_CHECK_PROCESSES.to_string(),
                    reason: "cannot check processes".to_string(),
                    detail: e.to_string(),
                });
            }
            Ok(true) => {
                return Some(PruneSkipped {
                    name: skipped.name,
                    path: skipped.path,
                    category: PRUNE_SKIP_IN_USE.to_string(),
                    reason: PRUNE_SKIP_IN_USE.to_string(),
                    detail: String::new(),
                });
            }
            Ok(false) => {}
        }

        if self.backing_repository_missing(&wt.path) {
            // An orphan needs no repository to answer for it: the only question
            // left is whether its operator authorized orphan removal at all.
            if !opts.prune_orphans {
                return Some(PruneSkipped {
                    name: skipped.name,
                    path: skipped.path,
                    category: PRUNE_SKIP_ORPHANED.to_string(),
                    reason: PRUNE_ORPHAN_WARNING.to_string(),
                    detail: String::new(),
                });
            }
            return None;
        }

        if planned_as_orphan {
            // The slot was planned as an orphan but its backing repository is
            // back. Go refuses this route (prune.go:641-645) rather than
            // reclassifying it as an ordinary worktree: the plan already
            // committed to a filesystem-only removal, so switching to the VCS
            // route now would deregister a worktree this run never verified.
            return Some(PruneSkipped {
                name: skipped.name,
                path: skipped.path,
                category: PRUNE_SKIP_ORPHANED.to_string(),
                reason: "backing repository recovered".to_string(),
                detail: String::new(),
            });
        }

        let (_worktree, skipped, _) = self.analyze_idle_worktree(
            wt,
            context.default_ref.as_deref(),
            opts,
            context.context_error.as_deref(),
        );
        if skipped.reason.is_empty() {
            None
        } else {
            Some(skipped)
        }
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

    // ─── M-009: the execute phase re-classifies inside the deleting lock ──

    /// A `pre_destroy` hook that dirties the worktree it runs in. Hooks run
    /// outside every lock, so this reproduces "the slot changed between the
    /// plan and the delete" exactly — including the arbitrarily wide window a
    /// real hook opens.
    #[cfg(unix)]
    const DIRTY_HOOK: &str = "printf x > late-arrival.txt";
    #[cfg(windows)]
    const DIRTY_HOOK: &str = "echo x > late-arrival.txt";

    /// Builds a pool whose `pre_destroy` hook dirties the worktree, with one
    /// idle, clean, unleased worktree. The pool's temp HOME is returned
    /// alongside so the caller keeps it alive — dropping it deletes the pool.
    fn setup_prune_test_with_dirty_hook() -> (
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
        let pool = Pool::open(
            &repo,
            None,
            &OpenOptions {
                config: TreehouseConfig {
                    root: Some(fake_home.path().to_str().unwrap().to_string()),
                    hooks: crate::config::Hooks {
                        pre_destroy: vec![DIRTY_HOOK.to_string()],
                        ..Default::default()
                    },
                    ..TreehouseConfig::default_config()
                },
                ..Default::default()
            },
        )
        .unwrap();

        let acquired = pool
            .get(&crate::pool::AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        let wt_path = acquired.path.clone();
        pool.release(wt_path.to_str().unwrap()).unwrap();
        (pool, wt_path, fake_home, dir)
    }

    /// M-009: a slot that goes dirty between the plan and the delete must not
    /// be destroyed. The plan found it clean and idle; the `pre_destroy` hook
    /// (running with no lock held) made it dirty. The execute phase used to
    /// re-check only the reservation identity and delete it anyway, so the
    /// destroy engine and the prune engine disagreed about "disposable" under
    /// exactly the race the two-phase contract exists to close.
    #[test]
    fn prune_reclassifies_inside_the_deleting_lock() {
        let (pool, wt_path, _home, _repo) = setup_prune_test_with_dirty_hook();

        let result = pool
            .prune(&PruneOptions {
                dry_run: false,
                prune_orphans: false,
                pre_destroy: Vec::new(),
            })
            .unwrap();

        assert!(
            result.pruned.is_empty(),
            "a slot that dirtied after planning must not be destroyed, got: {:?}",
            result.pruned
        );
        assert!(result.errors.is_empty(), "this is a skip, not a failure");
        assert!(
            result
                .skipped
                .iter()
                .any(|s| s.category == PRUNE_SKIP_UNCOMMITTED),
            "the skip must name uncommitted changes, got: {:?}",
            result.skipped
        );
        assert!(
            wt_path.join("late-arrival.txt").exists(),
            "the worktree must survive with the late write intact"
        );

        // The state entry is retained, AND our destroy reservation is restored:
        // left stamped, the slot reads as `destroying` and every later prune
        // skips it until this process exits.
        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        let entry = state
            .worktrees
            .iter()
            .find(|w| w.path == wt_path)
            .expect("entry must remain in state");
        assert!(!entry.destroying, "the reservation must be restored");
        assert_eq!(entry.owner_pid, 0);
    }

    // ─── The orphan/recovered handoff in the final safety check ───────────

    /// Builds a pool whose `pre_destroy` hook RESTORES a hidden backing
    /// repository, with one idle worktree whose `.git` currently points at
    /// that hidden path (so it classifies as an orphan at plan time).
    ///
    /// Returns the pool, the worktree path, and the directory the real gitdir
    /// was moved into. The caller keeps both temp dirs alive.
    ///
    /// The pool is opened TWICE on purpose: the hook command can only be
    /// written once the gitdir it has to restore is known, and that path only
    /// exists after the first worktree has been created. Both pools resolve the
    /// same repository and root, so they share one pool directory.
    #[cfg(unix)]
    fn setup_prune_test_with_recovering_hook() -> (
        Pool,
        std::path::PathBuf,
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
        let root = fake_home.path().to_str().unwrap().to_string();

        // Pass 1: no hooks, just to materialize a worktree we can inspect.
        let plain = Pool::open(
            &repo,
            None,
            &OpenOptions {
                config: TreehouseConfig {
                    root: Some(root.clone()),
                    ..TreehouseConfig::default_config()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let acquired = plain
            .get(&crate::pool::AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        let wt_path = acquired.path.clone();
        plain.release(wt_path.to_str().unwrap()).unwrap();

        // Make it an orphan by moving the gitdir the `.git` file NAMES out of
        // the way. `backing_repository_missing` only asks whether that target
        // exists, so the rename is enough — and the hook can undo it.
        let contents = std::fs::read_to_string(wt_path.join(".git")).unwrap();
        assert!(contents.starts_with("gitdir:"), "got {contents:?}");
        let target = Path::new(contents.trim_start_matches("gitdir:").trim()).to_path_buf();
        assert!(
            target.is_absolute(),
            "expected an absolute gitdir, got {target:?}"
        );
        let hidden = target.with_extension("orphan-hidden");
        std::fs::rename(&target, &hidden).unwrap();

        // Pass 2: the same pool, now with a hook that puts the gitdir back.
        let hook = format!(
            "mv '{}' '{}'",
            hidden.to_string_lossy(),
            target.to_string_lossy()
        );
        let pool = Pool::open(
            &repo,
            None,
            &OpenOptions {
                config: TreehouseConfig {
                    root: Some(root),
                    hooks: crate::config::Hooks {
                        pre_destroy: vec![hook],
                        ..Default::default()
                    },
                    ..TreehouseConfig::default_config()
                },
                ..Default::default()
            },
        )
        .unwrap();

        (pool, wt_path, hidden, fake_home, dir)
    }

    /// A slot planned as an ORPHAN whose backing repository reappears must be
    /// reported as an orphan whose repository came back — Go's
    /// `finalOrphanPruneSafetyCheck` dispatch (prune.go:518-519, 645-647).
    ///
    /// The plan committed this slot to a filesystem-only removal with NO merge
    /// or dirty verification at all. Once the gitdir is back it is an ordinary,
    /// verifiable worktree again, so it must be classified as "recovered" and
    /// not quietly folded into the ordinary route that would deregister a
    /// worktree this run never checked.
    ///
    /// The assertion is on the skip CATEGORY, which is what Go reports and what
    /// the operator reads. Deleting the branch does also leave the worktree on
    /// disk — an orphan's empty context cannot resolve a default ref, so the
    /// fall-through refuses with a vaguer reason — but it loses the
    /// classification, which is the contract pinned here.
    #[cfg(unix)]
    #[test]
    fn orphan_planned_slot_is_skipped_when_its_repository_recovers() {
        let (pool, wt_path, hidden, _home, _repo) = setup_prune_test_with_recovering_hook();

        // Precondition: the pool agrees the slot is an orphan right now.
        assert!(
            pool.backing_repository_missing(&wt_path.to_string_lossy()),
            "test setup must leave the slot orphaned before prune runs"
        );

        // `prune_orphans: true` is required, or the plan never reaches the
        // orphan branch at all and this would prove nothing.
        let result = pool
            .prune(&PruneOptions {
                dry_run: false,
                prune_orphans: true,
                pre_destroy: Vec::new(),
            })
            .unwrap();

        assert!(
            result.pruned.is_empty(),
            "a slot whose repository came back must not be deleted: {:?}",
            result.pruned
        );
        assert!(
            result
                .skipped
                .iter()
                .any(|s| s.category == PRUNE_SKIP_ORPHANED
                    && s.reason == "backing repository recovered"),
            "the skip must name the recovered repository, got: {:?}",
            result.skipped
        );
        // The hook really did put the gitdir back (it MOVES the hidden copy
        // home, so `hidden` is gone and the slot is no longer orphaned), and
        // the worktree survived the skip.
        assert!(
            !hidden.exists(),
            "the hook should have moved the hidden gitdir back"
        );
        assert!(
            !pool.backing_repository_missing(&wt_path.to_string_lossy()),
            "the backing repository must be resolvable again"
        );
        assert!(wt_path.exists(), "the worktree must survive the skip");
    }

    // ─── M-020: the repo root is resolved per slot ───────────────────────

    /// M-020: `prune --all` opens the pool BY DIRECTORY, so `self.root` is the
    /// pool directory's parent — not a repository. Resolving one root for the
    /// whole pool therefore failed for every slot, `default_ref` came back
    /// unresolved, and prune could never reclaim anything: it degraded to
    /// "records a skip, keeps the entry" forever. Each slot must be resolved
    /// from its OWN path instead.
    #[test]
    fn prune_under_an_all_sweep_resolves_each_slots_own_repository() {
        let (pool, wt_path, _home, _repo) = setup_prune_test();
        // Reopen the way `--all` does: by pool directory, with no repository
        // in hand.
        let swept = Pool::open_at(pool.pool_dir(), &OpenOptions::default()).unwrap();
        assert_eq!(
            swept.root,
            pool.pool_dir().parent().unwrap(),
            "an --all sweep has no repository as its root"
        );

        let result = swept
            .prune(&PruneOptions {
                dry_run: false,
                prune_orphans: false,
                pre_destroy: Vec::new(),
            })
            .unwrap();

        assert!(
            result.skipped.is_empty(),
            "no slot should be unverifiable under a per-slot root, got: {:?}",
            result.skipped
        );
        assert_eq!(
            result.pruned.len(),
            1,
            "the idle worktree must actually be reclaimed"
        );
        assert!(!wt_path.exists(), "the worktree directory must be gone");
    }

    /// Every slot is resolved from its own path, so a pool whose slots belong
    /// to different clones is classified against each one's own merge ref.
    #[test]
    fn context_resolver_reads_the_worktrees_own_repository() {
        let (pool, wt_path, _home, _repo) = setup_prune_test();
        let mut resolver = PruneContextResolver::new(&pool);
        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        let wt = state.worktrees.iter().find(|w| w.path == wt_path).unwrap();
        let context = resolver.context(wt).unwrap();
        assert!(
            context.repo_root.ends_with("repo"),
            "must resolve the worktree's own repository, got: {:?}",
            context.repo_root
        );
        assert!(
            context.default_ref.is_some(),
            "a live local-only repo resolves a default ref"
        );
        // Memoized: a second call for the same slot must not re-fetch.
        let again = resolver.context(wt).unwrap();
        assert_eq!(again.repo_root, context.repo_root);
        assert_eq!(again.default_ref, context.default_ref);
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
