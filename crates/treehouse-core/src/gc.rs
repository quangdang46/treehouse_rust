//! `treehouse gc`: safe reclaim of stale, orphaned, and dead-owner worktrees.
//!
//! The #1 agent pain: worktrees leak when agents crash or forget to return
//! them. `gc` reclaims them, but ONLY when provably safe:
//! - A **valid** lease is NEVER a candidate (distinct skip category).
//! - An **expired** lease is a candidate only if it also passes the full
//!   disposable bar (idle + clean + merged + backing repo present).
//! - Corrupt-recovered entries (no `expires_at`, permanent lease) are NEVER
//!   gc'd — a human must verify.
//! - A live agent past TTL is never evicted: its running process makes the
//!   worktree "in use", so gc skips it.
//!
//! Dry-run by default. Execution reuses the shared two-phase engine (reserve
//! `Destroying`+owner under lock, hooks outside, re-verify
//! `sameDestroyReservation`, remove).

use std::path::Path;

use crate::pool::{Pool, PoolError};
use crate::prune::PRUNE_SKIP_IN_USE;
use crate::reservation::Reservation;
use crate::state::{State, WorktreeEntry, heal_state};
use crate::state_file;

/// A worktree gc can reclaim (stale) or explicitly selected orphan.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GcWorktree {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    /// The category tag shown in output.
    pub tag: String,
    pub warning: String,
}

/// A worktree gc left in place.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GcSkipped {
    pub name: String,
    pub path: String,
    pub category: String,
    pub reason: String,
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

/// The result of a gc run.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GcResult {
    pub dry_run: bool,
    pub candidates: Vec<GcWorktree>,
    pub reclaimed: Vec<GcWorktree>,
    pub skipped: Vec<GcSkipped>,
    /// Worktrees whose state entry was retained because physical cleanup failed.
    /// These remain eligible for retry on the next gc run.
    #[serde(default)]
    pub errors: Vec<CleanupError>,
    pub reclaimable_bytes: u64,
    pub freed_bytes: u64,
}

/// Options for gc.
#[derive(Debug, Clone)]
pub struct GcOptions {
    pub dry_run: bool,
    pub prune_orphans: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            prune_orphans: false,
        }
    }
}

/// Skip categories.
pub const GC_SKIP_VALID_LEASE: &str = "valid lease";
pub const GC_SKIP_IN_USE: &str = "in use";
pub const GC_SKIP_DISPOSABLE_BAR: &str = "not disposable";

/// The `tag` gc assigns to an orphaned candidate. An orphan has no live
/// repository to deregister the worktree from, so its cleanup is filesystem
/// only.
const GC_TAG_ORPHANED: &str = "stale/orphaned";

impl Pool {
    /// Reclaims stale worktrees (Go-style two-phase, strictly safe).
    pub fn gc(&self, opts: &GcOptions) -> Result<GcResult, PoolError> {
        // Snapshot under one lock: read + heal + write.
        let entries = crate::prune::with_pool_snapshot(self)?;

        // Each slot is resolved against its OWN repository (M-020); the fetch
        // + default-ref lookup is memoized per resolved root. Under an `--all`
        // sweep `self.root` is the pool directory's PARENT and not a
        // repository at all, so a single pool-wide root would leave every slot
        // permanently unverifiable and gc would reclaim nothing, forever.
        let mut resolver = crate::prune::PruneContextResolver::new(self);

        let mut result = GcResult {
            dry_run: opts.dry_run,
            ..Default::default()
        };
        let mut planned: Vec<(GcWorktree, crate::prune::PruneContext)> = Vec::new();
        let now = chrono::Utc::now();

        for wt in &entries {
            // An orphan has no live repository to resolve a ref in, and
            // `analyze_gc_candidate` answers from the orphan branch before it
            // ever reads the default ref.
            let context = if self.backing_repository_missing(&wt.path) {
                crate::prune::PruneContext {
                    repo_root: std::path::PathBuf::new(),
                    default_ref: None,
                    context_error: None,
                }
            } else {
                match resolver.context(wt) {
                    Ok(c) => c,
                    Err(reason) => {
                        result.skipped.push(GcSkipped {
                            name: wt.name.clone(),
                            path: wt.path.clone(),
                            category: crate::prune::PRUNE_SKIP_CANNOT_VERIFY.to_string(),
                            reason: format!("cannot verify worktree: {reason}"),
                        });
                        continue;
                    }
                }
            };
            let (candidate, skipped) = self.analyze_gc_candidate(
                wt,
                context.default_ref.as_deref(),
                opts,
                now,
            );
            if let Some(skip) = skipped {
                result.skipped.push(skip);
                continue;
            }
            if let Some(c) = candidate {
                result.reclaimable_bytes += c.bytes;
                result.candidates.push(c.clone());
                planned.push((c, context));
            }
        }

        if opts.dry_run || planned.is_empty() {
            return Ok(result);
        }

        let (reclaimed, errors) = self.execute_gc(&planned, opts)?;
        result.reclaimed = reclaimed.clone();
        result.errors = errors;
        result.freed_bytes = reclaimed.iter().map(|w| w.bytes).sum();
        Ok(result)
    }

