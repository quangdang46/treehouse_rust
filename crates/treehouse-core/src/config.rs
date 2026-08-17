//! Config loading: repo `treehouse.toml` + user `~/.config/treehouse/config.toml`.
//!
//! Precedence (Go parity):
//! - Repo config takes precedence for repo-safe settings (`max_trees`, `root`).
//! - Hooks are loaded ONLY from user config — repo hooks are ignored for safety.
//! - `load_global` ignores repo config entirely (it may run without a repo).
//!
//! Pool dir resolution:
//! - `root == ""` → `$HOME/.treehouse`
//! - relative/`.` roots nest under the repo root
//! - absolute / `$VAR`-expanded roots nest under the given root + `/.treehouse`
//! - a relative root without a repo is an error

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::env::TreehouseEnv;

/// Repo-safe + user config settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreehouseConfig {
    pub max_trees: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
    /// P1 additive: default TTL for `treehouse run` leases. Zero/None = no TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
}

/// Lifecycle hooks (user-level only; repo hooks are ignored for safety).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Hooks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_create: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_destroy: Vec<String>,
}

impl Hooks {
    fn is_empty(&self) -> bool {
        self.post_create.is_empty() && self.pre_destroy.is_empty()
    }
}

/// The default `max_trees` (Go `DefaultConfig`).
pub const DEFAULT_MAX_TREES: u32 = 16;

impl Default for TreehouseConfig {
    fn default() -> Self {
        Self::default_config()
    }
}

impl TreehouseConfig {
    pub fn default_config() -> Self {
        TreehouseConfig {
            max_trees: DEFAULT_MAX_TREES,
            root: None,
            hooks: Hooks::default(),
            lease_ttl_secs: None,
        }
    }

    /// Loads repo + user config with Go's merge rules (see module docs).
    pub fn load(repo_root: &Path) -> Result<Self, ConfigError> {
        let mut cfg = Self::default_config();

        let repo_path = repo_root.join("treehouse.toml");
        let has_repo_config = repo_path.exists();
        if has_repo_config {
            let text = std::fs::read_to_string(&repo_path)
                .map_err(|e| ConfigError::Io(repo_path.display().to_string(), e))?;
            let decoded: TreehouseConfig = toml::from_str(&text)
                .map_err(|e| ConfigError::Toml(repo_path.display().to_string(), e.to_string()))?;
            cfg.max_trees = decoded.max_trees;
            cfg.root = decoded.root;
            cfg.lease_ttl_secs = decoded.lease_ttl_secs;
            // Repo hooks are ignored for safety.
            cfg.hooks = Hooks::default();
        }

        let (user_cfg, has_user_config) = load_user()?;
        if has_user_config {
            if !has_repo_config {
                cfg = user_cfg;
            } else {
                // Repo wins repo-safe settings; user provides hooks only.
                cfg.hooks = user_cfg.hooks;
            }
        }

        Ok(cfg)
    }

    /// Loads default + user config ONLY (ignores repo config).
    pub fn load_global() -> Result<Self, ConfigError> {
        let (user_cfg, has_user_config) = load_user()?;
        if has_user_config {
            Ok(user_cfg)
        } else {
            Ok(Self::default_config())
        }
    }
}

/// Loads the user-level config, if present.
fn load_user() -> Result<(TreehouseConfig, bool), ConfigError> {
    let cfg = TreehouseConfig::default_config();
    let Some(user_path) = user_config_path() else {
        return Ok((cfg, false));
    };
    if !user_path.exists() {
        return Ok((cfg, false));
    }
    let text = std::fs::read_to_string(&user_path)
        .map_err(|e| ConfigError::Io(user_path.display().to_string(), e))?;
    let decoded: TreehouseConfig = toml::from_str(&text)
        .map_err(|e| ConfigError::Toml(user_path.display().to_string(), e.to_string()))?;
    Ok((decoded, true))
}

/// `$HOME/.config/treehouse/config.toml` (Windows uses `%USERPROFILE%`).
fn user_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("treehouse")
            .join("config.toml"),
    )
}

