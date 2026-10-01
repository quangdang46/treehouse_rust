//! Safe-by-default worktree destruction (two-phase reservation).
//!
//! Port of Go `internal/pool/destroy.go` (the v2.0.0 safety contract):
//! - Dry-run unless `--yes`.
//! - Narrow explicit targets; NO cross-pool/global destroy.
//! - Each risk class its own `--include-*` opt-in.
//! - A leased worktree is NEVER removed by `--all`, only by an exact named
//!   path + `--include-leased`.
//! - Two-phase: reserve `Destroying=true` + fresh owner under the lock, run
//!   `pre_destroy` hooks outside, re-verify `sameDestroyReservation` under a
//!   fresh lock, then delete. Any skip restores the original owner
//!   reservation; a worktree re-acquired mid-hook is never deleted.

use std::path::{Path, PathBuf};

use crate::pool::{Pool, PoolError, with_pool_lock};
use crate::process::ProcessInfo;
use crate::reservation::Reservation;
use crate::state::{State, WorktreeEntry, heal_state};
use crate::state_file;
use crate::worktree::{
    ClassCheckResults, ClassSet, DestroyClass, DestroyOptions as ClassifyOptions,
};

/// How long destruction waits for lingering processes after SIGTERM before
/// escalating (matches `get`/`return`).
pub const DESTROY_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

/// Destroy options (CLI surface). Dry-run is the default (safe by default).
#[derive(Debug, Clone)]
pub struct DestroyOptions {
    pub dry_run: bool,
    pub include_unlanded: bool,
    pub include_in_use: bool,
    pub include_leased: bool,
    pub pre_destroy: Vec<String>,
}

impl Default for DestroyOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            include_unlanded: false,
            include_in_use: false,
            include_leased: false,
            pre_destroy: Vec::new(),
        }
    }
}

/// A worktree planned for destruction.
#[derive(Debug, Clone)]
pub struct DestroyTarget {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub class: DestroyClass,
    pub classes: ClassSet,
    pub processes: Vec<ProcessInfo>,
    pub detail: String,
}

/// A worktree skipped by destroy.
#[derive(Debug, Clone)]
pub struct DestroySkip {
    pub target: DestroyTarget,
    pub needed_flags: Vec<&'static str>,
    pub leased_bulk: bool,
    pub detail: String,
}

/// The result of a destroy operation.
#[derive(Debug, Clone, Default)]
pub struct DestroyResult {
    pub dry_run: bool,
    pub all: bool,
    pub scope: String,
    pub planned: Vec<DestroyTarget>,
    pub destroyed: Vec<DestroyTarget>,
    pub skipped: Vec<DestroySkip>,
    pub planned_bytes: u64,
    pub freed_bytes: u64,
}

/// Destroy targets: a single named path, or all worktrees in the pool.
#[derive(Debug, Clone)]
pub enum DestroyTargetSpec {
    /// `destroy <path>` — a single named worktree (allow_leased = true).
    Single(String),
    /// `destroy <pool> --all` — every worktree in the pool (allow_leased = false).
    All,
}

