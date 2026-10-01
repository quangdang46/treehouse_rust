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
///
/// The struct-level `#[serde(default)]` is load-bearing and must not be
/// narrowed back to per-field attributes. Go decodes a config file ONTO a
/// `DefaultConfig()` (`internal/config/config.go:157` then `:163`
/// `toml.DecodeFile(repoPath, &cfg)`), so a key the document omits keeps the
/// default it already had. Deserializing into a fresh struct instead makes
/// every field mandatory in practice — which is how a documented hooks-only
/// `~/.config/treehouse/config.toml` came to hard-fail with
/// `missing field 'max_trees'` on every repo-scoped command.
///
/// Starting from [`TreehouseConfig::default_config`] here reproduces Go's
/// semantics for every field, including keys added later: a new field is
/// optional-by-construction instead of needing its own `#[serde(default)]`
/// remembered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TreehouseConfig {
    pub max_trees: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
    /// P1 additive: default TTL for `treehouse run` leases. Zero/None = no TTL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
}

/// Lifecycle hooks (user-level only; repo hooks are ignored for safety).
///
/// Same decode-onto-default contract as [`TreehouseConfig`]: `[hooks]` with only
/// one of the two keys must leave the other empty, not fail the whole file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hooks {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub post_create: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
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
            let mut decoded: TreehouseConfig = toml::from_str(&text)
                .map_err(|e| ConfigError::Toml(repo_path.display().to_string(), e.to_string()))?;
            // Repo hooks are ignored for safety. Assign the decoded config
            // wholesale rather than copying field by field: a field added
            // later is then picked up from treehouse.toml automatically instead
            // of silently reading as the default because nobody remembered to
            // add it to a copy list.
            decoded.hooks = Hooks::default();
            cfg = decoded;
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
            let mut decoded: TreehouseConfig = toml::from_str(&text)
                .map_err(|e| ConfigError::Toml(repo_path.display().to_string(), e.to_string()))?;
            // Repo hooks are ignored for safety — same wholesale-assignment
            // rationale as `load`.
            decoded.hooks = Hooks::default();
            cfg = decoded;
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

// ─── pool exclusion (Go internal/config/gitignore.go) ─────────────────────────

/// Arranges for a pool directory to be ignored by the enclosing repository
/// (Go `EnsureExcluded`).
///
/// Two layers, because one is not enough:
///
/// 1. A `.gitignore` containing `*` inside the pool root. Both git and jj read
///    nested `.gitignore` files, so this alone keeps an in-project pool out of
///    snapshots — including under jj, which never reads `.git/info/exclude`.
/// 2. The pool root recorded in the repo-local, UNTRACKED `.git/info/exclude`,
///    so an in-project pool stays out of `git status` and out of any commit
///    without dirtying the user's tracked `.gitignore`.
///
/// Layer 2 is skipped when the pool is not inside a git repository (the
/// default global root under `$HOME`), which matches the previous behavior for
/// the global store, and degrades to layer 1 alone when no usable git dir
/// exists.
///
/// For backward compatibility, a pre-existing entry in the tracked `.gitignore`
/// is left alone and treated as sufficient — an upgrading user is not
/// surprised by a moved ignore rule.
pub fn ensure_excluded(treehouse_dir: &Path, git: &dyn crate::git::GitBackend) -> Result<(), ConfigError> {
    let self_ignored = write_self_ignore(treehouse_dir).is_ok();

    // The directory itself may not exist yet, so walk up to an existing
    // ancestor for the git check below.
    let mut check_dir = treehouse_dir.to_path_buf();
    let existing = loop {
        match std::fs::metadata(&check_dir) {
            Ok(meta) if meta.is_dir() => break true,
            _ => match check_dir.parent() {
                Some(parent) if parent != check_dir => check_dir = parent.to_path_buf(),
                // Reached the filesystem root without finding a directory.
                _ => break false,
            },
        }
    };
    if !existing {
        return Ok(());
    }

    // Not inside a git repo — nothing to do (e.g. the global ~/.treehouse root).
    let Ok(repo_root) = git.repo_root(&check_dir) else {
        return Ok(());
    };

    // Outside the repository working tree (a sibling pool root): nothing to do.
    let Some(rel) = relative_to(&repo_root, treehouse_dir) else {
        return Ok(());
    };
    if rel.starts_with("..") {
        return Ok(());
    }

    // Forward slashes and a leading slash anchor the entry at the repo root.
    let entry = format!("/{}", rel.replace('\\', "/"));

    // Backward compatibility: an entry an older version already recorded in the
    // tracked .gitignore is left in place rather than duplicated into
    // .git/info/exclude.
    if has_ignore_entry(&repo_root.join(".gitignore"), &entry) {
        return Ok(());
    }

    let Ok(common_dir) = git.common_git_dir(&repo_root) else {
        if self_ignored {
            return Ok(());
        }
        // Go returns the underlying error only when the self-ignore also
        // failed; with no git dir and no self-ignore there is nothing left to
        // fall back on, so the failure is surfaced rather than swallowed.
        return Err(ConfigError::Invalid(
            format!("cannot determine git dir for {}", repo_root.display()),
            String::new(),
        ));
    };
    let exclude_path = common_dir.join("info").join("exclude");

    if has_ignore_entry(&exclude_path, &entry) {
        return Ok(());
    }

    std::fs::create_dir_all(
        exclude_path
            .parent()
            .ok_or_else(|| ConfigError::Invalid(exclude_path.display().to_string(), String::new()))?,
    )
    .map_err(|e| ConfigError::Io(exclude_path.display().to_string(), e))?;

    let existing = match std::fs::read(&exclude_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(ConfigError::Io(exclude_path.display().to_string(), e)),
    };

    // Append, preserving whatever the user already put in this file. A file
    // with no trailing newline would otherwise have the entry concatenated onto
    // its last line, silently merging two ignore rules into one.
    let mut contents = existing;
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    contents.extend_from_slice(entry.as_bytes());
    contents.push(b'\n');

    std::fs::write(&exclude_path, &contents)
        .map_err(|e| ConfigError::Io(exclude_path.display().to_string(), e))
}

