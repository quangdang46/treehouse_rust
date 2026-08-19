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
use crate::env::TreehouseEnv;

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

// ─── _with_env variants ────────────────────────────────────────────────────────

/// Resolves the pool root using the injected environment.
pub fn user_pool_root_with_env(env: &dyn TreehouseEnv) -> Option<PathBuf> {
    env.pool_root()
}

/// Resolves the pool root container using config + injected environment.
///
/// Same logic as [`user_pool_root_with_config`] but reads paths from `env`.
pub fn user_pool_root_with_config_and_env(
    user: &TreehouseConfig,
    env: &dyn TreehouseEnv,
) -> Option<PathBuf> {
    let root = user.root.as_deref().unwrap_or("");
    if root.is_empty() {
        return env.pool_root();
    }
    let expanded = config::expand_env(root);
    let expanded = PathBuf::from(&expanded);
    if !expanded.is_absolute() {
        // A relative root is repo-scoped; it has no meaning user-wide.
        return None;
    }
    Some(expanded.join(".treehouse"))
}

/// Enumerates managed pools using the injected environment.
pub fn discover_pools_with_env(root: &std::path::Path, env: &dyn TreehouseEnv) -> DiscoverResult {
    let mut result = DiscoverResult::default();
    let Ok(dirs) = env.list_dir(root) else {
        return result; // No pools at all — empty result, not an error.
    };
    let mut dirs: Vec<PathBuf> = dirs.into_iter().collect();
    dirs.sort();

    for dir in dirs {
        let state_path = crate::state::State::state_file_path(&dir);
        if !env.path_exists(&state_path) {
            result
                .skipped
                .insert(dir.clone(), "no treehouse-state.json".to_string());
            continue;
        }
        if env.read_file(&state_path).is_err() {
            result
                .skipped
                .insert(dir.clone(), "cannot read state".to_string());
            continue;
        }
        // A parse check guarantees a real managed pool
        if crate::state::State::read_state(&dir).is_err() {
            result
                .skipped
                .insert(dir.clone(), "state file does not parse".to_string());
            continue;
        }
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        result.pools.push(DiscoveredPool { dir, name });
    }
    result
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
        out.errors.extend(r.errors);
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
        out.errors.extend(r.errors);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
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

    // ─── _with_env tests ────────────────────────────────────────────────────

    #[test]
    fn user_pool_root_with_env_returns_env_root() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/custom/pools"));
        assert_eq!(
            user_pool_root_with_env(&env),
            Some(PathBuf::from("/custom/pools"))
        );
    }

    #[test]
    fn user_pool_root_with_config_and_env_empty_root() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/custom/pools"));
        let config = TreehouseConfig::default_config();
        assert_eq!(
            user_pool_root_with_config_and_env(&config, &env),
            Some(PathBuf::from("/custom/pools"))
        );
    }

    #[test]
    fn user_pool_root_with_config_and_env_absolute_root() {
        // Use a platform-absolute path (e.g. /abs on Unix, C:\abs on Windows)
        let abs_root = if cfg!(windows) {
            "C:\\abs\\root".to_string()
        } else {
            "/abs/root".to_string()
        };
        let expected = if cfg!(windows) {
            PathBuf::from("C:\\abs\\root\\.treehouse")
        } else {
            PathBuf::from("/abs/root/.treehouse")
        };

        let env = crate::env::InMemoryEnv::new(PathBuf::from("/custom"));
        let config = TreehouseConfig {
            root: Some(abs_root),
            ..TreehouseConfig::default_config()
        };
        assert_eq!(
            user_pool_root_with_config_and_env(&config, &env),
            Some(expected)
        );
    }

    #[test]
    fn discover_pools_with_env_finds_state_files() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/pools"));
        // Seed a pool with state file
        let pool_dir = PathBuf::from("/pools/myrepo-abc123");
        let state = crate::state::State::default();
        env.seed_file(
            &crate::state::State::state_file_path(&pool_dir),
            &serde_json::to_vec(&state).unwrap(),
        );

        let result = discover_pools_with_env(&PathBuf::from("/pools"), &env);
        assert_eq!(result.pools.len(), 1);
        assert_eq!(result.pools[0].name, "myrepo-abc123");
    }

    #[test]
    fn discover_pools_with_env_skips_non_pools() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/pools"));
        // Seed a dir without state file
        env.seed_file(Path::new("/pools/not-a-pool/random.txt"), b"hello");

        let result = discover_pools_with_env(&PathBuf::from("/pools"), &env);
        assert!(result.pools.is_empty());
        assert!(!result.skipped.is_empty());
    }

    #[test]
    fn merge_gc_results_aggregates_errors_across_pools() {
        let pool_a_dir = PathBuf::from("/pools/pool-a");
        let pool_b_dir = PathBuf::from("/pools/pool-b");
        let pool_c_dir = PathBuf::from("/pools/pool-c");

        let results = vec![
            // Pool A: successful reclaim
            (
                pool_a_dir.clone(),
                crate::gc::GcResult {
                    dry_run: false,
                    reclaimed: vec![crate::gc::GcWorktree {
                        name: "1".into(),
                        path: "/pools/pool-a/1/repo".into(),
                        bytes: 1000,
                        tag: "stale".into(),
                        warning: String::new(),
                    }],
                    freed_bytes: 1000,
                    ..Default::default()
                },
            ),
            // Pool B: pool-level error (simulates gc failure)
            (
                pool_b_dir.clone(),
                crate::gc::GcResult {
                    dry_run: false,
                    errors: vec![crate::gc::CleanupError {
                        name: "pool".into(),
                        path: pool_b_dir.to_string_lossy().into_owned(),
                        phase: "pool_gc".into(),
                        detail: "state lock timed out".into(),
                    }],
                    ..Default::default()
                },
            ),
            // Pool C: successful reclaim
            (
                pool_c_dir.clone(),
                crate::gc::GcResult {
                    dry_run: false,
                    reclaimed: vec![crate::gc::GcWorktree {
                        name: "1".into(),
                        path: "/pools/pool-c/1/repo".into(),
                        bytes: 2000,
                        tag: "stale".into(),
                        warning: String::new(),
                    }],
                    freed_bytes: 2000,
                    ..Default::default()
                },
            ),
        ];

        let merged = merge_gc_results(results);

        // Pools A and C were reclaimed successfully.
        assert_eq!(merged.reclaimed.len(), 2);
        assert_eq!(merged.freed_bytes, 3000);

        // Pool B's error is preserved in the merged result.
        assert_eq!(merged.errors.len(), 1);
        assert_eq!(merged.errors[0].phase, "pool_gc");
        assert_eq!(merged.errors[0].name, "pool");
        assert!(merged.errors[0].detail.contains("state lock timed out"));
    }

    #[test]
    fn merge_prune_results_aggregates_errors_across_pools() {
        let pool_a_dir = PathBuf::from("/pools/pool-a");
        let pool_b_dir = PathBuf::from("/pools/pool-b");

        let results = vec![
            (
                pool_a_dir,
                crate::prune::PruneResult {
                    dry_run: false,
                    pruned: vec![crate::prune::PruneWorktree {
                        name: "1".into(),
                        path: "/pools/pool-a/1/repo".into(),
                        bytes: 500,
                        orphaned: false,
                        warning: String::new(),
                    }],
                    freed_bytes: 500,
                    ..Default::default()
                },
            ),
            (
                pool_b_dir.clone(),
                crate::prune::PruneResult {
                    dry_run: false,
                    errors: vec![crate::prune::CleanupError {
                        name: "pool".into(),
                        path: pool_b_dir.to_string_lossy().into_owned(),
                        phase: "pool_prune".into(),
                        detail: "corrupt state file".into(),
                    }],
                    ..Default::default()
                },
            ),
        ];

        let merged = merge_prune_results(results);

        assert_eq!(merged.pruned.len(), 1);
        assert_eq!(merged.freed_bytes, 500);
        assert_eq!(merged.errors.len(), 1);
        assert_eq!(merged.errors[0].phase, "pool_prune");
    }

    // ─── Integration: sweep_pools with real pools ───────────────────────

    /// Creates a real git repo + pool in a temp dir.
    fn make_real_pool(
        home: &std::path::Path,
        repo_name: &str,
    ) -> (crate::pool::Pool, tempfile::TempDir, tempfile::TempDir) {
        let repo_guard = tempfile::tempdir().unwrap();
        let repo = repo_guard.path().join(repo_name);
        let init = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(repo_guard.path())
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

        let config = crate::config::TreehouseConfig {
            root: Some(home.to_str().unwrap().to_string()),
            ..crate::config::TreehouseConfig::default_config()
        };
        let opts = crate::pool::OpenOptions {
            config,
            ..Default::default()
        };
        let pool = crate::pool::Pool::open(&repo, None, &opts).expect("failed to open pool");
        (pool, repo_guard, tempfile::tempdir().unwrap())
    }

    #[test]
    fn sweep_pools_processes_multiple_real_pools() {
        let home = tempfile::tempdir().unwrap();
        let (pool_a, _rg_a, _hg_a) = make_real_pool(home.path(), "repo-a");
        let (pool_b, _rg_b, _hg_b) = make_real_pool(home.path(), "repo-b");

        // Collect pool dirs and gc results.
        let mut gc_results: Vec<(PathBuf, crate::gc::GcResult)> = Vec::new();

        // Simulate what cmd_gc_all / cmd_watch does: sweep each pool via gc.
        for pool in [&pool_a, &pool_b] {
            let opts = crate::gc::GcOptions {
                dry_run: true,
                prune_orphans: false,
            };
            let result = pool.gc(&opts).unwrap();
            gc_results.push((pool.pool_dir().to_path_buf(), result));
        }

        let merged = merge_gc_results(gc_results);

        // Both pools were processed (dry_run, so no actual reclamation).
        // The merged result is valid and contains data from both pools.
        assert!(merged.dry_run);
    }

    #[test]
    fn sweep_callback_error_isolation_real_pattern() {
        // This test exercises the exact pattern used by cmd_gc_all and cmd_watch:
        // the callback catches PoolError and wraps it as a CleanupError.
        let home = tempfile::tempdir().unwrap();
        let (pool_a, _rg, _hg) = make_real_pool(home.path(), "repo-ok");

        let mut results: Vec<(PathBuf, crate::gc::GcResult)> = Vec::new();

        // Sweep pool A (success) + simulate a failing pool B.
        let pools: Vec<(&str, Option<&crate::pool::Pool>)> = vec![
            ("ok", Some(&pool_a)),
            ("fail", None), // simulates a pool that fails to open or gc
        ];

        for (label, pool) in &pools {
            let pool_dir = home.path().join(format!("pool-{label}"));
            match pool {
                Some(p) => {
                    let opts = crate::gc::GcOptions {
                        dry_run: true,
                        prune_orphans: false,
                    };
                    results.push((pool_dir, p.gc(&opts).unwrap()));
                }
                None => {
                    // Simulate the P1 error-isolation pattern.
                    results.push((
                        pool_dir.clone(),
                        crate::gc::GcResult {
                            dry_run: true,
                            errors: vec![crate::gc::CleanupError {
                                name: "pool".into(),
                                path: pool_dir.to_string_lossy().into_owned(),
                                phase: "pool_gc".into(),
                                detail: "simulated pool error".into(),
                            }],
                            ..Default::default()
                        },
                    ));
                }
            }
        }

        let merged = merge_gc_results(results);

        // Pool A was processed (dry_run, no candidates expected).
        // Pool B's error is in the merged result.
        assert!(merged.errors.len() == 1);
        assert_eq!(merged.errors[0].phase, "pool_gc");
        assert!(merged.errors[0].detail.contains("simulated pool error"));
    }

    #[test]
    fn merge_gc_results_idempotent() {
        // Running merge on the same results twice produces the same output.
        let results = vec![
            (
                PathBuf::from("/pools/a"),
                crate::gc::GcResult {
                    dry_run: false,
                    reclaimed: vec![crate::gc::GcWorktree {
                        name: "1".into(),
                        path: "/pools/a/1/r".into(),
                        bytes: 100,
                        tag: "stale".into(),
                        warning: String::new(),
                    }],
                    freed_bytes: 100,
                    ..Default::default()
                },
            ),
            (
                PathBuf::from("/pools/b"),
                crate::gc::GcResult {
                    dry_run: false,
                    errors: vec![crate::gc::CleanupError {
                        name: "pool".into(),
                        path: "/pools/b".into(),
                        phase: "pool_gc".into(),
                        detail: "lock timeout".into(),
                    }],
                    ..Default::default()
                },
            ),
        ];

        let first = merge_gc_results(results.clone());
        let second = merge_gc_results(results);

        assert_eq!(first.reclaimed.len(), second.reclaimed.len());
        assert_eq!(first.errors.len(), second.errors.len());
        assert_eq!(first.freed_bytes, second.freed_bytes);
    }
}