/// Resolves the pool directory for a repo: `poolRoot/<repoName>-<hash>` where
/// the hash is the first 6 hex of sha256 of the remote URL (or abs repo path
/// for local-only repos).
pub fn resolve_pool_dir(
    repo_root: &Path,
    root: Option<&str>,
    remote_url: Option<&str>,
) -> Result<PathBuf, ConfigError> {
    let hash_input = remote_url
        .filter(|u| !u.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let short_hash = crate::git::short_hash(&hash_input);
    let pool_name = format!("{repo_name}-{short_hash}");

    let pool_root = resolve_pool_root(repo_root, root)?;
    Ok(pool_root.join(pool_name))
}

/// Resolves the directory that contains per-repository pools.
pub fn resolve_pool_root(repo_root: &Path, root: Option<&str>) -> Result<PathBuf, ConfigError> {
    let root = root.unwrap_or("");
    if root.is_empty() {
        let home = home_dir().ok_or_else(|| {
            ConfigError::Invalid("home dir not found".into(), "cannot resolve $HOME".into())
        })?;
        return Ok(home.join(".treehouse"));
    }

    let expanded = expand_env(root);
    let expanded = PathBuf::from(&expanded);
    if !expanded.is_absolute() {
        if repo_root.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                format!("relative treehouse root {root:?} requires a repository"),
                String::new(),
            ));
        }
        return Ok(repo_root.join(expanded).join(".treehouse"));
    }
    Ok(expanded.join(".treehouse"))
}

/// Expands `$VAR` / `${VAR}` in a string (Go `os.ExpandEnv`).
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            if i + 1 < chars.len() && chars[i + 1] == '{' {
                if let Some(end) = chars[i + 2..].iter().position(|&c| c == '}') {
                    let name: String = chars[i + 2..i + 2 + end].iter().collect();
                    if let Ok(val) = std::env::var(&name) {
                        out.push_str(&val);
                        i += 2 + end + 1;
                        continue;
                    }
                    i += 2 + end + 1;
                    continue;
                }
            } else if i + 1 < chars.len() && (chars[i + 1].is_alphanumeric() || chars[i + 1] == '_')
            {
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let name: String = chars[i + 1..j].iter().collect();
                if let Ok(val) = std::env::var(&name) {
                    out.push_str(&val);
                    i = j;
                    continue;
                }
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// ─── _with_env variants ────────────────────────────────────────────────────────

/// Resolves the pool directory using the injected environment.
///
/// Same logic as [`resolve_pool_dir`] but reads paths from `env` instead of
/// hardcoded `$HOME/.treehouse`.
pub fn resolve_pool_dir_with_env(
    repo_root: &Path,
    root: Option<&str>,
    remote_url: Option<&str>,
    env: &dyn TreehouseEnv,
) -> Result<PathBuf, ConfigError> {
    let hash_input = remote_url
        .filter(|u| !u.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let short_hash = crate::git::short_hash(&hash_input);
    let pool_name = format!("{repo_name}-{short_hash}");

    let pool_root = resolve_pool_root_with_env(repo_root, root, env)?;
    Ok(pool_root.join(pool_name))
}

/// Resolves the pool root directory using the injected environment.
///
/// Same logic as [`resolve_pool_root`] but reads paths from `env` instead of
/// hardcoded `$HOME/.treehouse`.
pub fn resolve_pool_root_with_env(
    repo_root: &Path,
    root: Option<&str>,
    env: &dyn TreehouseEnv,
) -> Result<PathBuf, ConfigError> {
    let root = root.unwrap_or("");
    if root.is_empty() {
        let pool = env.pool_root().ok_or_else(|| {
            ConfigError::Invalid(
                "pool root not found".into(),
                "env.pool_root() returned None".into(),
            )
        })?;
        return Ok(pool);
    }

    let expanded = expand_env(root);
    let expanded = PathBuf::from(&expanded);
    if !expanded.is_absolute() {
        if repo_root.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                format!("relative treehouse root {root:?} requires a repository"),
                String::new(),
            ));
        }
        return Ok(repo_root.join(expanded).join(".treehouse"));
    }
    Ok(expanded.join(".treehouse"))
}

/// Resolves the user config path using the injected environment.
pub fn user_config_path_with_env(env: &dyn TreehouseEnv) -> Option<PathBuf> {
    env.user_config_path()
}

/// Loads the user-level config using the injected environment.
fn load_user_with_env(env: &dyn TreehouseEnv) -> Result<(TreehouseConfig, bool), ConfigError> {
    let cfg = TreehouseConfig::default_config();
    let Some(user_path) = env.user_config_path() else {
        return Ok((cfg, false));
    };
    if !env.path_exists(&user_path) {
        return Ok((cfg, false));
    }
    let text = env
        .read_file(&user_path)
        .map_err(|e| ConfigError::Io(user_path.display().to_string(), e))?;
    let decoded: TreehouseConfig = toml::from_str(&text)
        .map_err(|e| ConfigError::Toml(user_path.display().to_string(), e.to_string()))?;
    Ok((decoded, true))
}

