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
#[derive(Debug, Clone)]
pub struct GcWorktree {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    /// The category tag shown in output.
    pub tag: String,
    pub warning: String,
}

/// A worktree gc left in place.
#[derive(Debug, Clone)]
pub struct GcSkipped {
    pub name: String,
    pub path: String,
    pub category: String,
    pub reason: String,
}

/// The result of a gc run.
#[derive(Debug, Clone, Default)]
pub struct GcResult {
    pub dry_run: bool,
    pub candidates: Vec<GcWorktree>,
    pub reclaimed: Vec<GcWorktree>,
    pub skipped: Vec<GcSkipped>,
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

impl Pool {
    /// Reclaims stale worktrees (Go-style two-phase, strictly safe).
    pub fn gc(&self, opts: &GcOptions) -> Result<GcResult, PoolError> {
        // Snapshot under one lock: read + heal + write.
        let entries = crate::prune::with_pool_snapshot(self)?;

        let repo_root = self
            .git
            .main_repo_root(&self.root)
            .unwrap_or_else(|_| self.root.clone());
        let default_ref = self.resolve_prune_default_ref(&repo_root);

        let mut result = GcResult {
            dry_run: opts.dry_run,
            ..Default::default()
        };
        let mut planned: Vec<GcWorktree> = Vec::new();
        let now = chrono::Utc::now();

        for wt in &entries {
            let (candidate, skipped) = self.analyze_gc_candidate(
                wt,
                default_ref.as_ref().ok().map(|s| s.as_str()),
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
                planned.push(c);
            }
        }

        if opts.dry_run || planned.is_empty() {
            return Ok(result);
        }

        let reclaimed = self.execute_gc(&planned)?;
        result.reclaimed = reclaimed.clone();
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
                    tag: "stale/orphaned".into(),
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
    fn execute_gc(&self, planned: &[GcWorktree]) -> Result<Vec<GcWorktree>, PoolError> {
        // Phase 1: reserve Destroying + fresh owner.
        let reserved: Vec<(Reservation, GcWorktree)> =
            crate::pool::with_pool_lock(&self.dir, self.lock_timeout, || {
                let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
                heal_state(&mut state, |pid| self.process.started_at(pid));
                let mut reserved = Vec::new();
                for w in planned {
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
                    reserved.push((reservation, w.clone()));
                }
                state_file::write_state(&self.dir, &state)
                    .map_err(|e| PoolError::Io("writing state".into(), e))?;
                Ok::<_, PoolError>(reserved)
            })?;

        // Phase 2: re-verify + delete.
        crate::pool::with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let mut reclaimed = Vec::new();
            let mut removed = std::collections::HashSet::new();
            for (reservation, w) in &reserved {
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
                let repo = crate::git::GitRepo {
                    common_dir: self.root.clone(),
                    worktree: None,
                };
                let _ = self.git.worktree_remove(&repo, Path::new(&path));
                let _ = std::fs::remove_dir_all(Path::new(&path));
                removed.insert(path.clone());
                reclaimed.push(w.clone());
            }
            state.worktrees.retain(|e| !removed.contains(&e.path));
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".into(), e))?;
            Ok::<_, PoolError>(reclaimed)
        })
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
}