impl Pool {
    /// Destroys worktrees: single named path or all in the pool.
    pub fn destroy(
        &self,
        spec: &DestroyTargetSpec,
        opts: &DestroyOptions,
    ) -> Result<DestroyResult, PoolError> {
        let allow_leased = matches!(spec, DestroyTargetSpec::Single(_));
        let all = matches!(spec, DestroyTargetSpec::All);

        // Each slot resolves against its OWN repository (M-020). A pool is keyed
        // by ORIGIN URL, so two clones share one pool directory while every slot
        // still belongs to exactly one physical clone. Resolving a single root
        // for the whole pool runs `git worktree remove` from the wrong
        // repository, and under an `--all` sweep it is worse than wrong: the pool
        // is opened by directory, so `self.root` is the pool directory's PARENT
        // and not a repository at all — nothing could ever be reclaimed.
        //
        // This is the SAME resolver prune and gc use (Go
        // `worktreePruneContextResolver`, prune.go:401-418, reached from destroy
        // via `planAndDestroy`), so the three commands agree on "unmerged" and
        // on which repository owns a slot. Go's rule is stated at
        // `resolvePoolRepoRoot` (destroy.go:611-619): callers "must resolve
        // every slot independently and must not apply one slot's root to
        // another".
        let mut resolver = crate::prune::PruneContextResolver::new(self);

        // Build the target list: all managed worktrees, or just the named one.
        let state = State::read_state(&self.dir).map_err(PoolError::State)?;
        let targets: Vec<WorktreeEntry> = match spec {
            DestroyTargetSpec::All => state.worktrees.clone(),
            DestroyTargetSpec::Single(path) => {
                let mut found = None;
                for wt in &state.worktrees {
                    if wt.path == *path {
                        found = Some(wt.clone());
                        break;
                    }
                }
                match found {
                    Some(wt) => vec![wt],
                    None => return Err(PoolError::NotFound(path.clone())),
                }
            }
        };

        let mut result = DestroyResult {
            dry_run: opts.dry_run,
            all,
            scope: self.dir.to_string_lossy().into_owned(),
            ..Default::default()
        };

        // Classify + gate. Each target carries the context resolved FROM ITS
        // OWN PATH, and `execute_destroy` reuses that per-slot root for
        // `worktree_remove` rather than re-deriving one pool-wide value.
        let mut removable = Vec::new();
        for wt in &targets {
            // A markerless slot (Go `resolveDestroyContext`, destroy.go:252-258)
            // has no context at all: dispatching git on it would resolve the
            // repository ENCLOSING the pool. An empty default ref makes it
            // classify as Unverified, which is a skip, never a deletion.
            let context = if crate::pool::slot_backend_name(Path::new(&wt.path)).is_empty() {
                crate::prune::PruneContext {
                    repo_root: PathBuf::new(),
                    default_ref: None,
                    context_error: None,
                }
            } else {
                // A resolution failure leaves the default ref empty, which
                // `classify_for_destroy` reports as Unverified rather than
                // disposable (Go destroy.go:255-256).
                resolver.context(wt).unwrap_or(crate::prune::PruneContext {
                    repo_root: PathBuf::new(),
                    default_ref: None,
                    context_error: Some("could not resolve the worktree's repository".into()),
                })
            };
            let target =
                self.classify_for_destroy(wt, context.default_ref.as_deref().unwrap_or(""));
            let mut t = target;
            t.bytes = self.measure_size(&t.path);
            match self.allows(&t, allow_leased, opts) {
                Ok(()) => removable.push((t, context)),
                Err(skip) => result.skipped.push(skip),
            }
        }
        result.planned = removable.iter().map(|(t, _)| t.clone()).collect();
        result.planned_bytes = removable.iter().map(|(t, _)| t.bytes).sum();

        if opts.dry_run {
            return Ok(result);
        }

        // Execute: two-phase.
        let (destroyed, exec_skips) = self.execute_destroy(&removable, allow_leased, opts)?;
        result.destroyed = destroyed.clone();
        result.freed_bytes = destroyed.iter().map(|t| t.bytes).sum();
        result.skipped.extend(exec_skips);
        Ok(result)
    }

    /// Classifies a worktree for destroy using live state (Go classifyForDestroy).
    fn classify_for_destroy(&self, wt: &WorktreeEntry, default_ref: &str) -> DestroyTarget {
        // Scan ONCE and derive both outputs from it. Scanning twice would let
        // `procs` and `proc_scan_error` disagree when a process starts between
        // the two sweeps (each `find_in_worktree` re-enumerates), and it would
        // pay for two full refreshes per classification.
        let scan = self.process.find_in_worktree(Path::new(&wt.path));
        let proc_scan_error = scan.is_err();
        let procs = scan.unwrap_or_default();
        let backing_missing = self.backing_repository_missing(&wt.path);
        let dirty = self.git.is_dirty(Path::new(&wt.path)).ok();
        let merged = if default_ref.is_empty() {
            None
        } else {
            self.git
                .is_head_merged_into_ref(Path::new(&wt.path), default_ref)
                .ok()
        };
        let owner_alive = crate::reservation::owner_alive(wt, &self.process);

        let classified = crate::worktree::classify_for_destroy(
            &wt.name,
            &wt.path,
            wt.leased,
            &wt.lease_holder,
            owner_alive,
            &ClassCheckResults {
                processes: procs.clone(),
                backing_repo_missing: Some(backing_missing),
                dirty,
                merged,
                default_ref: default_ref.to_string(),
                proc_scan_error,
            },
        );

        DestroyTarget {
            name: classified.name,
            path: classified.path,
            bytes: 0,
            class: classified.class,
            classes: classified.classes,
            processes: procs,
            detail: classified.detail,
        }
    }