impl TreehouseConfig {
    /// Loads repo + user config using the injected environment.
    ///
    /// Same merge rules as [`TreehouseConfig::load`] but reads from `env`.
    pub fn load_with_env(repo_root: &Path, env: &dyn TreehouseEnv) -> Result<Self, ConfigError> {
        let mut cfg = Self::default_config();

        let repo_path = repo_root.join("treehouse.toml");
        let has_repo_config = env.path_exists(&repo_path);
        if has_repo_config {
            let text = env
                .read_file(&repo_path)
                .map_err(|e| ConfigError::Io(repo_path.display().to_string(), e))?;
            let decoded: TreehouseConfig = toml::from_str(&text)
                .map_err(|e| ConfigError::Toml(repo_path.display().to_string(), e.to_string()))?;
            cfg.max_trees = decoded.max_trees;
            cfg.root = decoded.root;
            cfg.lease_ttl_secs = decoded.lease_ttl_secs;
            // Repo hooks are ignored for safety.
            cfg.hooks = Hooks::default();
        }

        let (user_cfg, has_user_config) = load_user_with_env(env)?;
        if has_user_config {
            if !has_repo_config {
                cfg = user_cfg;
            } else {
                // Repo wins repo-safe settings; user provides hooks only.
                cfg.hooks = user_cfg.hooks;
            }
        }

        Ok(cfg)
    }
}

