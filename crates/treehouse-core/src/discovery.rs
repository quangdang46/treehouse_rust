//! Pool discovery for `--all` (global) sweeps.
//!
//! `prune --all`, `gc --all`, and `destroy --all` operate on every managed
//! pool under the user-level root (Go `findAllPools`). Pool directories are
//! `<poolRoot>/<repoName>-<6hex>` siblings; a directory is only a managed
//! pool when it contains a `treehouse-state.json` written by treehouse.
//!
//! The pool root respects the user config's `root` override (absolute roots
//! nest under `<root>/.treehouse`); the default is `$HOME/.treehouse`. A
//! relative user `root` cannot be interpreted without a repo — such a config
//! is reported and skipped for discovery.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::config::{self, ConfigError, TreehouseConfig};

/// A discovered pool dir plus the pool name (`<repoName>-<6hex>`).
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredPool {
    pub dir: PathBuf,
    pub name: String,
}

/// The result of a user-level discovery sweep.
#[derive(Debug, Clone, Default)]
pub struct DiscoverResult {
    /// Pools found (sorted by name).
    pub pools: Vec<DiscoveredPool>,
    /// Reasons a candidate directory could not be classified as a pool,
    /// keyed by directory path.
    pub skipped: BTreeMap<PathBuf, String>,
}

/// The default relative name of the user-level pool root container (Go:
/// `$HOME/.treehouse`).
pub fn user_pool_root() -> Option<PathBuf> {
    config::home_dir().map(|h| h.join(".treehouse"))
}

/// Resolves the container that holds per-repo pools under the user config's
/// `root`, honoring `$VAR` expansion. `None` when HOME is unset.
///
/// - `root = ""` → `$HOME/.treehouse`
/// - absolute / `$VAR`-expanded → `<root>/.treehouse`
/// - relative without a repo → cannot interpret (caller reports and skips)
pub fn user_pool_root_with_config(user: &TreehouseConfig) -> Option<PathBuf> {
    let root = user.root.as_deref().unwrap_or("");
    if root.is_empty() {
        return user_pool_root();
    }
    let expanded = config::expand_env(root);
    let expanded = PathBuf::from(&expanded);
    if !expanded.is_absolute() {
        // A relative root is repo-scoped; it has no meaning user-wide.
        return None;
    }
    Some(expanded.join(".treehouse"))
}

/// Enumerates managed pools under a pool root container.
///
/// Every immediate subdirectory is inspected; dirs actually containing a
/// treehouse state file are classified as pools. A state file that fails to
/// parse (IO or invalid JSON) is recorded in `skipped` — a corrupt or foreign
/// directory is never swept blindly.
pub fn discover_pools(root: &PathBuf) -> DiscoverResult {
    let mut result = DiscoverResult::default();
    let Ok(entries) = fs::read_dir(root) else {
        return result; // No pools at all — empty result, not an error.
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let state_path = crate::state::State::state_file_path(&dir);
        if !state_path.exists() {
            result
                .skipped
                .insert(dir.clone(), "no treehouse-state.json".to_string());
            continue;
        }
        if let Err(e) = fs::read_to_string(&state_path) {
            result
                .skipped
                .insert(dir.clone(), format!("cannot read state: {e}"));
            continue;
        }
        // A parse check guarantees a real managed pool (the state file parser
        // is the same one `gc`/`prune` use).
        if crate::state::State::read_state(&dir).is_err() {
            result
                .skipped
                .insert(dir.clone(), "state file does not parse".to_string());
            continue;
        }
        // Normalize path for the CLI-facing name is the dir's file name.
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        result.pools.push(DiscoveredPool { dir, name });
    }
    result
}

/// Loads the user-level (global) config, reporting a bad config gracefully.
pub fn load_user_config_for_discovery() -> Result<TreehouseConfig, ConfigError> {
    TreehouseConfig::load_global()
}

/// Runs a per-pool closure over every discovered managed pool under the user
/// level (Go `gc --all` / `prune --all` sweep).
///
/// Pools are processed in name order; each pool is guarded so one failing
/// pool does not abort the sweep. Each per-pool error is reported on stderr
/// and the sweep continues.
pub fn sweep_pools<F>(
    user: &TreehouseConfig,
    ctx_factory: impl Fn(&PathBuf) -> Result<crate::pool::Pool, crate::pool::PoolError>,
    mut per_pool: F,
) -> Result<(), crate::pool::PoolError>
where
    F: FnMut(&crate::pool::Pool) -> Result<(), crate::pool::PoolError>,
{
    let Some(root) = user_pool_root_with_config(user) else {
        return Ok(());
    };
    let found = discover_pools(&root);
    for p in &found.pools {
        let Ok(pool) = ctx_factory(&p.dir) else {
            let msg = format!("🌳 skipping pool {}: cannot open", p.dir.display());
            eprintln!("{msg}");
            continue;
        };
        if let Err(e) = per_pool(&pool) {
            let msg = format!("🌳 skipping pool {}: {e:#}", p.dir.display());
            eprintln!("{msg}");
        }
    }
    Ok(())
}

/// Aggregate `PruneResult`s from a sweep into one combined result (used by
/// `prune --all`; `max_trees` is only relevant per-pool so it is not merged).
pub fn merge_prune_results(
    results: Vec<(PathBuf, crate::prune::PruneResult)>,
) -> crate::prune::PruneResult {
    let mut out = crate::prune::PruneResult {
        dry_run: results.first().map(|(_, r)| r.dry_run).unwrap_or(true),
        ..Default::default()
    };
    for (dir, r) in results {
        let dirs = dir.to_string_lossy();
        for c in r.candidates {
            out.reclaimable_bytes += c.bytes;
            out.candidates.push(crate::prune::PruneWorktree {
                name: format!("{}/{}", dirs, c.name),
                ..c
            });
        }
        for p in r.pruned {
            out.freed_bytes += p.bytes;
            out.pruned.push(p);
        }
        out.skipped.extend(r.skipped);
    }
    out
}

/// Aggregate `GcResult`s from a sweep into one combined result.
pub fn merge_gc_results(results: Vec<(PathBuf, crate::gc::GcResult)>) -> crate::gc::GcResult {
    let mut out = crate::gc::GcResult {
        dry_run: results.first().map(|(_, r)| r.dry_run).unwrap_or(true),
        ..Default::default()
    };
    for (dir, r) in results {
        let dirs = dir.to_string_lossy();
        for c in r.candidates {
            out.reclaimable_bytes += c.bytes;
            out.candidates.push(crate::gc::GcWorktree {
                name: format!("{}/{}", dirs, c.name),
                ..c
            });
        }
        for p in r.reclaimed {
            out.freed_bytes += p.bytes;
            out.reclaimed.push(p);
        }
        out.skipped.extend(r.skipped);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn discovers_only_state_file_dirs() {
        let _guard = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // A real pool: dir with a valid state file.
        let pool_dir = tmp.path().join("myrepo-abc123");
        fs::create_dir_all(&pool_dir).unwrap();
        crate::state_file::write_state(&pool_dir, &crate::state::State::default()).unwrap();
        // A decoy dir without a state file.
        let decoy = tmp.path().join("not-a-pool");
        fs::create_dir_all(&decoy).unwrap();
        fs::write(decoy.join("random.txt"), "hello").unwrap();

        let result = discover_pools(&tmp.path().to_path_buf());
        assert_eq!(result.pools.len(), 1, "only the state-file dir is a pool");
        assert_eq!(result.pools[0].name, "myrepo-abc123");
        assert!(result.skipped.contains_key(&decoy));
    }
}