    /// Classifies one worktree for gc: stale-lease, ordinary idle, orphan, or
    /// skip (valid lease / in use / not disposable).
    fn analyze_gc_candidate(
        &self,
        wt: &WorktreeEntry,
        default_ref: Option<&str>,
        opts: &GcOptions,
        now: chrono::DateTime<chrono::Utc>,
    ) -> (Option<GcWorktree>, Option<GcSkipped>) {
        let name = wt.name.clone();
        let path = wt.path.clone();

        // Destroying or dead-owner reservations are healed by the snapshot.
        if wt.destroying || crate::reservation::owner_alive(wt, &self.process) {
            return (None, None);
        }

        // VALID leases are NEVER candidates.
        if wt.leased && !wt.is_stale_lease(now) {
            return (
                None,
                Some(GcSkipped {
                    name,
                    path,
                    category: GC_SKIP_VALID_LEASE.to_string(),
                    reason: "lease is valid (not expired)".into(),
                }),
            );
        }

        // In-use: a live process (incl. an agent past its TTL) is never evicted.
        if self
            .process
            .is_worktree_in_use(Path::new(&path))
            .unwrap_or(true)
        {
            return (
                None,
                Some(GcSkipped {
                    name,
                    path,
                    category: PRUNE_SKIP_IN_USE.to_string(),
                    reason: PRUNE_SKIP_IN_USE.into(),
                }),
            );
        }

        // Orphan: backing repo git metadata missing -> skip unless --prune-orphans.
        if self.backing_repository_missing(&path) {
            if !opts.prune_orphans {
                return (
                    None,
                    Some(GcSkipped {
                        name,
                        path,
                        category: crate::prune::PRUNE_SKIP_ORPHANED.to_string(),
                        reason: "content could not be verified".into(),
                    }),
                );
            }
            let container = Path::new(&path).parent().unwrap_or(Path::new(&path));
            let bytes = dir_size(container).unwrap_or(0);
            return (
                Some(GcWorktree {
                    name,
                    path,
                    bytes,
                    tag: GC_TAG_ORPHANED.into(),
                    warning: "content could not be verified".into(),
                }),
                None,
            );
        }

        // Stale-lease worktrees need the full disposable bar: clean + merged.
        let tag = if wt.leased { "stale lease" } else { "stale" };

        // Dirty -> skip (not disposable).
        if self.git.is_dirty(Path::new(&path)).unwrap_or(true) {
            return (
                None,
                Some(GcSkipped {
                    name,
                    path,
                    category: crate::prune::PRUNE_SKIP_UNCOMMITTED.to_string(),
                    reason: "worktree has uncommitted changes".into(),
                }),
            );
        }

        // Merge check against the default ref.
        let Some(default_ref) = default_ref else {
            return (
                None,
                Some(GcSkipped {
                    name,
                    path,
                    category: GC_SKIP_DISPOSABLE_BAR.to_string(),
                    reason: "cannot verify default branch".into(),
                }),
            );
        };
        match self
            .git
            .is_head_merged_into_ref(Path::new(&path), default_ref)
        {
            Ok(true) => {}
            Ok(false) => {
                return (
                    None,
                    Some(GcSkipped {
                        name,
                        path,
                        category: crate::prune::PRUNE_SKIP_UNMERGED.to_string(),
                        reason: format!("HEAD not merged into {default_ref}"),
                    }),
                );
            }
            Err(_) => {
                return (
                    None,
                    Some(GcSkipped {
                        name,
                        path,
                        category: GC_SKIP_DISPOSABLE_BAR.to_string(),
                        reason: "cannot verify merge".into(),
                    }),
                );
            }
        }

        let container = Path::new(&path).parent().unwrap_or(Path::new(&path));
        let bytes = dir_size(container).unwrap_or(0);
        (
            Some(GcWorktree {
                name,
                path,
                bytes,
                tag: tag.to_string(),
                warning: String::new(),
            }),
            None,
        )
    }