/// Errors from config loading.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid config: {0}")]
    Invalid(String, String),
    #[error("failed to read config file {0}: {1}")]
    Io(String, std::io::Error),
    #[error("failed to parse config file {0}: {1}")]
    Toml(String, String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Serializes tests that mutate process-global env vars so they don't
    /// interfere when run in parallel.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn default_config_has_16_max_trees() {
        let cfg = TreehouseConfig::default_config();
        assert_eq!(cfg.max_trees, 16);
        assert!(cfg.hooks.is_empty());
    }

    #[test]
    fn repo_config_loads_and_ignores_repo_hooks() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("treehouse.toml"),
            "max_trees = 4\n[hooks]\npost_create = [\"./scripts/setup.sh\"]\n",
        )
        .unwrap();
        let cfg = TreehouseConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.max_trees, 4);
        // Repo hooks are ignored for safety.
        assert!(cfg.hooks.is_empty(), "repo hooks must be ignored");
    }

    #[test]
    fn user_config_hooks_merge_when_repo_config_present() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("treehouse.toml"), "max_trees = 4\n").unwrap();

        // Write a fake user config into a temp HOME.
        let fake_home = tempfile::tempdir().unwrap();
        let user_dir = fake_home.path().join(".config").join("treehouse");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("config.toml"),
            "max_trees = 99\n[hooks]\npost_create = [\"./user-hook.sh\"]\n",
        )
        .unwrap();

        // Point HOME at the fake home for this test.
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let cfg = TreehouseConfig::load(dir.path()).unwrap();
        // Repo wins repo-safe settings.
        assert_eq!(cfg.max_trees, 4);
        // User provides hooks only.
        assert_eq!(cfg.hooks.post_create, ["./user-hook.sh"]);

        unsafe {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn user_config_authoritative_without_repo_config() {
        let _guard = env_lock().lock().unwrap();
        let fake_home = tempfile::tempdir().unwrap();
        let user_dir = fake_home.path().join(".config").join("treehouse");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("config.toml"),
            "max_trees = 32\n[hooks]\npre_destroy = [\"teardown.sh\"]\n",
        )
        .unwrap();
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let dir = tempfile::tempdir().unwrap();
        let cfg = TreehouseConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.max_trees, 32);
        assert_eq!(cfg.hooks.pre_destroy, ["teardown.sh"]);

        unsafe {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn load_global_ignores_repo_config() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("treehouse.toml"), "max_trees = 2\n").unwrap();
        // Point HOME at an empty fake home so a real user config can never
        // leak in (tests must not depend on the machine's HOME).
        let fake_home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }
        // No user config => load_global returns defaults, ignoring repo.
        let cfg = TreehouseConfig::load_global().unwrap();
        assert_eq!(cfg.max_trees, 16);
        unsafe {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn resolve_pool_dir_empty_root() {
        let _guard = env_lock().lock().unwrap();
        let fake_home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }
        let repo = Path::new("/work/myrepo");
        let hash = crate::git::short_hash("https://github.com/x/y.git");
        let pool = resolve_pool_dir(repo, None, Some("https://github.com/x/y.git")).unwrap();
        assert_eq!(
            pool,
            fake_home
                .path()
                .join(".treehouse")
                .join(format!("myrepo-{hash}")),
            "pool dir should be $HOME/.treehouse/<repo>-<hash>"
        );
        unsafe {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn relative_root_nests_under_repo() {
        let _guard = env_lock().lock().unwrap();
        let repo = Path::new("/work/myrepo");
        let root = resolve_pool_root(repo, Some("worktrees")).unwrap();
        assert_eq!(root, Path::new("/work/myrepo/worktrees/.treehouse"));
        let dot = resolve_pool_root(repo, Some(".")).unwrap();
        assert_eq!(dot, Path::new("/work/myrepo/.treehouse"));
    }

    #[test]
    fn absolute_root_nests_under_given_root() {
        let _guard = env_lock().lock().unwrap();
        let repo = Path::new("/work/myrepo");
        let root = resolve_pool_root(repo, Some("/abs/root")).unwrap();
        assert_eq!(root, Path::new("/abs/root/.treehouse"));
    }

    #[test]
    fn relative_root_without_repo_fails() {
        let _guard = env_lock().lock().unwrap();
        let err = resolve_pool_root(Path::new(""), Some("worktrees")).unwrap_err();
        assert!(
            err.to_string().contains("requires a repository"),
            "got {err}"
        );
    }

    #[test]
    fn env_expansion_in_root() {
        let _guard = env_lock().lock().unwrap();
        let fake_home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("TESTVAR", fake_home.path().to_str().unwrap());
        }
        let repo = Path::new("/work/myrepo");
        let root = resolve_pool_root(repo, Some("$TESTVAR/trees")).unwrap();
        assert_eq!(root, fake_home.path().join("trees/.treehouse"));
        unsafe {
            std::env::remove_var("TESTVAR");
        }
    }

    #[test]
    fn lease_ttl_is_additive() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("treehouse.toml"),
            "max_trees = 8\nlease_ttl_secs = 3600\n",
        )
        .unwrap();
        let cfg = TreehouseConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.max_trees, 8);
        assert_eq!(cfg.lease_ttl_secs, Some(3600));
    }

    // ─── _with_env tests ────────────────────────────────────────────────────

    #[test]
    fn resolve_pool_root_with_env_empty_root() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/custom/pools"));
        let root = resolve_pool_root_with_env(Path::new("/work/repo"), None, &env).unwrap();
        assert_eq!(root, PathBuf::from("/custom/pools"));
    }

    #[test]
    fn resolve_pool_root_with_env_absolute_root() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/custom"));
        let root =
            resolve_pool_root_with_env(Path::new("/work/repo"), Some("/abs/root"), &env).unwrap();
        assert_eq!(root, PathBuf::from("/abs/root/.treehouse"));
    }

    #[test]
    fn resolve_pool_root_with_env_relative_root() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/custom"));
        let repo = Path::new("/work/myrepo");
        let root = resolve_pool_root_with_env(repo, Some("worktrees"), &env).unwrap();
        assert_eq!(root, Path::new("/work/myrepo/worktrees/.treehouse"));
    }

    #[test]
    fn resolve_pool_dir_with_env_uses_env_root() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/custom/pools"));
        let hash = crate::git::short_hash("https://github.com/x/y.git");
        let pool = resolve_pool_dir_with_env(
            Path::new("/work/myrepo"),
            None,
            Some("https://github.com/x/y.git"),
            &env,
        )
        .unwrap();
        assert_eq!(
            pool,
            PathBuf::from("/custom/pools").join(format!("myrepo-{hash}"))
        );
    }

    #[test]
    fn user_config_path_with_env_delegates_to_env() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        let path = user_config_path_with_env(&env).unwrap();
        assert_eq!(path, PathBuf::from("/test/config/config.toml"));
    }

    #[test]
    fn load_with_env_reads_repo_config() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        env.seed_file(Path::new("/test/repo/treehouse.toml"), b"max_trees = 8\n");
        let cfg = TreehouseConfig::load_with_env(Path::new("/test/repo"), &env).unwrap();
        assert_eq!(cfg.max_trees, 8);
    }

    #[test]
    fn load_with_env_merge_rules() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        // Seed repo config
        env.seed_file(Path::new("/test/repo/treehouse.toml"), b"max_trees = 4\n");
        // Seed user config with hooks (must include max_trees for TOML parse)
        env.seed_file(
            Path::new("/test/config/config.toml"),
            b"max_trees = 99\n[hooks]\npost_create = [\"./setup.sh\"]\n",
        );
        let cfg = TreehouseConfig::load_with_env(Path::new("/test/repo"), &env).unwrap();
        // Repo wins max_trees
        assert_eq!(cfg.max_trees, 4);
        // User provides hooks
        assert_eq!(cfg.hooks.post_create, ["./setup.sh"]);
    }

    #[test]
    fn load_with_env_defaults_without_config() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        let cfg = TreehouseConfig::load_with_env(Path::new("/test/empty"), &env).unwrap();
        assert_eq!(cfg.max_trees, 16); // default
        assert!(cfg.hooks.is_empty());
    }
}
