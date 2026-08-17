//! Trait-based environment abstraction for treehouse-core.
//!
//! This module defines [`TreehouseEnv`], the trait that abstracts ALL filesystem
//! and environment side-effects. Consumers inject their own implementation to
//! control where pools live, how config is loaded, and where state is persisted.
//!
//! # Implementations
//!
//! - [`DefaultEnv`]: Identical to current behavior (real filesystem + env vars).
//!   Used by the CLI binary.
//! - [`InMemoryEnv`]: Zero filesystem for tests. Uses `HashMap` for files,
//!   `RwLock` for thread safety.
//!
//! # Usage
//!
//! ```rust,ignore
//! use treehouse_core::env::{InMemoryEnv, TreehouseEnv};
//! use std::path::PathBuf;
//!
//! // Zero-filesystem test environment
//! let env = InMemoryEnv::new(PathBuf::from("/test/pools"));
//! assert_eq!(env.pool_root(), Some(PathBuf::from("/test/pools")));
//! ```

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Metadata about a file (size, modification time).
#[derive(Debug, Clone)]
pub struct FileMeta {
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

/// Abstracts all filesystem and environment side-effects.
///
/// Consumer injects their own implementation — zero dot-folder, zero env vars.
/// All methods take `&self` — interior mutability via RwLock for InMemoryEnv.
pub trait TreehouseEnv: Send + Sync {
    /// Root directory for pool storage.
    /// Default: `$HOME/.treehouse`
    fn pool_root(&self) -> Option<PathBuf>;

    /// Path to the update-check cache file.
    /// Default: `~/.treehouse/update-check.json`
    fn update_cache_path(&self) -> Option<PathBuf>;

    /// Path to the user-level config file.
    /// Default: `~/.config/treehouse/config.toml`
    fn user_config_path(&self) -> Option<PathBuf>;

    /// Read a file's contents as a string.
    fn read_file(&self, path: &Path) -> io::Result<String>;

    /// Read raw bytes from a file.
    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// Write bytes to a file (atomic preferred).
    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()>;

    /// Create directories recursively.
    fn ensure_dir(&self, path: &Path) -> io::Result<()>;

    /// Check if a path exists.
    fn path_exists(&self, path: &Path) -> bool;

    /// List directory entries.
    fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;

    /// Get file metadata (size, modified time).
    fn file_meta(&self, path: &Path) -> io::Result<FileMeta>;

    /// Resolve an env var. Default: `std::env::var`.
    fn env_var(&self, name: &str) -> Option<String>;

    /// Resolve an env var as path. Default: `std::env::var_os`.
    fn env_var_os(&self, name: &str) -> Option<PathBuf>;

    /// Get the current working directory.
    fn cwd(&self) -> Option<PathBuf>;
}

// ─── DefaultEnv ────────────────────────────────────────────────────────────────

/// Default implementation — identical to current behavior.
/// Uses real filesystem + env vars. For CLI usage.
pub struct DefaultEnv;

impl DefaultEnv {
    fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    }
}

impl TreehouseEnv for DefaultEnv {
    fn pool_root(&self) -> Option<PathBuf> {
        Self::home_dir().map(|h| h.join(".treehouse"))
    }

    fn update_cache_path(&self) -> Option<PathBuf> {
        let home = Self::home_dir()?;
        Some(home.join(".treehouse").join("update-check.json"))
    }