    /// Whether the backing repo's git metadata is missing (Go
    /// `backingRepositoryMissing`): the worktree's `.git` file points to a
    /// gitdir that no longer exists.
    pub(crate) fn backing_repository_missing(&self, path: &str) -> bool {
        let git_file = Path::new(path).join(".git");
        let Ok(contents) = std::fs::read_to_string(&git_file) else {
            return false;
        };
        if let Some(gitdir) = contents.strip_prefix("gitdir:") {
            let gitdir = gitdir.trim();
            let gitdir_path = Path::new(path).join(gitdir);
            !gitdir_path.exists()
        } else {
            false
        }
    }

    /// The on-disk size of a worktree's numbered container.
    fn measure_size(&self, path: &str) -> u64 {
        let container = Path::new(path).parent().unwrap_or(Path::new(path));
        dir_size(container)
    }

    /// Whether `opts` authorize removing `target`. Returns a skip on failure.
    // One-shot CLI helper; the skip is built once per target, not in a hot loop.
    #[allow(clippy::result_large_err)]
    fn allows(
        &self,
        target: &DestroyTarget,
        allow_leased: bool,
        opts: &DestroyOptions,
    ) -> Result<(), DestroySkip> {
        let class_opts = ClassifyOptions {
            allow_leased,
            include_leased: opts.include_leased,
            include_in_use: opts.include_in_use,
            include_unlanded: opts.include_unlanded,
        };
        // Leased is NEVER removable via bulk --all.
        if !allow_leased && target.classes.contains(DestroyClass::Leased) {
            return Err(DestroySkip {
                target: target.clone(),
                needed_flags: vec![],
                leased_bulk: true,
                detail: "leased worktree is never removed by --all".into(),
            });
        }
        let missing = class_opts.missing_flags(&target.classes);
        if missing.is_empty() {
            Ok(())
        } else {
            Err(DestroySkip {
                target: target.clone(),
                needed_flags: missing,
                leased_bulk: false,
                detail: "missing required flags".into(),
            })
        }
    }