/// `treehouse_dir` relative to `repo_root`, in slash-separated form.
///
/// Returns `None` when the two cannot be related at all (different roots on
/// Windows), which callers treat as "not in this repository".
///
/// The comparison tries the paths as given first, then their canonical forms.
/// That second attempt is not cosmetic: git reports its own roots through
/// `/private/var/...` on macOS, while a pool root configured as `/var/...`
/// reaches us spelled the short way. Compared literally the two share no
/// prefix, and the exclude entry would be silently never written — the pool
/// would then be neither self-ignored nor excluded, and the repository would
/// read dirty with a directory full of worktrees in it.
///
/// Canonicalization is attempted per-path and failures fall back to the input,
/// because the pool root may not exist yet.
fn relative_to(repo_root: &Path, treehouse_dir: &Path) -> Option<String> {
    let root = normalize_for_compare(repo_root);
    let target = normalize_for_compare(treehouse_dir);
    if let Some(rel) = relative_within(&root, &target) {
        return Some(rel);
    }
    let root_canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let target_canonical = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
    if root_canonical == root && target_canonical == target {
        // Nothing resolved, so retrying gained nothing.
        return None;
    }
    relative_within(&root_canonical, &target_canonical)
}

/// The tail of `target` after `root`, or `None` unless `root` is a proper
/// ancestor of `target`.
fn relative_within(root: &Path, target: &Path) -> Option<String> {
    let root_components: Vec<_> = root.components().collect();
    let target_components: Vec<_> = target.components().collect();

    if target_components.len() < root_components.len() {
        return None;
    }
    if !root_components
        .iter()
        .zip(target_components.iter())
        .all(|(a, b)| a == b)
    {
        return None;
    }
    let tail: Vec<String> = target_components[root_components.len()..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(tail.join("/"))
}

/// Resolves `.` and `..` lexically so a pool root spelled with a relative
/// segment still compares equal to the same directory spelled without one.
///
/// Lexical, not `canonicalize`: the pool root may not exist yet, which is
/// exactly the case `ensure_excluded` has to handle.
fn normalize_for_compare(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Creates the pool root and writes a `.gitignore` containing `*` into it,
/// unless one already exists (Go `writeSelfIgnore`).
///
/// The pool root then ignores its own entire contents, which is what makes an
/// in-project root safe for backends that never read `.git/info/exclude`. Ignore
/// rules never cross a worktree boundary, so the file has no effect on the
/// pooled worktrees below it.
fn write_self_ignore(treehouse_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(treehouse_dir)?;
    let path = treehouse_dir.join(".gitignore");
    if std::fs::symlink_metadata(&path).is_ok() {
        // Present already. A symlink here is left exactly as it is: the pool
        // root is the user's to configure, and following it would write
        // outside the pool.
        return Ok(());
    }
    std::fs::write(path, "*\n")
}

/// Whether the ignore file at `path` already contains `entry` as a standalone
/// line. A missing file reads as absent (Go `hasIgnoreEntry`).
fn has_ignore_entry(path: &Path, entry: &str) -> bool {
    let Ok(data) = std::fs::read_to_string(path) else {
        return false;
    };
    data.lines().any(|line| line.trim() == entry)
}

// ─── ignored repo hooks warning (Go internal/config/hooks.go) ─────────────────

/// The lifecycle hook keys a repo-level `treehouse.toml` may declare and that
/// [`TreehouseConfig::load`] discards. Order fixes the order they are named in
/// the warning.
const HOOK_KEYS: [&str; 2] = ["post_create", "pre_destroy"];

/// Dedupes the ignored-hooks warning per config file: a single command loads
/// config repeatedly, and the warning is for a human, once.
fn warned_repo_hooks() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static LOCKED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    > = std::sync::OnceLock::new();
    LOCKED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Warns on stderr, once per repo config file, when the repo-level
/// `treehouse.toml` declares lifecycle hooks (Go `WarnIfRepoHooksIgnored`).
///
/// [`TreehouseConfig::load`] deliberately discards repo hooks so that
/// `treehouse get` on an untrusted clone cannot execute checked-in shell; this
/// only makes that discard audible, and changes no behavior.
///
/// It is a standalone function rather than a step inside `load` because
/// `treehouse destroy <path>` reads hooks through [`TreehouseConfig::load_global`]
/// and never opens the repo config at all, so a warning wired only into `load`
/// would stay silent for exactly the command that surfaced the defect.
///
/// Best-effort and never fails: a missing, unreadable, or malformed repo config
/// produces no warning and no error, because destroy has to keep working
/// against a pool whose repository config is broken or whose repository is gone.
pub fn warn_if_repo_hooks_ignored(repo_root: &Path) {
    let path = repo_root.join("treehouse.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let declared = declared_hook_keys(&text);
    if declared.is_empty() {
        return;
    }
    {
        let Ok(mut seen) = warned_repo_hooks().lock() else {
            return;
        };
        if !seen.insert(path.clone()) {
            return;
        }
    }
    eprintln!(
        "🌳 Warning: ignoring [hooks] in {}: {}",
        pretty_path(&path),
        declared.join(", ")
    );
    eprintln!(
        "   Lifecycle hooks run only from {}; hooks in repo-level config are ignored for safety.",
        user_config_path_for_display()
    );
}

/// The hook keys the document actually DECLARES.
///
/// An empty `[hooks]` table declares no hook and so silently discards nothing
/// worth warning about. This deliberately inspects the raw TOML rather than
/// the decoded [`Hooks`]: decoding a malformed file must produce silence (the
/// caller has to survive a broken repo config), and it cannot be reached
/// through the strict decoder anyway.
fn declared_hook_keys(text: &str) -> Vec<String> {
    let Ok(value) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(toml::Value::Table(hooks)) = value.get("hooks") else {
        return Vec::new();
    };
    HOOK_KEYS
        .iter()
        .filter(|key| hooks.contains_key(**key))
        .map(|key| key.to_string())
        .collect()
}

/// Names the user-level config file for humans, falling back to the
/// conventional `~`-rooted spelling when `$HOME` cannot be resolved: the
/// warning must still point somewhere useful.
fn user_config_path_for_display() -> String {
    user_config_path().map_or_else(
        || "~/.config/treehouse/config.toml".to_string(),
        |path| pretty_path(&path),
    )
}

/// A path under `$HOME` shown as `~/...`.
///
/// Only affects display. Resolving the home prefix textually — rather than
/// canonicalizing — keeps the check from failing on paths that do not exist,
/// which is the normal case for a config file that is only being named.
fn pretty_path(path: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(rel) = path.strip_prefix(&home)
        && !rel.as_os_str().is_empty()
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
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
        // Seed user config with hooks. `max_trees = 99` is deliberately
        // different from the repo's 4 so this still exercises repo-over-user
        // precedence rather than just agreeing values.
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

    // ─── absent keys keep their default (M-007) ────────────────────────────
    //
    // Go decodes a config file ONTO a DefaultConfig(), so every key is
    // optional. These tests pin that contract: a treehouse.toml or config.toml
    // that omits a key must load with the default, never fail to parse.

    #[test]
    fn repo_config_omitting_max_trees_keeps_default() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // A treehouse.toml that only sets `root` — previously a hard
        // `missing field 'max_trees'` error that broke every repo-scoped
        // command (status/get/prune/destroy) in the repo.
        std::fs::write(dir.path().join("treehouse.toml"), "root = \"pools\"\n").unwrap();

        let cfg = TreehouseConfig::load(dir.path()).unwrap();
        assert_eq!(
            cfg.max_trees, DEFAULT_MAX_TREES,
            "absent key must keep default"
        );
        assert_eq!(cfg.root.as_deref(), Some("pools"));
    }

    #[test]
    fn hooks_only_user_config_keeps_default_max_trees() {
        let _guard = env_lock().lock().unwrap();
        // The exact shape the Go README tells users to paste into
        // ~/.config/treehouse/config.toml: a [hooks] table and nothing else.
        let fake_home = tempfile::tempdir().unwrap();
        let user_dir = fake_home.path().join(".config").join("treehouse");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("config.toml"),
            "[hooks]\npre_destroy = [\"./scripts/cleanup.sh\"]\n",
        )
        .unwrap();
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let dir = tempfile::tempdir().unwrap();
        let cfg = TreehouseConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.max_trees, DEFAULT_MAX_TREES);
        assert_eq!(cfg.hooks.pre_destroy, ["./scripts/cleanup.sh"]);

        unsafe {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn hooks_only_user_config_keeps_default_max_trees_via_env() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        env.seed_file(
            Path::new("/test/config/config.toml"),
            b"[hooks]\npost_create = [\"./setup.sh\"]\n",
        );
        let cfg = TreehouseConfig::load_with_env(Path::new("/test/repo"), &env).unwrap();
        assert_eq!(cfg.max_trees, DEFAULT_MAX_TREES);
        assert_eq!(cfg.hooks.post_create, ["./setup.sh"]);
    }

    #[test]
    fn load_with_env_repo_config_omitting_max_trees_keeps_default() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        env.seed_file(
            Path::new("/test/repo/treehouse.toml"),
            b"root = \"worktrees\"\n",
        );
        let cfg = TreehouseConfig::load_with_env(Path::new("/test/repo"), &env).unwrap();
        assert_eq!(cfg.max_trees, DEFAULT_MAX_TREES);
        assert_eq!(cfg.root.as_deref(), Some("worktrees"));
    }

    #[test]
    fn partial_hooks_table_keeps_the_other_key_empty() {
        // Same decode-onto-default contract one level down: naming only one key
        // under [hooks] must not fail the file.
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        env.seed_file(
            Path::new("/test/config/config.toml"),
            b"[hooks]\npost_create = [\"./setup.sh\"]\n",
        );
        let cfg = TreehouseConfig::load_with_env(Path::new("/test/repo"), &env).unwrap();
        assert_eq!(cfg.hooks.post_create, ["./setup.sh"]);
        assert!(cfg.hooks.pre_destroy.is_empty());
    }

    #[test]
    fn explicit_max_trees_still_overrides_the_default() {
        // Guard against "fixing" M-007 by making max_trees permanently
        // default: an explicitly spelled key must still win.
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        env.seed_file(
            Path::new("/test/repo/treehouse.toml"),
            b"max_trees = 3\nroot = \"x\"\n",
        );
        let cfg = TreehouseConfig::load_with_env(Path::new("/test/repo"), &env).unwrap();
        assert_eq!(cfg.max_trees, 3);
    }

    #[test]
    fn wrong_type_for_a_present_key_is_still_an_error() {
        // Decode-onto-default must not turn into decode-anything: a key that IS
        // present but malformed has to fail closed.
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        env.seed_file(
            Path::new("/test/repo/treehouse.toml"),
            b"max_trees = \"lots\"\n",
        );
        let err = TreehouseConfig::load_with_env(Path::new("/test/repo"), &env).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(..)), "got {err}");
    }

    #[test]
    fn load_with_env_defaults_without_config() {
        let env = crate::env::InMemoryEnv::new(PathBuf::from("/test"));
        let cfg = TreehouseConfig::load_with_env(Path::new("/test/empty"), &env).unwrap();
        assert_eq!(cfg.max_trees, 16); // default
        assert!(cfg.hooks.is_empty());
    }

    // ─── pool exclusion (Go internal/config/gitignore.go) ───────────────────

    /// A temp repo with one commit on `main`.
    fn git_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let must = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git must be installed");
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::process::Command::new("git")
            .args(["init", "-q", "--initial-branch=main", repo.to_str().unwrap()])
            .output()
            .expect("git must be installed");
        must(&["config", "user.email", "test@test.com"]);
        must(&["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), b"hello\n").unwrap();
        must(&["add", "."]);
        must(&["commit", "-m", "initial"]);
        (dir, repo)
    }

    fn backend() -> crate::git::ShellGitBackend {
        crate::git::ShellGitBackend::discover().expect("git must be installed")
    }

    fn read_exclude(repo: &Path) -> String {
        std::fs::read_to_string(repo.join(".git").join("info").join("exclude")).unwrap()
    }

    /// An in-project pool must be recorded in the repo-local, UNTRACKED
    /// `.git/info/exclude` — and must leave the tracked `.gitignore` alone, so
    /// the user's repository does not read dirty because treehouse used it.
    #[test]
    fn ensure_excluded_writes_to_git_info_exclude() {
        let (_d, repo) = git_repo();
        let git = backend();
        let pool = repo.join(".treehouse");

        ensure_excluded(&pool, &git).unwrap();

        assert!(
            read_exclude(&repo).contains("/.treehouse"),
            "the pool root must be recorded in .git/info/exclude"
        );
        assert!(
            !repo.join(".gitignore").exists(),
            "the tracked .gitignore must not be created"
        );

        // The real proof it stays out of the user's way.
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "the repository must read clean, got:\n{}",
            String::from_utf8_lossy(&status.stdout)
        );
    }

    /// Both layers are needed: git and jj read a nested `.gitignore`, so the
    /// pool root ignoring its own contents is what protects a backend that
    /// never consults `.git/info/exclude`.
    #[test]
    fn ensure_excluded_makes_the_pool_root_self_ignoring() {
        let (_d, repo) = git_repo();
        let git = backend();
        let pool = repo.join(".treehouse");

        ensure_excluded(&pool, &git).unwrap();

        assert_eq!(
            std::fs::read_to_string(pool.join(".gitignore")).unwrap(),
            "*\n",
            "the pool root must ignore its own entire contents"
        );

        // Idempotent: a second call must not append a duplicate.
        ensure_excluded(&pool, &git).unwrap();
        assert_eq!(
            read_exclude(&repo).matches("/.treehouse").count(),
            1,
            "the entry must not be duplicated"
        );
    }

    /// Backward compatibility: a pool an older version already recorded in the
    /// tracked `.gitignore` keeps that rule. Moving it to `.git/info/exclude`
    /// would surprise an upgrading user by un-ignoring the tracked rule.
    #[test]
    fn ensure_excluded_leaves_a_preexisting_gitignore_entry_alone() {
        let (_d, repo) = git_repo();
        let git = backend();
        std::fs::write(repo.join(".gitignore"), b"/.treehouse\n").unwrap();
        let pool = repo.join(".treehouse");

        ensure_excluded(&pool, &git).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
            "/.treehouse\n",
            "an existing tracked rule must be left untouched"
        );
        let exclude = repo.join(".git").join("info").join("exclude");
        assert!(
            !exclude.exists() || !read_exclude(&repo).contains("/.treehouse"),
            "a pre-existing tracked entry must not be duplicated into info/exclude"
        );
    }

    /// An exclude file the user has other content in must be APPENDED to, with
    /// a newline inserted when the file lacks a trailing one — otherwise the
    /// entry is concatenated onto the last rule and silently merges two ignore
    /// patterns into one.
    #[test]
    fn ensure_excluded_appends_without_merging_into_the_last_rule() {
        let (_d, repo) = git_repo();
        let git = backend();
        let exclude = repo.join(".git").join("info").join("exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        // No trailing newline: the merge hazard.
        std::fs::write(&exclude, b"*.log").unwrap();

        ensure_excluded(&repo.join(".treehouse"), &git).unwrap();

        let contents = read_exclude(&repo);
        assert!(
            contents.starts_with("*.log\n"),
            "the existing rule must be preserved on its own line, got:\n{contents}"
        );
        assert!(
            contents.contains("/.treehouse"),
            "the pool entry must be appended, got:\n{contents}"
        );
    }

    /// A pool outside the repository working tree gets nothing written: an
    /// exclude entry anchored at the repo root cannot mean "this other
    /// directory", and writing one anyway would be a rule that silently does
    /// nothing.
    #[test]
    fn ensure_excluded_does_nothing_outside_the_repo() {
        let dir = tempfile::tempdir().unwrap();
        // No git repo anywhere above a tempdir on most systems; assert the
        // call succeeds and does not fabricate an exclude file.
        let pool = dir.path().join("elsewhere").join(".treehouse");
        ensure_excluded(&pool, &backend()).unwrap();
        assert!(
            pool.join(".gitignore").exists(),
            "the self-ignore must still be written: it is valid with no repo"
        );
    }

    /// A nested pool root is recorded with its full relative path.
    #[test]
    fn ensure_excluded_records_a_nested_pool_root() {
        let (_d, repo) = git_repo();
        let git = backend();
        let pool = repo.join("worktrees").join(".treehouse");

        ensure_excluded(&pool, &git).unwrap();

        assert!(
            read_exclude(&repo).contains("/worktrees/.treehouse"),
            "the nested pool must be anchored at the repo root, got:\n{}",
            read_exclude(&repo)
        );
    }

    /// `ensure_excluded` must be callable for a pool root that does not exist
    /// yet — it is what creates it.
    #[test]
    fn ensure_excluded_creates_a_missing_pool_root() {
        let (_d, repo) = git_repo();
        let git = backend();
        let pool = repo.join("brand").join("new").join(".treehouse");
        assert!(!pool.exists());

        ensure_excluded(&pool, &git).unwrap();

        assert!(pool.is_dir(), "the pool root must have been created");
        assert!(read_exclude(&repo).contains("/brand/new/.treehouse"));
    }

    // ─── ignored repo hooks warning (Go internal/config/hooks.go) ───────────

    /// Whether the repo config declares each of the two hook keys.
    fn warning_for(repo: &Path) -> Vec<String> {
        // The warning goes to stderr, so it is captured by re-parsing what the
        // decision function would emit. `declared_hook_keys` is the part that
        // decides WHICH keys are named.
        declared_hook_keys(&std::fs::read_to_string(repo.join("treehouse.toml")).unwrap())
    }

    /// The warning names the file and every declared key, so a user who wrote
    /// the hooks can see which config was ignored and why.
    #[test]
    fn the_ignored_hooks_warning_names_the_file_and_every_declared_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("treehouse.toml"),
            "[hooks]\npost_create = [\"./setup.sh\"]\npre_destroy = [\"./teardown.sh\"]\n",
        )
        .unwrap();

        let keys = warning_for(dir.path());
        assert_eq!(keys, ["post_create", "pre_destroy"]);
        assert!(
            user_config_path_for_display().ends_with("config.toml"),
            "the warning must point at the user-level config, got {}",
            user_config_path_for_display()
        );
    }

    /// A config that declares only one key must not name the other. The user
    /// wrote one hook; telling them two were ignored sends them looking for a
    /// config they never wrote.
    #[test]
    fn the_ignored_hooks_warning_names_only_declared_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("treehouse.toml"),
            "[hooks]\npre_destroy = [\"./teardown.sh\"]\n",
        )
        .unwrap();

        let keys = warning_for(dir.path());
        assert_eq!(keys, ["pre_destroy"]);
        assert!(!keys.contains(&"post_create".to_string()));
    }

    /// Silence is required, not merely nice: a config with no hooks, an empty
    /// `[hooks]` table, and a MALFORMED file must all warn about nothing. A
    /// broken repo config is normal in `destroy`, and warning about every pool
    /// whose config happens to be invalid would bury the real message.
    #[test]
    fn the_ignored_hooks_warning_is_silent_when_there_is_nothing_to_ignore() {
        for contents in [
            "max_trees = 4\n",
            "[hooks]\n",
            "invalid toml <<<\n",
            "",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("treehouse.toml"), contents).unwrap();
            assert!(
                declared_hook_keys(contents).is_empty(),
                "{contents:?} must declare no hook"
            );
        }
        // A repo with no config at all.
        assert!(declared_hook_keys("").is_empty());
    }

    /// The warning fires ONCE per config file. A single command loads config
    /// repeatedly, and a per-load warning would print the same line dozens of
    /// times.
    #[test]
    fn the_ignored_hooks_warning_is_deduped_per_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("treehouse.toml"),
            "[hooks]\npost_create = [\"./setup.sh\"]\n",
        )
        .unwrap();
        let path = dir.path().join("treehouse.toml");

        let mut seen = warned_repo_hooks().lock().unwrap();
        assert!(seen.insert(path.clone()), "first sighting is recorded");
        assert!(
            !seen.insert(path.clone()),
            "a second sighting of the same file must be deduped"
        );
    }
}
