# Treehouse Custom Dot-Folder Refactor Plan

> **Goal:** Export a trait-based API from `treehouse-core` so consumers can inject their own persistence, path resolution, and config — eliminating all hardcoded `.treehouse` directories, config file paths, and env var dependencies.

---

## Current State

### Dot-folder locations (hardcoded)

| Path | Purpose | File:Line |
|---|---|---|
| `$HOME/.treehouse/` | Default pool root | `config.rs:164` |
| `<root>/.treehouse/` | Custom root pool | `config.rs:176,178` |
| `$HOME/.config/treehouse/config.toml` | User config | `config.rs:124-131` |
| `~/.treehouse/update-check.json` | Update cache | `updater.rs:92` |

### Env vars (hardcoded reads)

| Variable | Purpose | File |
|---|---|---|
| `HOME` / `USERPROFILE` | Path resolution | `config.rs:125,222` `updater.rs:88` |
| `TREEHOUSE_DIR` | Worktree path in child | `main.rs:143,189,208` `run.rs:117` |
| `TREEHOUSE_LEASE_ID` | Lease identity in child | `run.rs:118` |
| `TREEHOUSE_LEASE_HOLDER` | Lease holder fallback | `main.rs:104,635` |
| `TREEHOUSE_LEASE_TTL` | TTL fallback | `main.rs:110` |
| `TREEHOUSE_NO_UPDATE_CHECK` | Suppress update check | `main.rs:25,44` |
| `SHELL` / `COMSPEC` | Shell for subshells | `main.rs:550` `hooks.rs:93` |
| `GIT_BIN` | Git binary override | `git/shell.rs:117` |

### Architecture strengths (keep)

- `treehouse-core` is already a pure library — no `clap`, `anyhow`, `owo-colors`
- Every command returns structured `CommandResult` — CLI only formats
- `GitBackend` trait already exists (object-safe, future `gix` swap)
- `Pool` is the primary entrypoint — clean constructor pattern

---

## Design: `TreehouseEnv` Trait

### 1. New file: `crates/treehouse-core/src/env.rs`

```rust
use std::path::{Path, PathBuf};
use std::io;

/// Abstracts all filesystem and environment side-effects.
/// Consumer injects their own implementation — zero dot-folder, zero env vars.
pub trait TreehouseEnv: Send + Sync {
    /// Root directory for pool storage.
    /// Default: `$HOME/.treehouse`
    fn pool_root(&self) -> Option<PathBuf>;

    /// Path to the update-check cache file.
    /// Default: `$HOME/.treehouse/update-check.json`
    fn update_cache_path(&self) -> Option<PathBuf>;

    /// Path to the user-level config file.
    /// Default: `$HOME/.config/treehouse/config.toml`
    fn user_config_path(&self) -> Option<PathBuf>;

    /// Read a file's contents.
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

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}
```

### 2. Default implementation: `DefaultEnv`

```rust
/// Default implementation — identical to current behavior.
/// Uses real filesystem + env vars. For CLI usage.
pub struct DefaultEnv;

impl TreehouseEnv for DefaultEnv {
    fn pool_root(&self) -> Option<PathBuf> {
        home_dir().map(|h| h.join(".treehouse"))
    }

    fn update_cache_path(&self) -> Option<PathBuf> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)?;
        Some(home.join(".treehouse").join("update-check.json"))
    }

    fn user_config_path(&self) -> Option<PathBuf> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)?;
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
```

### 3. Test implementation: `InMemoryEnv`