    /// Executes the two-phase destroy (Go `executeDestroy`).
    ///
    /// Each planned target arrives with the context resolved from its OWN path,
    /// and that per-slot `repo_root` is what `worktree_remove` runs against.
    fn execute_destroy(
        &self,
        removable: &[(DestroyTarget, crate::prune::PruneContext)],
        allow_leased: bool,
        opts: &DestroyOptions,
    ) -> Result<(Vec<DestroyTarget>, Vec<DestroySkip>), PoolError> {
        if removable.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let planned_by_path: std::collections::HashMap<
            String,
            (DestroyTarget, crate::prune::PruneContext),
        > = removable
            .iter()
            .map(|(t, ctx)| (t.path.clone(), (t.clone(), ctx.clone())))
            .collect();

        // Phase 1: reserve Destroying + fresh owner under the lock.
        //
        // Returns the skips alongside the reservations: a target that flips to
        // non-removable between the plan phase and here (concurrent destroy,
        // or it became dirty/in-use/leased) is neither destroyed nor reported
        // unless we carry its skip out. Go appends both phases' skips into
        // `result.Skipped` (destroy.go:378, :383).
        let (reserved, mut skips) = with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            heal_state(&mut state, |pid| self.process.started_at(pid));
            let mut reserved = Vec::new();
            let mut skips = Vec::new();
            for i in 0..state.worktrees.len() {
                let Some((_, context)) = planned_by_path.get(&state.worktrees[i].path) else {
                    continue;
                };
                let context = context.clone();
                let default_ref = context.default_ref.as_deref().unwrap_or("");
                let current = self.classify_for_destroy(&state.worktrees[i], default_ref);
                if state.worktrees[i].destroying
                    && crate::reservation::owner_alive(&state.worktrees[i], &self.process)
                {
                    skips.push(DestroySkip {
                        target: current,
                        needed_flags: vec![],
                        leased_bulk: false,
                        detail: "reserved by another destroy".into(),
                    });
                    continue;
                }
                if let Err(skip) = self.allows(&current, allow_leased, opts) {
                    skips.push(skip);
                    continue;
                }
                let path = state.worktrees[i].path.clone();
                let reservation =
                    Reservation::reserve_destroy(&path, &mut state.worktrees[i], &self.process)
                        .map_err(|e| {
                            PoolError::Git(crate::git::GitError::new(
                                "reserve destroy",
                                e.to_string(),
                                crate::git::GitErrorKind::Other,
                            ))
                        })?;
                reserved.push((reservation, current, context));
            }
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".into(), e))?;
            Ok::<_, PoolError>((reserved, skips))
        })?;

        // Hooks OUTSIDE all locks (non-fatal).
        if !opts.pre_destroy.is_empty() {
            for (reservation, _, _) in &reserved {
                let mut out = std::io::stdout();
                let mut err = std::io::stderr();
                crate::hooks::run(
                    &opts.pre_destroy,
                    Path::new(&reservation.worktree),
                    &mut out,
                    &mut err,
                );
            }
        }

        // Phase 2: re-verify + delete under a fresh lock.
        let (destroyed, exec_skips) = with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let mut destroyed = Vec::new();
            let mut skips = Vec::new();
            let mut removed_paths = std::collections::HashSet::new();

            for (reservation, planned, context) in &reserved {
                let idx = state
                    .worktrees
                    .iter()
                    .position(|w| w.path == reservation.worktree);
                let Some(idx) = idx else { continue };
                if !reservation.matches(&state.worktrees[idx]) {
                    skips.push(DestroySkip {
                        target: planned.clone(),
                        needed_flags: vec![],
                        leased_bulk: false,
                        detail: "re-acquired during pre-destroy hook".into(),
                    });
                    continue;
                }

                let path = state.worktrees[idx].path.clone();
                // Re-classify with the ORIGINAL reservation restored (so a
                // worktree that's still disposable is removed, but one that
                // became dirty/in-use is skipped).
                let mut current_entry = state.worktrees[idx].clone();
                reservation.restore_original(&mut current_entry);
                let mut current = self.classify_for_destroy(
                    &current_entry,
                    context.default_ref.as_deref().unwrap_or(""),
                );
                current.bytes = planned.bytes;
                if self.allows(&current, allow_leased, opts).is_err() {
                    reservation.restore_original(&mut state.worktrees[idx]);
                    skips.push(DestroySkip {
                        target: current,
                        needed_flags: vec![],
                        leased_bulk: false,
                        detail: "re-classified as not removable".into(),
                    });
                    continue;
                }

                // If in-use and authorized, terminate processes.
                if current.classes.contains(DestroyClass::InUse) && opts.include_in_use {
                    match self
                        .process
                        .terminate_with_grace(Path::new(&path), DESTROY_GRACE_PERIOD)
                    {
                        Ok(_) => {}
                        Err(e) => {
                            reservation.restore_original(&mut state.worktrees[idx]);
                            skips.push(DestroySkip {
                                target: current.clone(),
                                needed_flags: vec![],
                                leased_bulk: false,
                                detail: format!("could not terminate worktree processes: {e}"),
                            });
                            continue;
                        }
                    }
                    // Re-scan with a LIVE table (`find_in_worktree`
                    // re-enumerates). Reading a pre-kill snapshot here would
                    // still list the pids we just terminated and make
                    // `--include-in-use` skip forever, so the flag could never
                    // remove a worktree that actually had processes.
                    //
                    // A scan ERROR fails CLOSED: it restores the reservation
                    // and skips rather than continuing to
                    // `worktree_remove` + `remove_dir_all` on a worktree whose
                    // liveness we could not establish (Go
                    // destroy.go:504-510).
                    let survivors = match self.process.find_in_worktree(Path::new(&path)) {
                        Ok(survivors) => survivors,
                        Err(e) => {
                            reservation.restore_original(&mut state.worktrees[idx]);
                            skips.push(DestroySkip {
                                target: current,
                                needed_flags: vec![],
                                leased_bulk: false,
                                detail: format!("could not verify worktree processes stopped: {e}"),
                            });
                            continue;
                        }
                    };
                    if !survivors.is_empty() {
                        reservation.restore_original(&mut state.worktrees[idx]);
                        skips.push(DestroySkip {
                            target: current,
                            needed_flags: vec![],
                            leased_bulk: false,
                            detail: "worktree processes still running after termination".into(),
                        });
                        continue;
                    }
                }

                // Remove the worktree from the repository that OWNS it (M-020).
                //
                // `context.repo_root` was resolved from THIS slot's path in the
                // plan phase. Using a pool-wide root here is what made a
                // shared pool's removals run from the wrong repository, and made
                // `destroy --all` (where `self.root` is not a repository at all)
                // reclaim nothing while recording a CleanupError per slot
                // forever. Go reaches the same per-slot root through
                // `resolvePoolRepoRoot(wt)` at destroy.go:588.
                let remove_root = context.repo_root.clone();
                if remove_root.as_os_str().is_empty() {
                    // No live repository owns this slot, so there is nothing to
                    // deregister it from: git has no registration to drop.
                    // Go takes the filesystem route alone for the same slots
                    // (destroy.go:576-590).
                    reservation.restore_original(&mut state.worktrees[idx]);
                    skips.push(DestroySkip {
                        target: current,
                        needed_flags: vec![],
                        leased_bulk: false,
                        detail: "cannot resolve the repository owning this worktree".into(),
                    });
                    continue;
                }
                match self.git.worktree_remove(
                    &crate::git::GitRepo {
                        common_dir: remove_root,
                        worktree: None,
                    },
                    Path::new(&path),
                ) {
                    Ok(()) => {}
                    Err(e) => {
                        reservation.restore_original(&mut state.worktrees[idx]);
                        skips.push(DestroySkip {
                            target: current,
                            needed_flags: vec![],
                            leased_bulk: false,
                            detail: e.to_string(),
                        });
                        continue;
                    }
                }
                let _ = remove_dir_all_guarded(Path::new(&path));
                removed_paths.insert(path.clone());
                destroyed.push(current);
            }

            // Drop removed entries from state.
            state.worktrees.retain(|w| !removed_paths.contains(&w.path));
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".into(), e))?;
            Ok::<_, PoolError>((destroyed, skips))
        })?;

        // Both phases' skips reach the caller: a phase-1 skip means the target
        // was never even attempted, so reporting only phase 2 would make it
        // vanish from the output — and `destroy <path> --yes` would exit 0
        // having destroyed nothing.
        skips.extend(exec_skips);
        Ok((destroyed, skips))
    }
}