    /// Executes gc via the shared two-phase engine.
    ///
    /// Returns reclaimed worktrees and any cleanup errors. A worktree whose
    /// physical cleanup fails is **retained** in state so it remains eligible
    /// for retry on the next gc run.
    fn execute_gc(
        &self,
        planned: &[(GcWorktree, crate::prune::PruneContext)],
        opts: &GcOptions,
    ) -> Result<(Vec<GcWorktree>, Vec<CleanupError>), PoolError> {
        // Phase 1: reserve Destroying + fresh owner.
        let reserved: Vec<(Reservation, GcWorktree, crate::prune::PruneContext)> =
            crate::pool::with_pool_lock(&self.dir, self.lock_timeout, || {
                let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
                heal_state(&mut state, |pid| self.process.started_at(pid));
                let mut reserved = Vec::new();
                for (w, context) in planned {
                    let Some(idx) = state.worktrees.iter().position(|e| e.path == w.path) else {
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
                            "reserve gc",
                            e.to_string(),
                            crate::git::GitErrorKind::Other,
                        ))
                    })?;
                    reserved.push((reservation, w.clone(), context.clone()));
                }
                state_file::write_state(&self.dir, &state)
                    .map_err(|e| PoolError::Io("writing state".into(), e))?;
                Ok::<_, PoolError>(reserved)
            })?;

        // Phase 2: RE-CLASSIFY + physical cleanup + state commit.
        //
        // gc's two-phase engine has the same shape as prune's, so it gets the
        // same treatment: the plan's verdict is not trusted, because the
        // `pre_destroy` hooks run with no lock held between the two phases and
        // a slot can go dirty, gain a process, or gain a valid lease in that
        // window. The full classification is re-run inside the deleting lock.
        //
        // Invariant: a worktree is removed from state **only** when cleanup
        // fully succeeds. On failure the entry is retained with its
        // reservation restored, so it is immediately retryable rather than
        // stuck behind a `destroying` flag naming a still-live process.
        crate::pool::with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let mut reclaimed = Vec::new();
            let mut removed = std::collections::HashSet::new();
            let mut errors = Vec::new();

            for (reservation, w, context) in &reserved {
                let Some(idx) = state
                    .worktrees
                    .iter()
                    .position(|e| e.path == reservation.worktree)
                else {
                    continue;
                };
                if !reservation.matches(&state.worktrees[idx]) {
                    continue; // re-acquired mid-hook; never remove
                }
                let path = state.worktrees[idx].path.clone();

                if self.final_gc_safety_check(&state.worktrees[idx], context, opts) {
                    reservation.restore_original(&mut state.worktrees[idx]);
                    continue;
                }

                let repo = crate::git::GitRepo {
                    // The repository that OWNS this worktree, not whatever
                    // repository the pool happens to have been opened against.
                    common_dir: context.repo_root.clone(),
                    worktree: None,
                };

                // A: git worktree remove. An orphan has no live repository to
                // deregister from, so it takes the filesystem route alone.
                let mut cleanup_ok = true;
                if w.tag != GC_TAG_ORPHANED
                    && let Err(e) = self.git.worktree_remove(&repo, Path::new(&path))
                {
                    errors.push(CleanupError {
                        name: w.name.clone(),
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
                                name: w.name.clone(),
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
                    reclaimed.push(w.clone());
                } else {
                    reservation.restore_original(&mut state.worktrees[idx]);
                }
            }

            state.worktrees.retain(|e| !removed.contains(&e.path));
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".into(), e))?;
            Ok::<_, PoolError>((reclaimed, errors))
        })
    }

    /// The last word on disposability, run inside the deleting lock. `true`
    /// means the slot must be left alone.
    ///
    /// Every condition the plan checked is checked again against the state
    /// just read: a valid lease taken during the unlocked hook window, a
    /// process that walked in, dirtiness, and merge state. Fails closed on
    /// ambiguity — an `is_dirty` that errors counts as dirty.
    fn final_gc_safety_check(
        &self,
        wt: &WorktreeEntry,
        context: &crate::prune::PruneContext,
        opts: &GcOptions,
    ) -> bool {
        let now = chrono::Utc::now();
        // A valid lease taken between plan and delete makes the slot off limits
        // — this is the whole point of the two-phase contract.
        if wt.leased && !wt.is_stale_lease(now) {
            return true;
        }
        if self
            .process
            .is_worktree_in_use(Path::new(&wt.path))
            .unwrap_or(true)
        {
            return true;
        }
        if self.backing_repository_missing(&wt.path) {
            return !opts.prune_orphans;
        }
        if self.git.is_dirty(Path::new(&wt.path)).unwrap_or(true) {
            return true;
        }
        let Some(default_ref) = context.default_ref.as_deref() else {
            return true;
        };
        !matches!(
            self.git
                .is_head_merged_into_ref(Path::new(&wt.path), default_ref),
            Ok(true)
        )
    }
}