```rust
use std::collections::HashMap;
use std::sync::RwLock;

/// In-memory environment for tests — zero filesystem side-effects.
pub struct InMemoryEnv {
    files: RwLock<HashMap<PathBuf, Vec<u8>>>,
    dirs: RwLock<HashMap<PathBuf, bool>>,
    env: HashMap<String, String>,
    pool_root: PathBuf,
    cache_dir: PathBuf,
    config_dir: PathBuf,
}

impl InMemoryEnv {
    pub fn new(pool_root: PathBuf) -> Self {
        Self {
            files: RwLock::new(HashMap::new()),
            dirs: RwLock::new(HashMap::new()),
            env: HashMap::new(),
            cache_dir: pool_root.join("cache"),
            config_dir: pool_root.join("config"),
            pool_root,
        }
    }

    pub fn with_env(mut self, key: &str, val: &str) -> Self {
        self.env.insert(key.to_string(), val.to_string());
        self
    }

    /// Seed a file for testing.
    pub fn seed_file(&self, path: &Path, content: &[u8]) {
        self.files.write().unwrap().insert(path.to_path_buf(), content.to_vec());
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
        files.get(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        let files = self.files.read().unwrap();
        files.get(path).cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.files.write().unwrap().insert(path.to_path_buf(), data.to_vec());
        if let Some(parent) = path.parent() {
            self.dirs.write().unwrap().insert(parent.to_path_buf(), true);
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
        let files = self.files.read().unwrap();
        let Ok(entries) = std::fs::read_dir(&prefix) else {
            return Ok(vec![]);
        };
        Ok(entries.filter_map(|e| e.ok()).map(|e| e.path()).collect())
    }

    fn file_meta(&self, path: &Path) -> io::Result<FileMeta> {
        let files = self.files.read().unwrap();
        files.get(path)
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
```

---

## Refactor Steps (ordered)

### Phase 1: Trait definition + wiring (no behavior change)

| Step | File | Change |
|---|---|---|
| 1.1 | `treehouse-core/src/env.rs` | **NEW** — `TreehouseEnv` trait, `DefaultEnv`, `InMemoryEnv`, `FileMeta` |
| 1.2 | `treehouse-core/src/lib.rs` | Add `pub mod env;` |
| 1.3 | `config.rs` | Add `resolve_pool_root_with_env(root, env: &dyn TreehouseEnv)` alongside existing fn |
| 1.4 | `config.rs` | Add `resolve_pool_dir_with_env(repo_root, root, remote_url, env)` |
| 1.5 | `config.rs` | Add `user_config_path_with_env(env)` |
| 1.6 | `config.rs` | Add `load_user_with_env(env)` |
| 1.7 | `config.rs` | Add `TreehouseConfig::load_with_env(repo_root, env)` |
| 1.8 | `discovery.rs` | Add `user_pool_root_with_env(env)` alongside existing fn |
| 1.9 | `discovery.rs` | Add `sweep_pools_with_env(user, env, ...)` |
| 1.10 | `updater.rs` | Add `cache_path_with_env(env)` |
| 1.11 | `updater.rs` | Add `read_cache_with_env(env)`, `write_cache_with_env(env, ...)` |
| 1.12 | `state_file.rs` | Add `write_state_with_env(pool_dir, state, env)` |
| 1.13 | `state.rs` | Add `State::read_state_with_env(pool_dir, env)` |
| 1.14 | `lock.rs` | Add `with_state_lock_with_env(pool_dir, env, ...)` |

> **Key:** All `_with_env` variants accept `&dyn TreehouseEnv` as their last parameter. Existing fns remain unchanged — backward compatible.

### Phase 2: Pool struct accepts env

| Step | File | Change |
|---|---|---|
| 2.1 | `pool.rs` | Add `env: Arc<dyn TreehouseEnv>` field to `Pool` |
| 2.2 | `pool.rs` | Add `Pool::open_with_env(repo_root, remote_url, opts, env)` |
| 2.3 | `pool.rs` | Wire `env` through all internal calls (`State::read_state_with_env`, `with_state_lock_with_env`, etc.) |
| 2.4 | `pool.rs` | Keep `Pool::open()` using `DefaultEnv` — backward compatible |
| 2.5 | `pool.rs` | Add `pub fn env(&self) -> &dyn TreehouseEnv` accessor |

### Phase 3: Core struct wrapper