/// Recursively removes a directory, refusing unsafe roots (Go
/// `removable_worktree_container`). Returns whether removal succeeded.
fn remove_dir_all_guarded(path: &Path) -> bool {
    // Refuse to remove a filesystem root or home.
    let p = path.to_string_lossy();
    if p.is_empty() || p == "/" || p == "\\" || p.ends_with(":\\") {
        return false;
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// Recursively measures a directory's total size in bytes.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    total += dir_size(&path);
                } else {
                    total += meta.len();
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destroy_options_default_dry_run() {
        let opts = DestroyOptions::default();
        assert!(opts.dry_run);
        assert!(!opts.include_unlanded);
        assert!(!opts.include_in_use);
        assert!(!opts.include_leased);
    }

    #[test]
    fn remove_dir_all_guarded_refuses_roots() {
        assert!(!remove_dir_all_guarded(Path::new("")));
        assert!(!remove_dir_all_guarded(Path::new("/")));
        assert!(!remove_dir_all_guarded(Path::new("C:\\")));
        // A normal temp dir is removable.
        let dir = tempfile::tempdir().unwrap();
        assert!(remove_dir_all_guarded(dir.path()));
    }

    #[test]
    fn dir_size_measures_nested() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), vec![1u8; 100]).unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), vec![2u8; 50]).unwrap();
        assert_eq!(dir_size(dir.path()), 150);
    }

    #[test]
    fn backing_repository_missing_detects_dead_gitdir() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        // Points to a gitdir that doesn't exist => missing.
        std::fs::write(wt.join(".git"), "gitdir: ../../gone.git\n").unwrap();
        let pool = Pool {
            root: dir.path().to_path_buf(),
            dir: dir.path().join("pool"),
            git: std::sync::Arc::new(crate::git::ShellGitBackend::discover().unwrap()),
            process: std::sync::Arc::new(crate::process::ProcessTable::new()),
            config: crate::config::TreehouseConfig::default_config(),
            lock_timeout: std::time::Duration::from_secs(2),
            env: std::sync::Arc::new(crate::env::DefaultEnv),
        };
        assert!(pool.backing_repository_missing(&wt.to_string_lossy()));

        // Existing gitdir => not missing.
        std::fs::create_dir_all(dir.path().join("real.git")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../real.git\n").unwrap();
        assert!(!pool.backing_repository_missing(&wt.to_string_lossy()));

        // Not a linked worktree (no .git file) => not missing.
        std::fs::remove_file(wt.join(".git")).unwrap();
        assert!(!pool.backing_repository_missing(&wt.to_string_lossy()));
    }

    /// Builds a real git repo with one commit on `main`, opens a pool against
    /// it under a temp HOME, and acquires one worktree.
    ///
    /// Returns the pool and the acquired path. The two temp dirs are returned
    /// so they outlive the pool: the pool dir lives under `home`, and the
    /// worktree's git metadata points into `repo`. Real git is required (the
    /// destroy path runs `git worktree remove`), matching the existing pool
    /// integration test.
    fn real_pool_with_one_worktree() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Pool,
        std::path::PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let init = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .expect("git must be installed");
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        let git_in_repo = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git must be installed");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git_in_repo(&["config", "user.email", "t@t.com"]);
        git_in_repo(&["config", "user.name", "T"]);
        std::fs::write(repo.join("README.md"), b"hi\n").unwrap();
        git_in_repo(&["add", "."]);
        git_in_repo(&["commit", "-m", "init"]);

        let home = tempfile::tempdir().unwrap();
        let opts = crate::pool::OpenOptions {
            config: crate::config::TreehouseConfig {
                root: Some(home.path().to_str().unwrap().to_string()),
                ..crate::config::TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        let pool = Pool::open(&repo, None, &opts).unwrap();
        let acquired = pool
            .get(&crate::pool::AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        let path = acquired.path.clone();
        pool.release(&path.to_string_lossy()).unwrap();
        // Hand both temp dirs back so the caller keeps them alive (and so they
        // are actually cleaned up when the caller is done).
        (dir, home, pool, path)
    }

    /// M-004: `destroy --include-in-use` must actually remove a worktree that
    /// had a live process.
    ///
    /// Before the fix the post-kill survivor re-scan read the same frozen
    /// snapshot that still listed the just-SIGKILLed pid, so `is_empty()` was
    /// never true and destroy ALWAYS skipped with "worktree processes still
    /// running after termination" — the opt-in flag was a silent no-op.
    #[test]
    fn destroy_include_in_use_removes_a_worktree_that_had_a_process() {
        let (_repo, _home, pool, path) = real_pool_with_one_worktree();
        assert!(path.exists(), "worktree exists before destroy");

        // Start a process INSIDE the worktree, after the pool (and its process
        // table) were built — the ordering that made the old scan blind.
        let mut cmd = std::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" });
        if cfg!(windows) {
            cmd.args(["-n", "60", "127.0.0.1"]);
        } else {
            cmd.arg("60");
        }
        let mut child = cmd
            .current_dir(&path)
            .spawn()
            .expect("spawn idle child in the worktree");

        let result = pool
            .destroy(
                &DestroyTargetSpec::All,
                &DestroyOptions {
                    dry_run: false,
                    include_in_use: true,
                    include_unlanded: true,
                    include_leased: false,
                    pre_destroy: vec![],
                },
            )
            .expect("destroy must not error");

        // The child is terminated by destroy, so reap it to avoid a zombie.
        let _ = child.wait();

        assert_eq!(
            result.destroyed.len(),
            1,
            "--include-in-use must destroy the worktree; skips were: {:?}",
            result
                .skipped
                .iter()
                .map(|s| s.detail.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            !result
                .skipped
                .iter()
                .any(|s| s.detail.contains("still running after termination")),
            "the survivor check must not trip on a process that was actually killed"
        );
        assert!(!path.exists(), "the worktree directory must be gone");
    }

    /// M-011: a target skipped in phase 1 must still be reported.
    ///
    /// The phase-1 lock closure used to build `skips` and return only
    /// `reserved`, so the vector never escaped; phase 2 iterates `reserved`
    /// alone and the skip was dropped on the floor. A destroy that lost that
    /// race then printed "Destroyed 0 worktree(s)" and exited 0.
    #[test]
    fn phase_one_skip_is_reported_in_the_result() {
        let (_repo, _home, pool, path) = real_pool_with_one_worktree();

        // Simulate a concurrent destroy winning the race: mark the worktree
        // Destroying with a LIVE owner (this process), which is exactly the
        // condition phase 1 checks before reserving.
        let mut state = State::read_state(&pool.dir).unwrap();
        let me = std::process::id() as i32;
        let my_started = pool
            .process
            .started_at(me)
            .expect("own start time resolves");
        let wt = state
            .worktrees
            .iter_mut()
            .find(|w| w.path == path.to_string_lossy())
            .expect("worktree in state");
        wt.destroying = true;
        wt.owner_pid = me;
        wt.owner_started_at = my_started;
        crate::state_file::write_state(&pool.dir, &state).unwrap();

        let result = pool
            .destroy(
                &DestroyTargetSpec::All,
                &DestroyOptions {
                    dry_run: false,
                    include_in_use: true,
                    include_unlanded: true,
                    include_leased: true,
                    pre_destroy: vec![],
                },
            )
            .expect("destroy must not error");

        assert!(
            result.destroyed.is_empty(),
            "a worktree reserved by another destroy must not be removed"
        );
        assert!(
            result
                .skipped
                .iter()
                .any(|s| s.detail == "reserved by another destroy"),
            "the phase-1 skip must reach the caller; got {:?}",
            result
                .skipped
                .iter()
                .map(|s| s.detail.as_str())
                .collect::<Vec<_>>()
        );
        assert!(path.exists(), "the reserved worktree must be left on disk");
    }

    /// Builds a real repo with a commit, then a pool opened BY DIRECTORY over
    /// the same slot the repo-relative open produced.
    ///
    /// This is the `destroy --all` / `prune --all` shape (Go feature #146): the
    /// pool is keyed by origin, so `--all` opens it by directory and `self.root`
    /// becomes the pool directory's PARENT — a directory that is NOT a
    /// repository. Returns the pool plus the temp dirs that keep everything
    /// alive.
    fn pool_opened_by_directory() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        std::path::PathBuf,
        Pool,
        std::path::PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let out = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .expect("git must be installed");
        assert!(
            out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let git_in_repo = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git must be installed");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git_in_repo(&["config", "user.email", "t@t.com"]);
        git_in_repo(&["config", "user.name", "T"]);
        std::fs::write(repo.join("README.md"), b"hi\n").unwrap();
        git_in_repo(&["add", "."]);
        git_in_repo(&["commit", "-m", "init"]);

        let home = tempfile::tempdir().unwrap();
        let repo_opened = Pool::open(
            &repo,
            None,
            &crate::pool::OpenOptions {
                config: crate::config::TreehouseConfig {
                    root: Some(home.path().to_str().unwrap().to_string()),
                    ..crate::config::TreehouseConfig::default_config()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let acquired = repo_opened
            .get(&crate::pool::AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        let path = acquired.path.clone();
        repo_opened.release(&path.to_string_lossy()).unwrap();
        let pool_dir = repo_opened.pool_dir().to_path_buf();

        // Re-open BY DIRECTORY — exactly what the `--all` sweeps do.
        let by_dir = Pool::open_at(&pool_dir, &Default::default()).unwrap();
        assert!(
            !by_dir.root.join(".git").exists(),
            "precondition: an --all pool's root is not a repository, which is \
             why a single pool-wide repo_root can never reclaim anything"
        );
        (dir, home, repo, by_dir, path)
    }

    /// M-020 (destroy half): `destroy --all` must resolve each slot's owning
    /// repository from the SLOT'S OWN PATH, not once from `self.root`.
    ///
    /// Before the fix, `destroy` resolved one `repo_root` from `self.root` and
    /// reused it for every slot's `worktree_remove`. Under `Pool::open_at` —
    /// the shape every `--all` sweep uses — `self.root` is the pool directory's
    /// PARENT and not a repository, so `git worktree remove` failed for every
    /// slot and destroy reclaimed nothing, forever. This test fails if the
    /// per-slot resolution is removed and the single pool-wide root comes back.
    #[test]
    fn destroy_all_removes_a_slot_resolved_from_its_own_path() {
        let (_dir, _home, repo, pool, path) = pool_opened_by_directory();
        assert!(path.exists(), "worktree exists before destroy");

        let result = pool
            .destroy(
                &DestroyTargetSpec::All,
                &DestroyOptions {
                    dry_run: false,
                    include_in_use: true,
                    include_unlanded: true,
                    include_leased: false,
                    pre_destroy: vec![],
                },
            )
            .expect("destroy must not error");

        assert_eq!(
            result.destroyed.len(),
            1,
            "destroy --all must reclaim the slot; skips were: {:?}",
            result
                .skipped
                .iter()
                .map(|s| s.detail.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            !path.exists(),
            "the worktree directory must be gone from disk"
        );

        // And the repository that owned it no longer lists it: the removal ran
        // against the slot's OWN repo, which is the only one that could
        // deregister it.
        let listing = std::process::Command::new("git")
            .args(["worktree", "list"])
            .current_dir(&repo)
            .output()
            .expect("git must be installed");
        assert!(listing.status.success());
        let out = String::from_utf8_lossy(&listing.stdout);
        assert!(
            !out.contains(&path.to_string_lossy().to_string()),
            "the owning repo must no longer list the worktree; got:\n{out}"
        );
    }

    /// M-020 (destroy half): a slot whose repository cannot be resolved is
    /// SKIPPED, never deleted.
    ///
    /// `git worktree remove` against a nonexistent repository is the loud
    /// failure the fix removes for healthy slots — but for a genuinely
    /// unresolvable one, skipping is the only safe answer, and the caller must
    /// be told why rather than watching a bare git error scroll past.
    #[test]
    fn destroy_skips_a_slot_whose_repository_cannot_be_resolved() {
        let (_dir, _home, _repo, pool, _path) = pool_opened_by_directory();

        // A slot whose `.git` marker names a gitdir that does not exist: the
        // repository that owned it is gone, so no root can be resolved.
        let mut state = State::read_state(&pool.dir).unwrap();
        assert_eq!(state.worktrees.len(), 1);
        state.worktrees[0].destroying = false;
        crate::state_file::write_state(&pool.dir, &state).unwrap();

        // Point the recorded path at a directory with no repository above it.
        let orphan_parent = tempfile::tempdir().unwrap();
        let orphan = orphan_parent.path().join("gone");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join(".git"), "gitdir: /nonexistent/treehouse.git\n").unwrap();
        let mut state = State::read_state(&pool.dir).unwrap();
        state.worktrees[0].path = orphan.to_string_lossy().into_owned();
        crate::state_file::write_state(&pool.dir, &state).unwrap();

        let result = pool
            .destroy(
                &DestroyTargetSpec::All,
                &DestroyOptions {
                    dry_run: false,
                    include_in_use: true,
                    include_unlanded: true,
                    include_leased: true,
                    pre_destroy: vec![],
                },
            )
            .expect("destroy must not error");

        assert!(
            result.destroyed.is_empty(),
            "a slot with no resolvable repository must never be destroyed"
        );
        assert!(
            orphan.exists(),
            "the unresolvable slot's directory must be left on disk"
        );
    }
}
