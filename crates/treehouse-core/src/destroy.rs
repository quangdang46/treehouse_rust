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

use std::path::Path;

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

        // Resolve the merge target so destroy and prune agree on "unmerged".
        let repo_root = self
            .git
            .main_repo_root(&self.root)
            .ok()
            .unwrap_or_else(|| self.root.clone());
        let default_ref = self
            .git
            .default_branch_merge_ref(&crate::git::GitRepo {
                common_dir: repo_root.clone(),
                worktree: None,
            })
            .ok()
            .unwrap_or_default();

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

        // Classify + gate.
        let mut removable = Vec::new();
        for wt in &targets {
            let target = self.classify_for_destroy(wt, &default_ref);
            let mut t = target;
            t.bytes = self.measure_size(&t.path);
            match self.allows(&t, allow_leased, opts) {
                Ok(()) => removable.push(t),
                Err(skip) => result.skipped.push(skip),
            }
        }
        result.planned = removable.clone();
        result.planned_bytes = removable.iter().map(|t| t.bytes).sum();

        if opts.dry_run {
            return Ok(result);
        }

        // Execute: two-phase.
        let (destroyed, exec_skips) =
            self.execute_destroy(&removable, &repo_root, &default_ref, allow_leased, opts)?;
        result.destroyed = destroyed.clone();
        result.freed_bytes = destroyed.iter().map(|t| t.bytes).sum();
        result.skipped.extend(exec_skips);
        Ok(result)
    }

    /// Classifies a worktree for destroy using live state (Go classifyForDestroy).
    fn classify_for_destroy(&self, wt: &WorktreeEntry, default_ref: &str) -> DestroyTarget {
        let procs = self
            .process
            .find_in_worktree(Path::new(&wt.path))
            .unwrap_or_default();
        let proc_scan_error = self.process.find_in_worktree(Path::new(&wt.path)).is_err();
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
    fn execute_destroy(
        &self,
        removable: &[DestroyTarget],
        repo_root: &Path,
        default_ref: &str,
        allow_leased: bool,
        opts: &DestroyOptions,
    ) -> Result<(Vec<DestroyTarget>, Vec<DestroySkip>), PoolError> {
        if removable.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let planned_by_path: std::collections::HashMap<String, DestroyTarget> = removable
            .iter()
            .map(|t| (t.path.clone(), t.clone()))
            .collect();

        // Phase 1: reserve Destroying + fresh owner under the lock.
        let reserved: Vec<(Reservation, DestroyTarget)> =
            with_pool_lock(&self.dir, self.lock_timeout, || {
                let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
                heal_state(&mut state, |pid| self.process.started_at(pid));
                let mut reserved = Vec::new();
                let mut skips = Vec::new();
                for i in 0..state.worktrees.len() {
                    if !planned_by_path.contains_key(&state.worktrees[i].path) {
                        continue;
                    }
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
                    if self.allows(&current, allow_leased, opts).is_err() {
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
                    reserved.push((reservation, current));
                }
                state_file::write_state(&self.dir, &state)
                    .map_err(|e| PoolError::Io("writing state".into(), e))?;
                Ok::<_, PoolError>(reserved)
            })?;
        let _ = planned_by_path;

        // Hooks OUTSIDE all locks (non-fatal).
        if !opts.pre_destroy.is_empty() {
            for (reservation, _) in &reserved {
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
        let (destroyed, skips) = with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let mut destroyed = Vec::new();
            let mut skips = Vec::new();
            let mut removed_paths = std::collections::HashSet::new();

            for (reservation, planned) in &reserved {
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
                let mut current = self.classify_for_destroy(&current_entry, default_ref);
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
                    if !self
                        .process
                        .find_in_worktree(Path::new(&path))
                        .unwrap_or_default()
                        .is_empty()
                    {
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

                // Remove the worktree.
                match self.git.worktree_remove(
                    &crate::git::GitRepo {
                        common_dir: repo_root.to_path_buf(),
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
}