| Step | File | Change |
|---|---|---|
| 3.1 | `treehouse-core/src/lib.rs` | Add `pub struct TreehouseCore<E: TreehouseEnv = DefaultEnv>` |
| 3.2 | `lib.rs` | Add `TreehouseCore::open(repo_root)` — CLI path, uses `DefaultEnv` |
| 3.3 | `lib.rs` | Add `TreehouseCore::with_env(env, config)` — library path |
| 3.4 | `lib.rs` | Add methods: `acquire()`, `release()`, `status()`, `prune()`, `destroy()`, `gc()` |
| 3.5 | `lib.rs` | Add `impl TreehouseCore<InMemoryEnv>` for test convenience |

```rust
pub struct TreehouseCore<E: TreehouseEnv = DefaultEnv> {
    env: E,
    config: TreehouseConfig,
}

impl TreehouseCore {
    /// CLI usage — zero config, DefaultEnv.
    pub fn open(repo_root: &Path) -> Result<Self, ConfigError> {
        let config = TreehouseConfig::load(repo_root)?;
        Ok(Self { env: DefaultEnv, config })
    }
}

impl<E: TreehouseEnv> TreehouseCore<E> {
    /// Library usage — consumer injects env.
    pub fn with_env(env: E, config: TreehouseConfig) -> Self {
        Self { env, config }
    }

    pub fn pool_root(&self) -> Option<PathBuf> {
        self.env.pool_root()
    }

    pub fn acquire(&self, repo_root: &Path, branch: Option<&str>) -> Result<Acquired, PoolError> {
        let remote = crate::git::origin_remote(repo_root)?;
        let pool = Pool::open_with_env(repo_root, remote.as_deref(), OpenOptions {
            config: self.config.clone(),
            ..Default::default()
        }, &self.env)?;
        pool.get(&AcquireOptions { branch: branch.map(String::from), ..Default::default() })
    }
    // ... more methods
}
```

### Phase 4: Consumer-facing API surface

```rust
use treehouse_core::{TreehouseCore, DefaultEnv, InMemoryEnv};

// ── CLI (unchanged) ──
let mut core = TreehouseCore::open(Path::new("."))?;
core.acquire(Some("main"))?;

// ── Library: custom dot-folder ──
struct MyEnv { base: PathBuf }
impl TreehouseEnv for MyEnv {
    fn pool_root(&self) -> Option<PathBuf> {
        Some(self.base.join("pools"))
    }
    fn update_cache_path(&self) -> Option<PathBuf> {
        Some(self.base.join("cache/update-check.json"))
    }
    fn user_config_path(&self) -> Option<PathBuf> {
        Some(self.base.join("config/treehouse.toml"))
    }
    // ... read/write delegates to consumer's storage
}

let core = TreehouseCore::with_env(
    MyEnv { base: PathBuf::from("/tmp/my-app") },
    TreehouseConfig { max_trees: 4, ..Default::default() },
);

// ── Library: zero filesystem (tests, WASM) ──
let core = TreehouseCore::with_env(
    InMemoryEnv::new(PathBuf::from("/test")),
    TreehouseConfig::default(),
);
```

### Phase 5: CLI wiring (optional, minimal)

| Step | File | Change |
|---|---|---|
| 5.1 | `main.rs` | No changes — `TreehouseCore::open()` uses `DefaultEnv` automatically |
| 5.2 | `main.rs` | Add `--env-path <DIR>` flag (optional) for users who want custom pool root without config file |

---

## Files Modified

| File | Phase | Nature of change |
|---|---|---|
| `treehouse-core/src/env.rs` | 1 | **NEW** — trait + impls |
| `treehouse-core/src/lib.rs` | 1,3 | Add `pub mod env;` + `TreehouseCore` struct |
| `treehouse-core/src/config.rs` | 1 | Add `_with_env` variants (existing fns untouched) |
| `treehouse-core/src/discovery.rs` | 1 | Add `_with_env` variants |
| `treehouse-core/src/updater.rs` | 1 | Add `_with_env` variants |
| `treehouse-core/src/state_file.rs` | 1 | Add `_with_env` variant |
| `treehouse-core/src/state.rs` | 1 | Add `_with_env` variant |
| `treehouse-core/src/lock.rs` | 1 | Add `_with_env` variant |
| `treehouse-core/src/pool.rs` | 2 | Add env field + `open_with_env` |
| `treehouse-core/src/gc.rs` | 2 | Wire env through sweep |
| `treehouse-core/src/prune.rs` | 2 | Wire env through sweep |
| `treehouse-core/src/destroy.rs` | 2 | Wire env through destroy |
| `treehouse-core/src/doctor.rs` | 2 | Wire env through health checks |
| `treehouse/src/main.rs` | 5 | Optional `--env-path` flag |