    fn user_config_path(&self) -> Option<PathBuf> {
        let home = Self::home_dir()?;
        Some(home.join(".config").join("treehouse").join("config.toml"))
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, data)
    }

    fn ensure_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)
            .map(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).collect())
    }

    fn file_meta(&self, path: &Path) -> io::Result<FileMeta> {
        let meta = std::fs::metadata(path)?;
        Ok(FileMeta {
            size: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn env_var_os(&self, name: &str) -> Option<PathBuf> {
        std::env::var_os(name).map(PathBuf::from)
    }

    fn cwd(&self) -> Option<PathBuf> {
        std::env::current_dir().ok()
    }
}

// ─── InMemoryEnv ───────────────────────────────────────────────────────────────

/// In-memory environment for tests — zero filesystem side-effects.
///
/// Uses `HashMap` for files and directories, `RwLock` for thread safety.
/// Env vars are set at construction (no lock needed).
pub struct InMemoryEnv {
    files: RwLock<HashMap<PathBuf, Vec<u8>>>,
    dirs: RwLock<HashMap<PathBuf, bool>>,
    env: HashMap<String, String>,
    pool_root: PathBuf,
    cache_dir: PathBuf,
    config_dir: PathBuf,
}

impl InMemoryEnv {
    /// Create a new in-memory environment with the given pool root.
    pub fn new(pool_root: PathBuf) -> Self {
        let cache_dir = pool_root.join("cache");
        let config_dir = pool_root.join("config");
        Self {
            files: RwLock::new(HashMap::new()),
            dirs: RwLock::new(HashMap::new()),
            env: HashMap::new(),
            pool_root,
            cache_dir,
            config_dir,
        }
    }

    /// Builder: set an env var.
    pub fn with_env(mut self, key: &str, val: &str) -> Self {
        self.env.insert(key.to_string(), val.to_string());
        self
    }

    /// Seed a file for testing. Also registers parent directories.
    pub fn seed_file(&self, path: &Path, content: &[u8]) {
        self.files
            .write()
            .unwrap()
            .insert(path.to_path_buf(), content.to_vec());
        // Register all parent directories up to pool_root
        let mut current = path.parent();
        while let Some(p) = current {
            if p == self.pool_root || !p.starts_with(&self.pool_root) {
                break;
            }
            self.dirs.write().unwrap().insert(p.to_path_buf(), true);
            current = p.parent();
        }
    }
}

impl TreehouseEnv for InMemoryEnv {
    fn pool_root(&self) -> Option<PathBuf> {
        Some(self.pool_root.clone())
    }

    fn update_cache_path(&self) -> Option<PathBuf> {
        Some(self.cache_dir.join("update-check.json"))
    }

    fn user_config_path(&self) -> Option<PathBuf> {
        Some(self.config_dir.join("config.toml"))
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        let files = self.files.read().unwrap();
        files
            .get(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        let files = self.files.read().unwrap();
        files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.files
            .write()
            .unwrap()
            .insert(path.to_path_buf(), data.to_vec());
        if let Some(parent) = path.parent() {
            self.dirs
                .write()
                .unwrap()
                .insert(parent.to_path_buf(), true);
        }
        Ok(())
    }

    fn ensure_dir(&self, path: &Path) -> io::Result<()> {
        self.dirs.write().unwrap().insert(path.to_path_buf(), true);
        Ok(())
    }

    fn path_exists(&self, path: &Path) -> bool {
        self.files.read().unwrap().contains_key(path)
            || self.dirs.read().unwrap().contains_key(path)
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let prefix = path.to_path_buf();
        let mut entries = Vec::new();

        // Collect registered directories whose parent is `path`
        {
            let dirs = self.dirs.read().unwrap();
            for p in dirs.keys() {
                if p.parent() == Some(&prefix) && p != &prefix {
                    entries.push(p.clone());
                }
            }
        }

        // Collect files whose parent is `path`
        {
            let files = self.files.read().unwrap();
            for p in files.keys() {
                if p.parent() == Some(&prefix) {
                    entries.push(p.clone());
                }
            }
        }

        entries.sort();
        entries.dedup();
        Ok(entries)
    }

    fn file_meta(&self, path: &Path) -> io::Result<FileMeta> {
        let files = self.files.read().unwrap();
        files
            .get(path)
            .map(|data| FileMeta {
                size: data.len() as u64,
                modified: None,
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }

    fn env_var_os(&self, name: &str) -> Option<PathBuf> {
        self.env.get(name).map(PathBuf::from)
    }

    fn cwd(&self) -> Option<PathBuf> {
        self.env_var("PWD").map(PathBuf::from)
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_env_read_write_roundtrip() {
        let env = InMemoryEnv::new(PathBuf::from("/test"));
        env.seed_file(Path::new("/test/file.txt"), b"hello");
        assert_eq!(env.read_file(Path::new("/test/file.txt")).unwrap(), "hello");
        assert!(env.path_exists(Path::new("/test/file.txt")));
        assert!(!env.path_exists(Path::new("/test/missing.txt")));
    }

    #[test]
    fn in_memory_env_list_dir() {
        let env = InMemoryEnv::new(PathBuf::from("/test"));
        env.seed_file(Path::new("/test/a.txt"), b"a");
        env.seed_file(Path::new("/test/b.txt"), b"b");
        let entries = env.list_dir(Path::new("/test")).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn in_memory_env_env_var() {
        let env = InMemoryEnv::new(PathBuf::from("/test")).with_env("HOME", "/fake/home");
        assert_eq!(env.env_var("HOME"), Some("/fake/home".to_string()));
        assert_eq!(env.env_var("MISSING"), None);
    }

    #[test]
    fn default_env_pool_root_matches_current() {
        let env = DefaultEnv;
        // DefaultEnv reads $HOME — if unset, pool_root returns None.
        // Either outcome is correct behavior.
        if let Some(root) = env.pool_root() {
            assert!(root.ends_with(".treehouse"));
        }
    }
}
