//! treehouse-core: pure library port of Treehouse (Go v2.1.1 behavioral reference).
//!
//! This crate contains the pool/state/lease/owner/process/git/hooks/config
//! logic with **no** CLI concerns (clap, anyhow, owo-colors live only in the
//! `treehouse` binary crate). Each command produces a structured [`CommandResult`]
//! that the CLI formats as human/json/toon.

/// Library version, kept in sync with the workspace `version` field.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod config;
pub mod destroy;
pub mod discovery;
pub mod doctor;
pub mod env;
pub mod gc;
pub mod git;
pub mod hardening;
pub mod hooks;
pub mod lease;
pub mod lock;
pub mod pool;
pub mod process;
pub mod prune;
pub mod reservation;
pub mod result;
pub mod run;
pub mod state;
pub mod state_file;
pub mod updater;
pub mod worktree;

// ─── TreehouseCore wrapper ─────────────────────────────────────────────────────

use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::{ConfigError, TreehouseConfig};
use env::{DefaultEnv, InMemoryEnv, TreehouseEnv};
use pool::{AcquireOptions, OpenOptions, Pool, PoolError, WorktreeStatus};

/// High-level entry point for library consumers.
///
/// Wraps [`Pool`] with a simpler API that handles boilerplate (config loading,
/// env injection). The consumer is responsible for providing the remote URL
/// when opening pools.
///
/// # Examples
///
/// ```rust,ignore
/// use treehouse_core::TreehouseCore;
/// use std::path::Path;
///
/// // CLI usage — zero config, DefaultEnv
/// let core = TreehouseCore::open(Path::new("."))?;
/// ```
pub struct TreehouseCore<E: TreehouseEnv + 'static = DefaultEnv> {
    env: Arc<E>,
    config: TreehouseConfig,
}

impl TreehouseCore {
    /// CLI usage — zero config, DefaultEnv.
    pub fn open(repo_root: &Path) -> Result<Self, ConfigError> {
        let config = TreehouseConfig::load(repo_root)?;
        Ok(Self {
            env: Arc::new(DefaultEnv),
            config,
        })
    }
}

impl<E: TreehouseEnv> TreehouseCore<E> {
    /// Library usage — consumer injects env + explicit config.
    pub fn with_env(env: E, config: TreehouseConfig) -> Self {
        Self {
            env: Arc::new(env),
            config,
        }
    }

    /// Library usage — consumer injects env, loads config from env.
    pub fn with_env_and_config(repo_root: &Path, env: E) -> Result<Self, ConfigError> {
        let config = TreehouseConfig::load_with_env(repo_root, &env)?;
        Ok(Self {
            env: Arc::new(env),
            config,
        })
    }

    /// The pool root directory from the injected environment.
    pub fn pool_root(&self) -> Option<PathBuf> {
        self.env.pool_root()
    }

    /// Open a pool for a repo. The caller provides the remote URL.
    pub fn open_pool(&self, repo_root: &Path, remote_url: Option<&str>) -> Result<Pool, PoolError> {
        Pool::open_with_env(
            repo_root,
            remote_url,
            &OpenOptions {
                config: self.config.clone(),
                ..Default::default()
            },
            self.env.clone(),
        )
    }

    /// Acquire a worktree from a pool.
    pub fn acquire(
        &self,
        repo_root: &Path,
        remote_url: Option<&str>,
        branch: Option<&str>,
    ) -> Result<pool::Acquired, PoolError> {
        let pool = self.open_pool(repo_root, remote_url)?;
        pool.get(&AcquireOptions {
            branch: branch.map(String::from),
            ..Default::default()
        })
    }

    /// Get pool status.
    pub fn status(
        &self,
        repo_root: &Path,
        remote_url: Option<&str>,
    ) -> Result<Vec<WorktreeStatus>, PoolError> {
        let pool = self.open_pool(repo_root, remote_url)?;
        pool.status()
    }
}

impl TreehouseCore<InMemoryEnv> {
    /// Create a test instance with InMemoryEnv.
    pub fn for_test(pool_root: PathBuf) -> Self {
        Self::with_env(InMemoryEnv::new(pool_root), TreehouseConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn treehouse_core_for_test_creates_working_instance() {
        let core = TreehouseCore::for_test(PathBuf::from("/test/pools"));
        assert_eq!(core.pool_root(), Some(PathBuf::from("/test/pools")));
    }

    #[test]
    fn treehouse_core_with_env_uses_injected_config() {
        let env = InMemoryEnv::new(PathBuf::from("/test"));
        let config = TreehouseConfig {
            max_trees: 4,
            ..TreehouseConfig::default_config()
        };
        let core = TreehouseCore::with_env(env, config);
        assert_eq!(core.config.max_trees, 4);
    }

    #[test]
    fn treehouse_core_open_uses_default_env() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("treehouse.toml"), "max_trees = 8\n").unwrap();
        let core = TreehouseCore::open(dir.path()).unwrap();
        assert_eq!(core.config.max_trees, 8);
        // pool_root should be ~/.treehouse (DefaultEnv)
        if let Some(root) = core.pool_root() {
            assert!(root.ends_with(".treehouse"));
        }
    }
}

#[cfg(all(test, feature = "toon"))]
mod toon_smoke_tests {
    use serde_json::json;

    #[test]
    fn toon_encoder_smoke() {
        let value = json!({
            "name": "Alice",
            "age": 30,
            "tags": ["rust", "toon"]
        });
        let encoded = toon::encode(value.clone(), None);
        assert!(encoded.contains("name: Alice"), "got: {encoded}");
        assert!(encoded.contains("tags[2]"), "got: {encoded}");
        // Round-trips back to a JsonValue.
        let decoded = toon::try_decode(&encoded, None).unwrap();
        let _ = decoded;
    }
}