---

## Backward Compatibility

| Consumer | Impact |
|---|---|
| Existing CLI users | **Zero** — `DefaultEnv` is identical to current behavior |
| Existing library users (via `Pool::open`) | **Zero** — `Pool::open()` wraps `DefaultEnv` |
| `treehouse init` | **Zero** — creates `treehouse.toml` in repo root, not in dot-folder |
| `treehouse.toml` config | **Zero** — `root` field still works, now also overridable via `TreehouseEnv` |
| Env vars | **Zero** — `DefaultEnv` reads them; custom envs can ignore them |

---

## Testing Strategy

### Unit tests per phase

- **Phase 1:** `InMemoryEnv` round-trip tests for each `_with_env` fn
- **Phase 2:** `Pool::open_with_env` with `InMemoryEnv` — acquire/release cycle
- **Phase 3:** `TreehouseCore::with_env` end-to-end with `InMemoryEnv`

### Integration tests

```rust
#[test]
fn custom_env_pool_acquire_release() {
    let env = InMemoryEnv::new(PathBuf::from("/test/pools"));
    let config = TreehouseConfig { max_trees: 4, ..Default::default() };
    let core = TreehouseCore::with_env(env, config);

    // Seed a fake git repo state
    core.env().seed_file(Path::new("/test/repo/.git/HEAD"), b"ref: refs/heads/main");

    let acquired = core.acquire(Path::new("/test/repo"), None).unwrap();
    assert!(!acquired.name.is_empty());

    let released = core.release(&acquired.path).unwrap();
    assert!(released);
}
```

### Regression tests

- Run existing test suite with `DefaultEnv` — must pass unchanged
- Run existing test suite with `InMemoryEnv` — must produce same logical results

---

## Migration Path

```
Week 1: Phase 1 (trait + _with_env variants)     ← pure additive, no breakage
Week 2: Phase 2 (Pool env field)                  ← internal, backward compatible
Week 3: Phase 3 (TreehouseCore wrapper)           ← new public API
Week 4: Phase 4 (consumer examples + docs)        ← documentation
Week 5: Phase 5 (optional CLI flag)               ← low priority
```

---

## Risk Assessment

| Risk | Mitigation |
|---|---|
| `dyn TreehouseEnv` trait object overhead | Acceptable — env calls are rare (file I/O dominates) |
| Atomic write in `InMemoryEnv` | Simulate with `HashMap::insert` — same semantics |
| Lock file in `InMemoryEnv` | Skip lock (single-threaded test env) or use `InMemoryLock` |
| Breaking existing tests | Phase 1 is purely additive; existing fns untouched |
| Env var leakage in tests | `InMemoryEnv` never touches `std::env` |

---

## Open Questions

1. **Lock abstraction:** Should `TreehouseEnv` include `fn lock(&self, path: &Path) -> Box<dyn LockGuard>`? Or keep `fd_lock` as an implementation detail of `DefaultEnv`?

2. **Atomic writes:** `state_file.rs` uses `tempfile::NamedTempFile::persist()` for atomic writes. Should `TreehouseEnv::write_file` guarantee atomicity, or should it be a separate `fn write_file_atomic`?

3. **Error types:** Should `TreehouseEnv` methods return `std::io::Result` or a custom `EnvError` enum?

4. **Git backend:** The `GitBackend` trait already exists. Should `TreehouseEnv` subsume it, or keep them orthogonal?

---

*Plan version: 2026-08-17 | Author: treehouse-core | Status: Draft*
