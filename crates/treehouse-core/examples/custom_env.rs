//! Example: Custom environment with a non-default pool root.
//!
//! Run with: `cargo run --example custom_env -p treehouse-core`

use std::path::PathBuf;
use treehouse_core::TreehouseCore;
use treehouse_core::config::TreehouseConfig;
use treehouse_core::env::{FileMeta, TreehouseEnv};

/// A custom environment that stores pools in a different location.
struct CustomEnv {
    base: PathBuf,
}

impl TreehouseEnv for CustomEnv {
    fn pool_root(&self) -> Option<PathBuf> {
        Some(self.base.join("pools"))
    }

    fn update_cache_path(&self) -> Option<PathBuf> {
        Some(self.base.join("cache/update-check.json"))
    }

    fn user_config_path(&self) -> Option<PathBuf> {
        Some(self.base.join("config/treehouse.toml"))
    }

    fn read_file(&self, path: &std::path::Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn read_bytes(&self, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write_file(&self, path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, data)
    }

    fn ensure_dir(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn path_exists(&self, path: &std::path::Path) -> bool {
        path.exists()
    }

    fn list_dir(&self, path: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)
            .map(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).collect())
    }

    fn file_meta(&self, path: &std::path::Path) -> std::io::Result<FileMeta> {
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

fn main() {
    let env = CustomEnv {
        base: PathBuf::from("/tmp/my-app"),
    };

    let config = TreehouseConfig {
        max_trees: 4,
        ..TreehouseConfig::default_config()
    };

    let core = TreehouseCore::with_env(env, config);

    println!("Pool root: {:?}", core.pool_root());
    println!("No .treehouse directory created!");
}