/// Recursively measures a directory's size.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ZERO_TIME;
    use chrono::{DateTime, Utc};

    #[test]
    fn gc_options_default_dry_run() {
        assert!(GcOptions::default().dry_run);
        assert!(!GcOptions::default().prune_orphans);
    }

    #[test]
    fn skip_categories_stable() {
        assert_eq!(GC_SKIP_VALID_LEASE, "valid lease");
        assert_eq!(GC_SKIP_IN_USE, "in use");
    }

    #[test]
    fn stale_lease_boundaries() {
        let now: DateTime<Utc> = DateTime::parse_from_rfc3339("2026-08-14T12:00:00Z")
            .unwrap()
            .into();
        let mut wt = WorktreeEntry {
            leased: true,
            expires_at: DateTime::parse_from_rfc3339("2026-08-14T12:30:00Z")
                .unwrap()
                .into(),
            ..Default::default()
        };
        assert!(wt.is_valid_lease(now));
        assert!(!wt.is_stale_lease(now));
        // Exactly at expiry: stale.
        assert!(
            wt.is_stale_lease(
                DateTime::parse_from_rfc3339("2026-08-14T12:30:00Z")
                    .unwrap()
                    .into()
            )
        );
        // Permanent lease (zero expires_at): never stale.
        wt.expires_at = ZERO_TIME;
        assert!(wt.is_valid_lease(now));
        assert!(!wt.is_stale_lease(now));
    }

    // ─── Integration tests for cleanup error hardening ──────────────────

    use crate::config::TreehouseConfig;
    use crate::pool::{OpenOptions, Pool};

    /// Creates a real git repo + pool with one worktree that has an expired lease.
    /// Returns (pool, worktree_path, tmp_home_guard, tmp_repo_guard).
    fn setup_gc_test() -> (
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

        // Acquire a worktree with a TTL lease.
        let acquired = pool
            .get(&crate::pool::AcquireOptions {
                branch: Some("main".to_string()),
                lease: Some(crate::pool::LeaseAcquireOptions {
                    holder: "test-agent".into(),
                    ttl: Some(chrono::Duration::hours(1)),
                }),
                ..Default::default()
            })
            .unwrap();
        let wt_path = acquired.path.clone();

        // Force the lease to be expired by writing expires_at into the state.
        {
            let state_path = crate::state::State::state_file_path(pool.pool_dir());
            let mut state: crate::state::State =
                serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
            let wt = state
                .worktrees
                .iter_mut()
                .find(|w| w.path == wt_path)
                .unwrap();
            wt.expires_at = chrono::Utc::now() - chrono::Duration::hours(1);
            crate::state_file::write_state(pool.pool_dir(), &state).unwrap();
        }

        (pool, wt_path, fake_home, dir)
    }

    #[test]
    fn gc_retains_state_on_removal_failure() {
        let (pool, wt_path, _home, _repo) = setup_gc_test();

        // Make the worktree directory read-only. On Unix, git worktree remove
        // --force succeeds (unlinks .git) but remove_dir_all fails on the
        // readonly dir. On Windows, both phases may fail because the readonly
        // attribute prevents file deletion inside the dir.
        make_readonly(&wt_path);

        let result = pool
            .gc(&GcOptions {
                dry_run: false,
                prune_orphans: false,
            })
            .unwrap();

        // Critical invariant: worktree must NOT be reclaimed on any failure.
        assert!(
            result.reclaimed.is_empty(),
            "worktree must not be reclaimed when physical cleanup fails"
        );
        // At least one error must be recorded (which phase depends on platform).
        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].phase == "git_worktree_remove"
                || result.errors[0].phase == "filesystem_remove",
            "error phase must be git_worktree_remove or filesystem_remove, got: {}",
            result.errors[0].phase
        );

        // State must retain the entry (eligible for retry).
        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        let wt = state.worktrees.iter().find(|w| w.path == wt_path).unwrap();
        assert!(wt.leased, "entry must remain leased after failed cleanup");
    }

    #[test]
    fn gc_retry_succeeds_after_fixing_error() {
        let (pool, wt_path, _home, _repo) = setup_gc_test();

        // Make read-only → first gc fails.
        make_readonly(&wt_path);

        let first = pool
            .gc(&GcOptions {
                dry_run: false,
                prune_orphans: false,
            })
            .unwrap();
        assert!(!first.errors.is_empty(), "first gc must record an error");
        assert!(first.reclaimed.is_empty());

        // State retains the entry (eligible for retry).
        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        assert!(
            state.worktrees.iter().any(|w| w.path == wt_path),
            "entry must remain in state after failed cleanup"
        );

        // Fix permissions → second gc succeeds.
        make_writable(&wt_path);

        let second = pool
            .gc(&GcOptions {
                dry_run: false,
                prune_orphans: false,
            })
            .unwrap();
        // After fixing the underlying error, retry must succeed.
        assert!(
            second.errors.is_empty(),
            "retry must not produce errors after fixing the issue, got: {:?}",
            second.errors
        );
    }

    #[test]
    fn gc_happy_path_removes_state_entry() {
        let (pool, wt_path, _home, _repo) = setup_gc_test();

        let result = pool
            .gc(&GcOptions {
                dry_run: false,
                prune_orphans: false,
            })
            .unwrap();

        assert_eq!(result.reclaimed.len(), 1);
        assert!(result.errors.is_empty());

        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        assert!(
            state.worktrees.iter().all(|w| w.path != wt_path),
            "state must not contain the reclaimed worktree"
        );
    }

    #[test]
    fn gc_not_found_directory_is_handled_gracefully() {
        let (pool, wt_path, _home, _repo) = setup_gc_test();

        // Pre-remove the worktree directory.
        std::fs::remove_dir_all(&wt_path).unwrap();
        assert!(!wt_path.exists());

        // heal_state drops entries with missing paths before gc analyzes.
        // gc should complete without panicking or producing errors.
        let result = pool
            .gc(&GcOptions {
                dry_run: false,
                prune_orphans: false,
            })
            .unwrap();

        // Entry was already removed by heal_state — nothing to reclaim.
        assert!(
            result.reclaimed.is_empty(),
            "missing-directory worktree must not be reclaimed"
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

    // ─── M-020: gc resolves each slot's repository from its own path ──────

    /// M-020: `gc --all` opens the pool BY DIRECTORY, so `self.root` is the
    /// pool directory's parent — not a repository. One pool-wide root therefore
    /// failed to resolve for every slot, `default_ref` came back unresolved,
    /// and every candidate was skipped as unverifiable: `gc --all` reclaimed
    /// nothing, forever. Each slot must be resolved from its OWN path.
    #[test]
    fn gc_under_an_all_sweep_resolves_each_slots_own_repository() {
        let (pool, wt_path, _home, _repo) = setup_gc_test();
        let swept = Pool::open_at(pool.pool_dir(), &OpenOptions::default()).unwrap();
        assert_eq!(
            swept.root,
            pool.pool_dir().parent().unwrap(),
            "an --all sweep has no repository as its root"
        );

        let result = swept
            .gc(&GcOptions {
                dry_run: false,
                prune_orphans: false,
            })
            .unwrap();

        assert!(
            result.skipped.is_empty(),
            "no slot should be unverifiable under a per-slot root, got: {:?}",
            result.skipped
        );
        assert_eq!(
            result.reclaimed.len(),
            1,
            "the expired-lease worktree must actually be reclaimed"
        );
        assert!(!wt_path.exists(), "the worktree directory must be gone");
    }

    /// The repository passed to `git worktree remove` must be the one that
    /// OWNS the worktree. A pool-wide root deregisters the wrong clone's
    /// bookkeeping; this pins the per-slot context that reaches the executor.
    #[test]
    fn gc_context_points_at_the_owning_repository() {
        let (pool, wt_path, _home, _repo) = setup_gc_test();
        let mut resolver = crate::prune::PruneContextResolver::new(&pool);
        let state = crate::state::State::read_state(pool.pool_dir()).unwrap();
        let wt = state.worktrees.iter().find(|w| w.path == wt_path).unwrap();
        let context = resolver.context(wt).unwrap();
        assert!(
            context.repo_root.ends_with("repo"),
            "must resolve the worktree's own repository, got: {:?}",
            context.repo_root
        );
        assert!(!context.repo_root.starts_with(&pool.root));
    }
}
