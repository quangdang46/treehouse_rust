//! The pool orchestrator — the public entrypoint to treehouse-core.
//!
//! Implements acquire (`get`), release (`return`), `release_conditional`, and
//! status (`list`).
//!
//! **Short-lock protocol** (the audit fix): git/hooks/process work runs
//! OUTSIDE the state lock. The reservation is stamped BEFORE the external
//! reset and re-validated AFTER (persisted reservation token). Go held the
//! lock during `git reset` (correct but stalls every other command); the Rust
//! port deliberately does not.
//!
//! The reservation IS the anti-TOCTOU wall: stamp it before the external
//! reset, keep it across return, re-validate at commit. A bare non-treehouse
//! process cd'ing in during reset is unprotected (only cooperative consumers
//! coordinated) — residual, documented, same as Go.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{TreehouseConfig, resolve_pool_dir_with_env};
use crate::env::{DefaultEnv, TreehouseEnv};
use crate::git::{GitBackend, GitError, GitErrorKind, GitRepo};
use crate::hooks;
use crate::lease::{Lease, LeaseInfo, mark_acquired_lease};
use crate::lock::{DEFAULT_LOCK_TIMEOUT, LockError, with_state_lock};
use crate::process::{ProcessInfo, ProcessTable};
use crate::reservation;
use crate::state::{SeedInventory, State, WorktreeEntry, ZERO_TIME, heal_state};
use crate::state_file;
use crate::vcs;

/// Options for opening a pool.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    pub config: TreehouseConfig,
    pub lock_timeout: std::time::Duration,
    /// The `--root` flag: a pool root the caller named explicitly.
    ///
    /// A separate tier from `config.root` because Go's precedence is
    /// `flag > TREEHOUSE_ROOT > treehouse.toml root > ~/.treehouse`
    /// (`config.ResolveRoot`, config.go:110-117) and the env var is read
    /// inside `open_with_env`, below where a flag belongs. Folding the flag
    /// into `config.root` would let `TREEHOUSE_ROOT` silently outrank
    /// something the user typed on the command line.
    pub root_override: Option<String>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            config: TreehouseConfig::default_config(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            root_override: None,
        }
    }
}

/// Options for acquiring a worktree (`get`).
#[derive(Debug, Clone, Default)]
pub struct AcquireOptions {
    /// The branch a worktree is cut from and reset to — Go's `BaseBranch`,
    /// NOT Go's `Branch` (which *creates* a branch). The two are separate
    /// options upstream because "cut from X" and "check out new Y" are
    /// separate decisions; conflating them is how a caller ends up resetting
    /// every acquired slot to a branch it meant to create.
    pub branch: Option<String>,
    /// Skip the `origin` fetch (Go `SkipFetch`, `--no-fetch`). For an
    /// air-gapped or offline acquisition, where the fetch would fail the
    /// whole command over a ref the worktree could have been cut from
    /// locally.
    pub skip_fetch: bool,
    /// CREATE and check out a new branch at the acquired commit (Go `Branch`,
    /// `-b`). Distinct from [`Self::branch`], which is where the worktree is
    /// cut FROM: this is the branch that comes into existence. The two are
    /// separate options upstream because "cut from X" and "check out new Y"
    /// are separate decisions.
    pub new_branch: Option<String>,
    /// Name a newly created worktree's directory `<repo>-<slot>` instead of
    /// `<repo>` (Go `UniqueLeaf`, `--unique-leaf`). Creation-time naming only —
    /// it never moves a worktree that already exists.
    pub unique_leaf: bool,
    /// Template for a newly created worktree's directory (Go `WorktreePath`,
    /// `--worktree-path`), e.g. `{pool}/{slot}/{repo}`. `None` keeps the
    /// built-in layout. A template supersedes `unique_leaf` because it names
    /// every segment of the path including the leaf.
    pub worktree_path: Option<String>,
    /// Lease the worktree (non-interactive) instead of an owner reservation.
    pub lease: Option<LeaseAcquireOptions>,
    /// Replace the committed `.worktreeinclude` for THIS acquisition (Go
    /// `IncludeManifest`, `--include-file`).
    ///
    /// `None` uses the manifest committed at the destination worktree's HEAD.
    /// `Some(bytes)` — including `Some(&[])` — replaces it, so an empty file
    /// means "seed nothing" and must NOT fall back to the committed one.
    pub include_manifest: Option<Vec<u8>>,
    /// Opt in to copy-on-write sharing of tracked file data for a FRESH slot
    /// (Go `APFSSharing`, `--apfs-sharing fresh`). Defaults to off.
    ///
    /// Only ever applied to a slot this acquisition just created. A recycled
    /// slot may still have external writers, and neither reuse nor return is
    /// an invitation to sweep it.
    pub apfs_sharing: bool,
}

/// Options for a lease acquisition.
#[derive(Debug, Clone)]
pub struct LeaseAcquireOptions {
    pub holder: String,
    /// Optional TTL (expires_at); None = permanent lease.
    pub ttl: Option<chrono::Duration>,
}

/// A successfully acquired worktree.
#[derive(Debug, Clone)]
pub struct Acquired {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub lease: Option<Lease>,
}

/// The environment variable that opts into unique worktree leaf directories
/// (Go `config.UniqueLeafEnvVar`, config.go:68).
pub const TREEHOUSE_UNIQUE_LEAF_VAR: &str = "TREEHOUSE_UNIQUE_LEAF";

/// The environment variable that overrides the configured worktree path
/// template (Go `config.WorktreePathEnvVar`, config.go:73).
pub const TREEHOUSE_WORKTREE_PATH_VAR: &str = "TREEHOUSE_WORKTREE_PATH";

/// Placeholders a worktree path template may use (Go
/// `internal/pool/worktree_path.go:16-19`). `{pool}` and `{repo_parent}`
/// expand to absolute directories; `{slot}` and `{repo}` are a single segment.
pub const PLACEHOLDER_POOL: &str = "{pool}";
pub const PLACEHOLDER_SLOT: &str = "{slot}";
pub const PLACEHOLDER_REPO: &str = "{repo}";
pub const PLACEHOLDER_REPO_PARENT: &str = "{repo_parent}";

const WORKTREE_PATH_PLACEHOLDERS: [&str; 4] = [
    PLACEHOLDER_POOL,
    PLACEHOLDER_SLOT,
    PLACEHOLDER_REPO,
    PLACEHOLDER_REPO_PARENT,
];

/// Placeholders that can tell one repository's slots from another's (Go
/// `repositoryScopedPlaceholders`, worktree_path.go:52-56).
///
/// One is REQUIRED: slot names are allocated per pool, so a template carrying
/// none sends the first slot of every repository to the same directory.
/// `{repo_parent}` is deliberately absent — two sibling repositories expand it
/// identically, so it distinguishes nothing on its own.
const REPOSITORY_SCOPED_PLACEHOLDERS: [&str; 2] = [PLACEHOLDER_POOL, PLACEHOLDER_REPO];

/// Resolves the directory a newly created slot is placed in (Go
/// `resolveWorktreePath`, worktree_path.go:170-198).
///
/// An empty template keeps the built-in `<pool>/<slot>/<repo>` layout byte for
/// byte; `unique_leaf` then appends `-<slot>` to the leaf, which is what makes
/// two clones' slots stop colliding on one directory name. A template
/// SUPERSEDES `unique_leaf` because it names every segment including the leaf.
/// Recycled slots never reach here — they are handed back at the path recorded
/// in state, so a template never moves a worktree that exists.
pub(crate) fn resolve_worktree_path(
    repo_root: &Path,
    pool_dir: &Path,
    slot: &str,
    template: &str,
    unique_leaf: bool,
) -> Result<PathBuf, PoolError> {
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());

    if template.is_empty() {
        let leaf = if unique_leaf {
            format!("{repo_name}-{slot}")
        } else {
            repo_name.clone()
        };
        return Ok(pool_dir.join(slot).join(leaf));
    }

    let expanded = validate_worktree_path_template(template)?;
    let replaced = expanded
        .replace(PLACEHOLDER_POOL, &pool_dir.to_string_lossy())
        .replace(PLACEHOLDER_SLOT, slot)
        .replace(PLACEHOLDER_REPO, &repo_name)
        .replace(PLACEHOLDER_REPO_PARENT, &repo_root.parent().unwrap_or(repo_root).to_string_lossy());

    let resolved = PathBuf::from(clean_slash_path(&replaced));
    if !resolved.is_absolute() {
        return Err(PoolError::Io(
            format!(
                "worktree path {template:?} resolves to the relative path {:?}",
                resolved.display().to_string()
            ),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "not absolute"),
        ));
    }
    Ok(resolved)
}

/// Checks a template without touching the filesystem and returns what it
/// expands to (Go `validateWorktreePathTemplate`, worktree_path.go:87-129).
///
/// Acquire runs this before anything else, so a bad template fails on EVERY
/// invocation — including the ones that recycle an existing slot and so never
/// expand it. Otherwise a typo would silently hand back a slot at the built-in
/// layout and never report itself.
pub(crate) fn validate_worktree_path_template(template: &str) -> Result<String, PoolError> {
    if template.is_empty() {
        return Ok(String::new());
    }
    let bad = |msg: String| {
        PoolError::Io(
            msg,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid worktree path"),
        )
    };
    // Every `{...}` group must be a known placeholder, so a misspelling is
    // reported instead of becoming a literal directory name.
    for group in braced_groups(template) {
        if !WORKTREE_PATH_PLACEHOLDERS.contains(&group.as_str()) {
            return Err(bad(format!(
                "worktree path {template:?} uses unknown placeholder {group} (supported: {})",
                WORKTREE_PATH_PLACEHOLDERS.join(" ")
            )));
        }
    }
    if !template.contains(PLACEHOLDER_SLOT) {
        return Err(bad(format!(
            "worktree path {template:?} must contain {PLACEHOLDER_SLOT}: without it every pool slot resolves to the same directory"
        )));
    }
    // Carrying `{slot}` is not enough: a following `..` cancels the segment it
    // produced, so two slots clean down to one path (Go `slotsResolveApart`,
    // worktree_path.go:135-142). Only `{slot}` is substituted; the others stay
    // literal text, which path cleaning treats as ordinary segments.
    let resolve_for = |slot: &str| clean_slash_path(&template.replace(PLACEHOLDER_SLOT, slot));
    if resolve_for("slot-a") == resolve_for("slot-b") {
        return Err(bad(format!(
            "worktree path {template:?} resolves to the same directory for every slot, so two slots would share one worktree; give {PLACEHOLDER_SLOT} a path segment that survives path cleaning"
        )));
    }
    if !REPOSITORY_SCOPED_PLACEHOLDERS
        .iter()
        .any(|p| template.contains(p))
    {
        return Err(bad(format!(
            "worktree path {template:?} must contain one of {}: slot names are allocated per repository, so without one the first slot of every repository resolves to the same directory",
            REPOSITORY_SCOPED_PLACEHOLDERS.join(" ")
        )));
    }
    Ok(template.to_string())
}

/// The `GitRepo` a repository-scoped branch check runs against.
fn repo_for_branch(root: &Path) -> GitRepo {
    GitRepo {
        common_dir: root.to_path_buf(),
        worktree: None,
    }
}

/// Rejects a branch name git itself would refuse (Go
/// `vcs.ValidateBranchName` -> `git check-ref-format --branch`).
///
/// `--branch` names a ref that is created in the user's repository, so a name
/// like `../evil` or `-x` must be refused before anything runs — git would
/// treat those as options or as a path rather than as a branch.
pub(crate) fn validate_branch_name(branch: &str) -> Result<(), PoolError> {
    if branch.is_empty() {
        return Err(PoolError::Io(
            "--branch requires a non-empty branch name".to_string(),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty branch name"),
        ));
    }
    // Git's own rule set, inlined so a bad name is rejected without spawning a
    // process and without depending on the caller's repository: the ref must
    // not begin with `-` or `.`, must not end in `.lock`, must not contain a
    // control character, ` `, `~`, `^`, `:`, `?`, `*`, `[`, or `\`, must not
    // contain `..` or `@{`, and must contain no empty path component.
    let bad = |why: &str| {
        PoolError::Io(
            format!("invalid branch name {branch:?}: {why}"),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid branch name"),
        )
    };
    if branch.starts_with('-') {
        return Err(bad("it would be read as an option"));
    }
    if branch.starts_with('.') {
        return Err(bad("a ref component may not begin with '.'"));
    }
    if branch.contains("..") {
        return Err(bad("it contains '..'"));
    }
    if branch.contains("@{") {
        return Err(bad("it contains '@{'"));
    }
    if branch.ends_with('.') {
        return Err(bad("a ref must not end with '.'"));
    }
    if branch.ends_with(".lock") {
        return Err(bad("a ref may not end with '.lock'"));
    }
    if branch.ends_with('/') {
        return Err(bad("a ref must not end with '/'"));
    }
    if branch.starts_with('/') {
        return Err(bad("a ref must not begin with '/'"));
    }
    if branch.contains("//") {
        return Err(bad("it contains an empty path component"));
    }
    for ch in branch.chars() {
        if (ch as u32) < 0x20 || ch == ' ' || " ~^:?*[\\".contains(ch) {
            return Err(bad("it contains a character git forbids in a ref name"));
        }
    }
    Ok(())
}

/// Every `{...}` group in `template`, in order (Go `placeholderPattern`,
/// worktree_path.go:60).
fn braced_groups(template: &str) -> Vec<String> {
    let bytes: Vec<char> = template.chars().collect();
    let mut groups = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // A group is `{...}` with no nested `{` inside; anything else (a
        // `{}` placeholder, or an unclosed brace) is left as ordinary text and
        // rejected by the caller's unknown-placeholder check.
        if bytes[i] == '{'
            && let Some(close) = (i + 1..bytes.len()).find(|&j| bytes[j] == '}')
            && !bytes[i + 1..close].contains(&'{')
        {
            groups.push(bytes[i..=close].iter().collect());
            i = close + 1;
            continue;
        }
        i += 1;
    }
    groups
}

/// Lexical path cleaning, matching Go's `filepath.Clean(filepath.FromSlash(...))`
/// closely enough for the template checks above: collapse `//`, drop `.`, and
/// resolve `..` without touching the filesystem.
fn clean_slash_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                match out.last() {
                    Some(&last) if last != ".." => {
                        out.pop();
                    }
                    _ => {
                        if !absolute {
                            out.push("..");
                        }
                    }
                };
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// The worktree status string values (Go parity).
pub const STATUS_AVAILABLE: &str = "available";
pub const STATUS_IN_USE: &str = "in-use";
pub const STATUS_DIRTY: &str = "dirty";
pub const STATUS_LEASED: &str = "leased";
pub const STATUS_HERE: &str = "you're here";
/// A slot whose contents cannot be judged at all (Go `StatusDamaged`,
/// pool.go:25): its VCS marker is missing, so every backend dispatch on that
/// path would silently resolve the repository ENCLOSING the pool. Distinct
/// from `dirty` — which is a claim about content — and from `in use`, which is
/// a claim about a reservation.
pub const STATUS_DAMAGED: &str = "damaged";

/// The environment variable that overrides the configured pool root
/// (Go `config.RootEnvVar`, config.go:63). Outranks `treehouse.toml`'s
/// `root`, which in turn outranks the built-in default.
pub(crate) const TREEHOUSE_ROOT_VAR: &str = "TREEHOUSE_ROOT";

/// One worktree's status as reported by `status`.
#[derive(Debug, Clone)]
pub struct WorktreeStatus {
    pub name: String,
    pub path: String,
    pub status: String,
    pub processes: Vec<ProcessInfo>,
    pub lease_id: String,
    pub lease_holder: String,
    pub leased_at: chrono::DateTime<chrono::Utc>,
}

/// The pool: ties together config, git, process, and state under one lock.
pub struct Pool {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub(crate) git: Arc<dyn GitBackend>,
    pub(crate) process: Arc<ProcessTable>,
    pub(crate) config: TreehouseConfig,
    pub(crate) lock_timeout: std::time::Duration,
    pub(crate) env: Arc<dyn TreehouseEnv>,
}

impl Pool {
    /// Opens (or creates) the pool for a repo. `remote_url` is used for the
    /// pool dir hash; falls back to the repo path when unknown.
    pub fn open(
        repo_root: &Path,
        remote_url: Option<&str>,
        opts: &OpenOptions,
    ) -> Result<Self, PoolError> {
        Self::open_with_env(repo_root, remote_url, opts, Arc::new(DefaultEnv))
    }

    /// Opens a pool using the injected environment.
    ///
    /// The pool root is resolved through `env`, never through the process
    /// environment directly: `--env-path` arrives here as a `TreehouseEnv`
    /// whose `pool_root()` names a custom directory, and calling the
    /// non-`_with_env` resolver silently discarded it, so the flag was a no-op
    /// and every invocation fell back to `~/.treehouse`.
    ///
    /// Precedence mirrors Go `config.ResolveRoot` (config.go:110-117):
    /// `TREEHOUSE_ROOT` outranks the `treehouse.toml` `root`, and the default
    /// is whichever tier each one resolves to. `env.pool_root()` is the
    /// `--env-path` channel and, as in Go, still wins when neither an env var
    /// nor a config root is set — it is the final fallback, not a competitor to
    /// the `root` argument. `opts.root_override` is the `--root` flag and sits
    /// above all of them, because Go ranks an explicit flag first.
    pub fn open_with_env(
        repo_root: &Path,
        remote_url: Option<&str>,
        opts: &OpenOptions,
        env: Arc<dyn TreehouseEnv>,
    ) -> Result<Self, PoolError> {
        // `TREEHOUSE_ROOT` is read through the injected env rather than
        // `std::env` so an in-memory environment can exercise the override
        // tier without mutating the process environment of the test binary.
        let env_root = env
            .env_var(TREEHOUSE_ROOT_VAR)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string());
        let root = opts
            .root_override
            .clone()
            .or(env_root)
            .or_else(|| opts.config.root.clone());
        let dir = resolve_pool_dir_with_env(repo_root, root.as_deref(), remote_url, env.as_ref())
            .map_err(PoolError::Config)?;
        env.ensure_dir(&dir)
            .map_err(|e| PoolError::Io(format!("creating pool dir {}", dir.display()), e))?;

        let git = crate::git::ShellGitBackend::discover().map_err(PoolError::Git)?;
        let process = ProcessTable::new();

        Ok(Pool {
            root: repo_root.to_path_buf(),
            dir,
            git: Arc::new(git),
            process: Arc::new(process),
            config: opts.config.clone(),
            lock_timeout: opts.lock_timeout,
            env,
        })
    }

    /// Opens a pool at an already-known pool directory (used by `--all`
    /// sweeps that discover pool dirs; the backing repo root is the pool
    /// dir's parent).
    pub fn open_at(pool_dir: &Path, opts: &OpenOptions) -> Result<Self, PoolError> {
        Self::open_at_with_env(pool_dir, opts, Arc::new(DefaultEnv))
    }

    /// Opens a pool at an already-known pool directory using the injected environment.
    pub fn open_at_with_env(
        pool_dir: &Path,
        opts: &OpenOptions,
        env: Arc<dyn TreehouseEnv>,
    ) -> Result<Self, PoolError> {
        env.ensure_dir(pool_dir)
            .map_err(|e| PoolError::Io(format!("creating pool dir {}", pool_dir.display()), e))?;

        let git = crate::git::ShellGitBackend::discover().map_err(PoolError::Git)?;
        let process = ProcessTable::new();

        Ok(Pool {
            root: pool_dir
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| pool_dir.to_path_buf()),
            dir: pool_dir.to_path_buf(),
            git: Arc::new(git),
            process: Arc::new(process),
            config: opts.config.clone(),
            lock_timeout: opts.lock_timeout,
            env,
        })
    }

    /// The pool directory.
    pub fn pool_dir(&self) -> &Path {
        &self.dir
    }

    /// The injected environment.
    pub fn env(&self) -> &dyn TreehouseEnv {
        &*self.env
    }

    /// Whether a worktree is dirty (git status --porcelain --untracked-files=all).
    pub fn git_is_dirty(&self, path: &Path) -> Result<bool, PoolError> {
        Ok(self.git.is_dirty(path)?)
    }

    /// Whether a pool SLOT is dirty, answered by the backend that slot's own
    /// marker names (Go `vcs.IsDirty`, vcs.go:648-650).
    ///
    /// Go's `IsDirty` is exactly `backendForWorktree(path).IsDirty(path)` — it
    /// dispatches on the SLOT, never on whichever backend the pool happened to be
    /// opened with. Asking `self.git` instead would run git commands inside a
    /// `.jj` workspace and report the failure in git's vocabulary, which hides
    /// the real cause. For a git slot the answer is unchanged, because the
    /// registry's git and the pool's git are both `ShellGitBackend::discover()`
    /// over the same `GIT_BIN` → `PATH` lookup.
    ///
    /// A markerless path still answers: [`vcs::backend_for_worktree`] falls
    /// back to the default backend for reads, because reading a status is
    /// harmless where rewriting a checkout is not. A slot the seam cannot
    /// resolve at all reads as CLEAN, preserving the `.unwrap_or(false)` this
    /// call site already applied — and `status` only reaches here after
    /// [`slot_backend_name`] has refused anything that is not a git slot as
    /// damaged.
    fn slot_is_dirty(&self, slot: &Path) -> bool {
        match vcs::backend_for_worktree(slot) {
            Ok(backend) => backend.is_dirty(slot).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Whether `dir` is a treehouse pool directory (Go `pool.IsPoolDir`).
    ///
    /// The state file is the marker, exactly as upstream: a directory that only
    /// *looks* like a pool (right parent, right name) is not one, and treating
    /// it as one would let `destroy --all` aim at an unrelated tree.
    pub fn is_pool_dir(dir: &Path) -> bool {
        State::state_file_path(dir).is_file()
    }

    /// Whether `path`'s VCS marker can be read, i.e. whether it names a real
    /// worktree of the configured backend (Go `vcs.WorktreeBackendName`).
    ///
    /// `false` means the slot is damaged, and a caller must NOT dispatch git on
    /// it: there is deliberately no fallback backend, because in the supported
    /// in-project pool layout the fallback resolves the repository ENCLOSING
    /// the pool and every "fact" it reports — dirty, in-use, branch — is about
    /// the user's own working tree rather than the slot.
    pub fn is_readable_worktree(path: &Path) -> bool {
        !slot_backend_name(path).is_empty()
    }

    /// Every worktree entry, in state order, read under the pool lock.
    fn read_entries(&self) -> Result<Vec<WorktreeEntry>, PoolError> {
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let state = State::read_state(&self.dir).map_err(PoolError::State)?;
            Ok(state.worktrees)
        })
    }

    /// Looks a worktree up by its exact path (Go `pool.FindByPath`).
    pub fn find_by_path(&self, path: &str) -> Result<Option<WorktreeEntry>, PoolError> {
        Ok(self.read_entries()?.into_iter().find(|w| w.path == path))
    }

    /// Looks a worktree up by its slot name — the first column
    /// `status` prints (Go `pool.FindByName`).
    pub fn find_by_name(&self, name: &str) -> Result<Option<WorktreeEntry>, PoolError> {
        Ok(self.read_entries()?.into_iter().find(|w| w.name == name))
    }

    /// Every slot name in the pool, for the diagnostic Go's
    /// `unknownWorktreeNameError` prints alongside a refused name.
    pub fn worktree_names(&self) -> Result<Vec<String>, PoolError> {
        Ok(self.read_entries()?.into_iter().map(|w| w.name).collect())
    }

    /// Durably leases an EXISTING worktree by name, in place (Go
    /// `pool.LeaseExisting`, the `treehouse lease` verb, #128).
    ///
    /// State-only by contract: nothing is fetched, reset, cleaned, or checked
    /// out, so it is safe on a slot already holding live work. That is the
    /// whole point of the verb — it hands a slot the same "never handed out,
    /// never pruned, never destroyed" protection a leased *acquisition* has,
    /// without touching a single byte of the worktree.
    pub fn lease_existing(&self, name: &str, holder: &str) -> Result<LeaseInfo, PoolError> {
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            // Read BEFORE heal, so a name whose directory has since vanished is
            // still "registered" and gets the actionable stale-entry message
            // rather than the misleading "no worktree named X".
            let registered = state.worktrees.iter().any(|w| w.name == name);
            heal_state(&mut state, |pid| self.process.started_at(pid));

            for wt in &mut state.worktrees {
                if wt.name != name {
                    continue;
                }
                if wt.destroying {
                    return Err(PoolError::BeingDestroyed(name.to_string()));
                }
                // Refuse rather than re-mint: a second lease would orphan the
                // first holder's identity, and `return --if-lease-id` for the
                // original would then silently stop matching.
                if wt.leased {
                    return Err(PoolError::AlreadyLeased {
                        name: name.to_string(),
                        holder: wt.lease_holder.clone(),
                    });
                }
                mark_acquired_lease(wt, holder, chrono::Utc::now());
                let info = crate::lease::lease_info_from_entry(wt);
                state_file::write_state(&self.dir, &state)
                    .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
                return Ok(info);
            }

            if registered {
                return Err(PoolError::StaleEntry(name.to_string()));
            }
            Err(PoolError::NoSuchWorktree(name.to_string()))
        })
    }

    /// Acquires a worktree from the pool (`get`).
    ///
    /// Short-lock protocol: branch+fetch outside the lock; reserve under lock;
    /// reset outside; re-validate under a second lock; hooks outside.
    pub fn get(&self, opts: &AcquireOptions) -> Result<Acquired, PoolError> {
        // Step 0 (OUTSIDE lock): validate the placement and branch options
        // BEFORE the fetch and before any slot is inspected.
        //
        // Both checks are pure text rules, so a wrong value costs nothing to
        // reject. Running them here — rather than only on the creation branch —
        // is what makes them bite on every invocation, INCLUDING the ones that
        // recycle an existing slot and so never expand the template. Without
        // this, a typo silently hands back a slot at the built-in layout and
        // never reports itself (Go `acquire`, pool.go:377-386).
        validate_worktree_path_template(opts.worktree_path.as_deref().unwrap_or(""))?;
        if let Some(new_branch) = opts.new_branch.as_deref() {
            validate_branch_name(new_branch)?;
            // Fails if the branch ALREADY exists: `-b` creates, it never
            // adopts (Go pool.go:412-418).
            if self.git.local_branch_exists(&repo_for_branch(&self.root), new_branch) {
                return Err(PoolError::Io(
                    format!("branch {new_branch:?} already exists"),
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "branch already exists",
                    ),
                ));
            }
        }

        // Step 1 (OUTSIDE lock): resolve the default branch and fetch origin.
        let repo = GitRepo {
            common_dir: self.root.clone(),
            worktree: None,
        };
        let branch = match &opts.branch {
            Some(b) => b.clone(),
            None => self.git.default_branch(&repo).map_err(PoolError::Git)?,
        };
        if !opts.skip_fetch && self.git.has_remote(&repo, "origin") {
            self.git.fetch(&repo).map_err(PoolError::Git)?;
        }

        // Step 1b (OUTSIDE lock): resolve this clone's identity once, so the
        // reuse loop can scope candidates to it without a `rev-parse` per slot
        // (Go `commonDir, identityErr := acquisitionCommonGitDir(repoRoot)`,
        // pool.go:412). After the fetch, matching Go: a base that exists only
        // on origin must be rejected against post-fetch refs.
        let requester = RequesterIdentity::resolve(self.git.as_ref(), &self.root);

        // Step 2 (LOCK #1): read + heal + scan + mark acquired + write.
        let AcquiredSlot {
            name,
            path,
            lease,
            origin,
            prior_inventory,
        } = acquire_locked(
            &self.dir,
            self.lock_timeout,
            self.config.max_trees,
            &self.root,
            &self.process,
            &self.git,
            opts,
            &branch,
            &repo,
            &requester,
        )?;

        // Step 3 (OUTSIDE lock): reset the worktree to the base branch.
        // The reservation held since step 2 is the anti-TOCTOU wall.
        //
        // Dispatched through the VCS seam rather than straight at `self.git`,
        // so a slot whose marker names another backend is handled by THAT
        // backend (Go `vcs.ResetWorktree`). For a git slot this resolves to the
        // git backend; for a slot with no marker it refuses instead of
        // resetting whatever repository encloses the pool.
        let slot = Path::new(&path);
        let backend = vcs::destructive_backend_for_worktree(slot).map_err(PoolError::Git)?;
        reset_for_acquisition(&backend, slot, &branch, &prior_inventory, origin)?;

        // Step 3a (OUTSIDE lock): copy the ignored files this pool is
        // responsible for into the slot (Go `seedWorktree`, pool.go:697).
        //
        // Runs on BOTH origins, after the reset and before the branch is
        // created, matching Go. The returned inventory is the only record of
        // what a later reset may delete, so it is computed here and persisted
        // in step 4.
        let seeded = backend
            .seed_worktree(&repo, slot, opts.include_manifest.as_deref())
            .map_err(|e| {
                PoolError::Git(GitError::new(
                    format!("seeding worktree {}", slot.display()),
                    format!(
                        "failed to seed .worktreeinclude into {}: {e}",
                        slot.display()
                    ),
                    GitErrorKind::Other,
                ))
            })?;

        // Step 3b (OUTSIDE lock): create and check out the requested branch.
        //
        // This is Go's `-b` (AcquireOptions.Branch, pool.go:567-601), and it
        // runs AFTER the reset so the branch is created at the commit the slot
        // was actually acquired at — creating it earlier would pin it to
        // whatever HEAD the slot happened to be recycled with.
        if let Some(new_branch) = opts.new_branch.as_deref() {
            backend
                .create_branch(slot, new_branch)
                .map_err(PoolError::Git)?;
        }

        // Step 4 (LOCK #2): re-validate the reservation is intact; rewrite
        // only if heal changed something.
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            heal_state(&mut state, |pid| self.process.started_at(pid));
            // Reservation check: if the worktree vanished or was re-acquired
            // mid-reset, surface it rather than returning a stale handle.
            if let Some(wt) = state.worktrees.iter_mut().find(|w| w.name == name) {
                // The seed inventory is recorded HERE, under the second lock,
                // and only once the files are actually on disk. Recording it
                // earlier would let a crash between the two leave a reset
                // authorized to delete files that were never copied.
                wt.set_seed_inventory(SeedInventory {
                    paths: seeded.clone(),
                    known: true,
                    backend: backend.name().to_string(),
                    // Go records a jj seed-authentication identity here. This
                    // port seeds git worktrees only and has no such marker, so
                    // the field stays empty rather than carrying a value that
                    // nothing would ever verify.
                    auth_identity: String::new(),
                });
                wt.set_base_branch(&branch);
                state_file::write_state(&self.dir, &state)
                    .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
            }
            Ok(())
        })?;

        // Step 4b (OUTSIDE lock): opt-in APFS sharing, FRESH slots only
        // (Go pool.go:782-795).
        //
        // Deliberately not on the recycled path: a reused slot may still have
        // external writers, and neither `return` nor reuse is an invitation to
        // sweep it. The module's own contract requires exclusive ownership of
        // the destination for the whole pass, which only a slot this
        // acquisition just created can promise.
        if origin == SlotOrigin::Fresh && opts.apfs_sharing {
            share_slot_files(&self.root, slot, &mut std::io::stderr());
        }

        // Step 5 (OUTSIDE lock): run post_create hooks (lease mode routes
        // stdout to stderr so machine output stays clean).
        if !self.config.hooks.post_create.is_empty() {
            let lease_mode = opts.lease.is_some();
            let mut out: Box<dyn std::io::Write> = if lease_mode {
                Box::new(std::io::sink())
            } else {
                Box::new(std::io::stdout())
            };
            let mut err = std::io::stderr();
            hooks::run(
                &self.config.hooks.post_create,
                Path::new(&path),
                out.as_mut(),
                &mut err,
            );
        }

        Ok(Acquired {
            name,
            path: PathBuf::from(path),
            branch,
            lease,
        })
    }

    /// Non-interactive lease acquire (`get --lease`) with an optional TTL.
    pub fn acquire_lease_with_ttl(
        &self,
        holder: &str,
        ttl: Option<chrono::Duration>,
    ) -> Result<LeaseInfo, PoolError> {
        self.acquire_lease_with_options(
            &AcquireOptions {
                lease: Some(LeaseAcquireOptions {
                    holder: holder.to_string(),
                    ttl,
                }),
                ..Default::default()
            },
            holder,
        )
    }

    /// Lease acquire that also honours the acquisition flags (Go
    /// `getLeaseRunE(repoRoot, poolDir, cfg, acquireOpts)`, cmd/get.go:169-173).
    ///
    /// This exists so `--lease` does not silently discard `--base`, `-b`,
    /// `--unique-leaf` and `--worktree-path`: the holder/TTL pair alone cannot
    /// carry them, and folding them in here keeps one acquire path for both
    /// modes instead of two that disagree.
    pub fn acquire_lease_with_options(
        &self,
        opts: &AcquireOptions,
        holder: &str,
    ) -> Result<LeaseInfo, PoolError> {
        let mut opts = opts.clone();
        opts.lease = Some(LeaseAcquireOptions {
            holder: holder.to_string(),
            ttl: opts.lease.as_ref().and_then(|l| l.ttl),
        });
        let acquired = self.get(&opts)?;
        let lease = acquired.lease.as_ref();
        Ok(LeaseInfo {
            path: acquired.path.to_string_lossy().into_owned(),
            lease_id: lease.map(|l| l.id.clone()).unwrap_or_default(),
            lease_holder: holder.to_string(),
            leased_at: lease.map(|l| l.acquired_at).unwrap_or(ZERO_TIME),
        })
    }

    /// Non-interactive lease acquire (`get --lease`), returning the identity.
    pub fn acquire_lease(&self, holder: &str) -> Result<LeaseInfo, PoolError> {
        let acquired = self.get(&AcquireOptions {
            lease: Some(LeaseAcquireOptions {
                holder: holder.to_string(),
                ttl: None,
            }),
            ..Default::default()
        })?;
        Ok(LeaseInfo {
            path: acquired.path.to_string_lossy().into_owned(),
            lease_id: acquired
                .lease
                .as_ref()
                .map(|l| l.id.clone())
                .unwrap_or_default(),
            lease_holder: holder.to_string(),
            leased_at: acquired
                .lease
                .as_ref()
                .map(|l| l.acquired_at)
                .unwrap_or(ZERO_TIME),
        })
    }

    /// Releases a managed worktree, clearing its reservation, and returns it
    /// to the available pool (Go `Release`).
    pub fn release(&self, worktree_path: &str) -> Result<(), PoolError> {
        self.release_conditional(worktree_path, &ReleasePreconditions::default(), None)
    }

    /// Releases conditionally, ABA-safe: preconditions + before_reset are
    /// validated under ONE lock; the reset runs OUTSIDE the lock (short-lock
    /// protocol); the reservation is cleared under a second lock. The
    /// reservation is held across the external reset so no concurrent acquire
    /// can re-assign the worktree mid-reset.
    pub fn release_conditional(
        &self,
        worktree_path: &str,
        preconditions: &ReleasePreconditions,
        before_reset: Option<&mut dyn FnMut() -> Result<(), PoolError>>,
    ) -> Result<(), PoolError> {
        // Repo/branch resolution outside the lock. Use main_repo_root so a
        // detached linked worktree resolves back to the owning repo (whose HEAD
        // is not detached) — otherwise default_branch would fall through to
        // init.defaultBranch ("master").
        let repo_root = self
            .git
            .main_repo_root(Path::new(worktree_path))
            .unwrap_or_else(|_| self.root.clone());
        let repo = GitRepo {
            common_dir: repo_root,
            worktree: None,
        };
        let branch = self.git.default_branch(&repo).map_err(PoolError::Git)?;

        // LOCK #1: find + validate preconditions + before_reset (under lock,
        // per Go doc — caller's termination/detachment can't race). The entry
        // is still reserved.
        //
        // The seed inventory is captured HERE, under the lock that proves the
        // entry is still releasable, and handed to the reset outside it. Reading
        // it after the unlock would race a concurrent acquisition that has
        // already reserved the slot and re-seeded it: the reset would then
        // delete files belonging to the NEW occupant.
        let prior_inventory = with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let wt = releasable_worktree(&mut state, worktree_path, preconditions, &self.process)?;
            let inventory = wt.seed_inventory();
            if let Some(cb) = before_reset {
                cb()?;
            }
            Ok(inventory)
        })?;

        // OUTSIDE the lock: reset the worktree. The reservation is still held,
        // so no acquire/destroy can take it mid-reset.
        //
        // A slot whose seed inventory is KNOWN is reset through
        // `reset_worktree_with_seeded_paths`, which removes exactly those files
        // and then resets (Go `ResetWorktreeWithSeededPaths`, pool.go:1060). An
        // ignored file the pool copied is invisible to `clean -fd`, so without
        // this the pool's own seeds accumulate in a slot forever.
        //
        // A slot with NO known inventory authorizes no deletion at all, and
        // resets plainly instead: the tracked tree is restored and nothing the
        // pool cannot account for is removed.
        let slot = Path::new(worktree_path);
        match prior_inventory.authorized_paths() {
            Some(seeded) => {
                vcs::destructive_backend_for_worktree(slot)
                    .map_err(PoolError::Git)?
                    .reset_worktree_with_seeded_paths(slot, &branch, seeded)
                    .map_err(PoolError::Git)?;
            }
            None => {
                self.git
                    .reset_worktree(slot, &branch)
                    .map_err(PoolError::Git)?;
            }
        }

        // LOCK #2: RE-VALIDATE the preconditions AND clear in ONE atomic lock.
        // This is what makes release exactly-once (ABA-safe): a concurrent
        // caller that passed LOCK #1 will find the lease already cleared here
        // and fail its precondition, so only one release ever succeeds.
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let wt = releasable_worktree(&mut state, worktree_path, preconditions, &self.process)?;
            wt.owner_pid = 0;
            wt.owner_started_at = 0;
            crate::state::clear_lease(wt);
            // A returned slot holds nothing this pool put there: the inventory
            // becomes a VERIFIED empty one (Go `releaseEntry`, pool.go:1466),
            // so the next acquisition reads "seeded nothing" and deletes
            // nothing, rather than acting on a list that no longer describes
            // the slot.
            wt.clear_seed_inventory();
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
            Ok(())
        })?;

        Ok(())
    }

    /// Read-only precondition validation (Go `ValidateReleasePreconditions`).
    pub fn validate_release_preconditions(
        &self,
        worktree_path: &str,
        preconditions: &ReleasePreconditions,
    ) -> Result<(), PoolError> {
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            let _ = releasable_worktree(&mut state, worktree_path, preconditions, &self.process)?;
            Ok(())
        })
    }

    /// Reports the status of managed worktrees (`status`), healing + writing
    /// state under ONE exclusive lock.
    pub fn status(&self) -> Result<Vec<WorktreeStatus>, PoolError> {
        let cwd = std::env::current_dir().unwrap_or_default();
        with_pool_lock(&self.dir, self.lock_timeout, || {
            let mut state = State::read_state(&self.dir).map_err(PoolError::State)?;
            heal_state(&mut state, |pid| self.process.started_at(pid));
            state_file::write_state(&self.dir, &state)
                .map_err(|e| PoolError::Io("writing state".to_string(), e))?;

            let mut result = Vec::new();
            for wt in &state.worktrees {
                if wt.destroying {
                    continue;
                }
                let procs = self
                    .process
                    .find_in_worktree(Path::new(&wt.path))
                    .unwrap_or_default();
                let mut ws = WorktreeStatus {
                    name: wt.name.clone(),
                    path: wt.path.clone(),
                    status: STATUS_AVAILABLE.to_string(),
                    processes: procs.clone(),
                    lease_id: String::new(),
                    lease_holder: String::new(),
                    leased_at: ZERO_TIME,
                };

                if wt.leased {
                    ws.status = STATUS_LEASED.to_string();
                    ws.lease_id = wt.lease_id.clone();
                    ws.lease_holder = wt.lease_holder.clone();
                    ws.leased_at = wt.leased_at;
                } else if reservation::owner_alive(wt, &self.process) {
                    ws.status = STATUS_IN_USE.to_string();
                } else if !procs.is_empty() {
                    ws.status = STATUS_IN_USE.to_string();
                    if cwd_in_worktree(&cwd, Path::new(&wt.path)) {
                        ws.status = STATUS_HERE.to_string();
                    }
                } else if slot_backend_name(Path::new(&wt.path)).is_empty() {
                    // Damaged. Checked before `is_dirty` because that call
                    // would resolve the repository ENCLOSING the pool and
                    // report on it, and reported separately from "dirty" so a
                    // slot whose contents cannot be read is never mistaken for
                    // one that was read and found changed.
                    ws.status = STATUS_DAMAGED.to_string();
                } else if self.slot_is_dirty(Path::new(&wt.path)) {
                    ws.status = STATUS_DIRTY.to_string();
                }
                // A slot the recovery scan adopted but could not read is
                // reported damaged rather than leased, so the read failure is
                // never mistaken for an ordinary lease. It stays leased
                // underneath, so acquire and prune keep skipping it like every
                // other recovered entry (Go pool.go:1258-1261).
                if !wt.recovery_error.is_empty() {
                    ws.status = STATUS_DAMAGED.to_string();
                }
                result.push(ws);
            }
            Ok(result)
        })
    }
}

/// Runs `f` under the pool state lock, flattening the double-`Result` that
/// `with_state_lock` returns (outer lock error, inner callback error).
pub(crate) fn with_pool_lock<T>(
    dir: &Path,
    timeout: std::time::Duration,
    f: impl FnOnce() -> Result<T, PoolError>,
) -> Result<T, PoolError> {
    with_state_lock(dir, timeout, f).map_err(Pool::lock_err_ty)
}

impl Pool {
    /// Flattens the lock's double-`Result` into a single [`PoolError`].
    ///
    /// A failure from inside the critical section is returned AS IS. Stringifying
    /// it would bury every typed error a caller can match on — `PoolFull` and
    /// the reason it is full, `BeingDestroyed`, `LeasePrecondition` — behind an
    /// opaque `Lock` variant that only its `Display` can recover. Only a real
    /// lock failure (timeout, I/O on the lock file) becomes `PoolError::Lock`.
    fn lock_err_ty(e: LockError<PoolError>) -> PoolError {
        match e {
            LockError::Callback(inner) => inner,
            other => PoolError::Lock(other.to_string()),
        }
    }
}

/// The lock-acquire critical section (LOCK #1 of `get`): read + heal + scan +
/// mark acquired + write, all under the pool state lock.
///
/// `requester` is the requester's own clone identity, resolved by the caller
/// before the lock was taken (Go `acquisitionCommonGitDir(repoRoot)` at
/// pool.go:412). Resolving it once outside the loop matters: a pool shared by
/// two clones must never hand one clone a slot the other owns, and proving
/// that is a `git rev-parse` per candidate otherwise.
#[allow(clippy::too_many_arguments)]
fn acquire_locked(
    dir: &Path,
    lock_timeout: std::time::Duration,
    max_trees: u32,
    root: &Path,
    process: &ProcessTable,
    git: &Arc<dyn GitBackend>,
    opts: &AcquireOptions,
    branch: &str,
    repo: &GitRepo,
    requester: &RequesterIdentity,
) -> Result<AcquiredSlot, PoolError> {
    with_state_lock(dir, lock_timeout, || {
        let mut state = State::read_state(dir).map_err(PoolError::State)?;
        heal_state(&mut state, |pid| process.started_at(pid));

        // Why a candidate slot was passed over, counted so a full pool can say
        // WHY it is full. Go feeds the same two counters into its PoolFull
        // message (pool.go:641-646): "in use" is the ordinary case and needs no
        // mention, but "another clone owns it" and "we could not prove who owns
        // it" are the two an operator can act on, and silently reporting a
        // generic full pool sends them looking in the wrong place.
        let mut skips = SkipCounts::default();

        // Find an available worktree (clean, not in-use, not leased, this
        // clone's, and holding no unlanded work).
        for i in 0..state.worktrees.len() {
            let candidate = {
                let wt = &state.worktrees[i];
                if wt.destroying || wt.leased || reservation::owner_alive(wt, process) {
                    continue;
                }
                let path = Path::new(&wt.path);

                // Marker first, before ANY dispatch on this path (Go
                // pool.go:468-480). A slot with no marker would have its
                // in-use, dirty and merge checks resolved against the
                // repository ENCLOSING the pool, and the reset would rewrite
                // that repository instead. Skipping is the only safe answer.
                if slot_backend_name(path).is_empty() {
                    continue;
                }

                // Clone identity, then availability. Pools are keyed by origin
                // URL, so two clones of the same origin share one pool dir, but
                // a linked worktree still belongs to ONE physical clone. Never
                // reset another clone's slot — even when both clones hold
                // identical refs — and never one whose owner, or our own,
                // cannot be proven.
                match requester.matches_slot(git.as_ref(), path) {
                    IdentityMatch::Yes => {}
                    IdentityMatch::OtherClone => {
                        skips.other_clone += 1;
                        continue;
                    }
                    IdentityMatch::Unverified => {
                        skips.unverified_clone += 1;
                        continue;
                    }
                }

                if process.is_worktree_in_use(path).unwrap_or(true) {
                    continue;
                }

                // A crashed or rebooted owner leaves the reservation empty
                // while its worktree still holds committed work: a clean tree
                // passes `is_dirty`, so availability alone must not authorize a
                // reset. Fail closed — if the merge state cannot be proven,
                // leave the slot untouched rather than let the reset discard
                // the work (Go pool.go:500-522, fix #104).
                match git.is_dirty(path) {
                    Ok(true) | Err(_) => continue,
                    Ok(false) => {}
                }
                if !requester.safe_to_reset(git.as_ref(), path, branch) {
                    continue;
                }

                Some((wt.name.clone(), wt.path.clone()))
            };
            if let Some((name, path)) = candidate {
                // Stamp the reservation (persisted now).
                let prior = state.worktrees[i].seed_inventory();
                let lease_info = mark_acquired_entry(&mut state.worktrees[i], opts, process);
                state_file::write_state(dir, &state)
                    .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
                return Ok(AcquiredSlot {
                    name,
                    path,
                    lease: lease_info,
                    origin: SlotOrigin::Recycled,
                    prior_inventory: prior,
                });
            }
        }

        // No available worktree — create a new one if the pool allows.
        if state.worktrees.len() as u32 >= max_trees {
            return Err(skips.into_pool_full(state.worktrees.len() as u32, max_trees));
        }
        let name = next_free_name(dir, &state);
        let wt_path = resolve_worktree_path(
            root,
            dir,
            &name,
            opts.worktree_path.as_deref().unwrap_or(""),
            opts.unique_leaf,
        )?;

        std::fs::create_dir_all(wt_path.parent().unwrap())
            .map_err(|e| PoolError::Io("creating worktree parent".to_string(), e))?;
        git.worktree_add(repo, &wt_path, branch)
            .map_err(PoolError::Git)?;

        let mut entry = WorktreeEntry {
            name: name.clone(),
            path: wt_path.to_string_lossy().into_owned(),
            created_at: chrono::Utc::now(),
            ..WorktreeEntry::default()
        };
        let lease_info = mark_acquired_entry(&mut entry, opts, process);
        state.worktrees.push(entry);
        state_file::write_state(dir, &state)
            .map_err(|e| PoolError::Io("writing state".to_string(), e))?;
        let path_str = wt_path.to_string_lossy().into_owned();
        Ok(AcquiredSlot {
            name,
            path: path_str,
            lease: lease_info,
            origin: SlotOrigin::Fresh,
            // A slot this call just created has nothing in it that we seeded,
            // so there is no inventory to honour and none to remove.
            prior_inventory: SeedInventory::default(),
        })
    })
    .map_err(Pool::lock_err_ty)
}

/// Whether a slot was reused or created, and what its previous occupant left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotOrigin {
    /// An available slot this acquisition handed back.
    Recycled,
    /// A worktree this acquisition just created.
    Fresh,
}

/// A slot the pool has reserved, returned by [`acquire_locked`].
///
/// `prior_inventory` is read under the SAME lock that stamped the reservation.
/// Reading it afterwards would be a race: the entry that recorded it may have
/// been rewritten by a concurrent acquisition between the unlock and the read,
/// and a reset that removes the wrong paths is not a recoverable mistake.
pub(crate) struct AcquiredSlot {
    pub name: String,
    pub path: String,
    pub lease: Option<Lease>,
    pub origin: SlotOrigin,
    pub prior_inventory: SeedInventory,
}

/// The outcome of comparing a candidate slot against the requester's clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityMatch {
    /// Same physical clone — the only case in which a slot may be reused.
    Yes,
    /// A different clone owns this worktree.
    OtherClone,
    /// Ownership could not be proven, in either direction.
    Unverified,
}

/// Why candidate slots were passed over, for the pool-full message.
#[derive(Debug, Default, Clone, Copy)]
struct SkipCounts {
    /// Slots proven to belong to another clone (Go `otherClone`).
    other_clone: u32,
    /// Slots whose ownership could not be proven (Go `unverifiedClone`).
    unverified_clone: u32,
}

impl SkipCounts {
    /// Builds the pool-full error, naming the actionable skip reasons.
    ///
    /// The plain message is kept byte-for-byte for the ordinary case: scripts
    /// match on it, and a full pool of genuinely in-use worktrees has nothing
    /// extra to say.
    fn into_pool_full(self, count: u32, max: u32) -> PoolError {
        if self.other_clone > 0 || self.unverified_clone > 0 {
            PoolError::PoolFullNotOurs {
                count,
                other_clone: self.other_clone,
                unverified_clone: self.unverified_clone,
                max,
            }
        } else {
            PoolError::PoolFull { count, max }
        }
    }
}

/// The requesting clone's identity, resolved once before the state lock.
struct RequesterIdentity {
    /// The main repo root backing this clone, or `None` when it could not be
    /// resolved. An unresolvable requester disables REUSE, never allocation:
    /// a fresh worktree is created in this clone's own repository, which is
    /// always safe, whereas reusing a slot of unknown provenance is not.
    root: Option<PathBuf>,
}

impl RequesterIdentity {
    /// Resolves the requester's clone identity from its repository root
    /// (Go `acquisitionCommonGitDir`, pool.go:365-372).
    fn resolve(git: &dyn GitBackend, repo_root: &Path) -> Self {
        Self {
            root: git.main_repo_root(repo_root).ok(),
        }
    }

    /// Whether `slot` is this clone's own worktree.
    fn matches_slot(&self, git: &dyn GitBackend, slot: &Path) -> IdentityMatch {
        let Some(mine) = &self.root else {
            return IdentityMatch::Unverified;
        };
        match git.main_repo_root(slot) {
            Ok(theirs) if same_file(mine, &theirs) => IdentityMatch::Yes,
            Ok(_) => IdentityMatch::OtherClone,
            Err(_) => IdentityMatch::Unverified,
        }
    }

    /// Whether `slot` may be reset to `branch` without discarding committed
    /// work (Go `vcs.IsWorktreeSafeToReset`).
    ///
    /// The check is `merge-base --is-ancestor HEAD <ref>`: a slot whose HEAD is
    /// reachable from the ref we are about to reset to holds nothing that reset
    /// would lose. An error is NOT "safe" — the ref could not be resolved, the
    /// worktree could not be read, or git failed — and every one of those means
    /// the answer is unknown, so the slot is left alone.
    ///
    /// This is the reuse loop's cheap SCREEN, not the authoritative guard, and
    /// the two are deliberately different calls. The screen asks only "is this
    /// slot worth taking?" and a wrong answer costs one skipped slot, so it
    /// stays a single `merge-base` against an unresolved ref. The authoritative
    /// pair — [`crate::vcs::is_worktree_safe_to_reset`] and
    /// [`crate::vcs::reset_worktree_to_ref`], reached from
    /// [`reset_for_acquisition`] as step 3 of [`Pool::get`] — pins the verified
    /// commit SHA together with the HEAD the ancestry check observed, and makes
    /// the RESET re-verify both under git's own `HEAD.lock`. Nothing between
    /// this screen and that reset can discard a commit silently, which is the
    /// whole reason the pair exists.
    ///
    /// Go's SECOND reading — the slot's recorded base branch, consulted when the
    /// first says unsafe — is still absent, because this port's
    /// `WorktreeEntry` carries Go's `base_branch` only as an uninterpreted
    /// `extra` field, so its "HEAD did not move between the two readings"
    /// guard has nothing to compare.
    fn safe_to_reset(&self, git: &dyn GitBackend, slot: &Path, branch: &str) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        let ref_ = git.branch_ref(
            &GitRepo {
                common_dir: root.clone(),
                worktree: None,
            },
            branch,
        );
        matches!(git.is_head_merged_into_ref(slot, &ref_), Ok(true))
    }
}

/// Restores `slot` to `branch` for a new occupant, closing the two races a
/// plain `reset_worktree` leaves open.
///
/// Go does this with one call, `vcs.ResetWorktreeToRefWithSeededPaths` — guard,
/// then seed removal, then the guarded reset, all under git's `HEAD.lock`
/// (gitvcs.go:808). This seam ships the guard (`is_worktree_safe_to_reset` +
/// `reset_worktree_to_ref`) and the seed-removal validator
/// ([`vcs::validate_seed_inventory`]) as separate pieces, so this composes them
/// in Go's order.
///
/// **Why the guarded pair and not `reset_worktree`.** Between the reuse loop's
/// safety check and the reset, a commit can land, or a file can be modified.
/// A plain reset discards both while every check reports success. The guarded
/// pair re-reads HEAD and re-checks dirtiness while holding git's own
/// `HEAD.lock`, so a change in that window is refused instead of destroyed.
/// When the checks pass, the worktree lands on exactly the commit it would
/// have before — same commit, same contents, plus two races closed.
///
/// `require_clean` is true only for a RECYCLED slot. Go's fresh path never
/// resets at all, so it never requires a clean tree; requiring it here would
/// newly fail an acquisition whose checkout filters left an untracked file
/// behind — a case that succeeds today and has nothing to do with the races.
fn reset_for_acquisition(
    backend: &Arc<dyn GitBackend>,
    slot: &Path,
    branch: &str,
    prior: &SeedInventory,
    origin: SlotOrigin,
) -> Result<(), PoolError> {
    // Non-git slots keep the pre-seam behavior. The guarded helpers shell out
    // to git, and a jj workspace has no git HEAD to lock — running them there
    // would guard the wrong repository rather than a wrong one.
    //
    // A git binary we cannot resolve is the same situation: the pool was
    // opened, so this is unexpected, but a slot that resets unguarded is
    // exactly what happened before this change and losing the guard is better
    // than failing every acquisition.
    let guarded = if backend.name() == vcs::BACKEND_GIT {
        resolved_git_bin()
    } else {
        None
    };

    let Some(git_bin) = guarded else {
        return backend
            .reset_worktree(slot, branch)
            .map_err(PoolError::Git);
    };

    // The guard is taken BEFORE anything is deleted, so a refusal leaves the
    // slot exactly as it was. Both SHAs it returns travel into the reset: they
    // are only meaningful as a pair, and re-deriving either at reset time is
    // the bug the pair exists to prevent.
    let guard = vcs::is_worktree_safe_to_reset(&git_bin, backend.as_ref(), slot, branch)
        .map_err(PoolError::Git)?;
    if !guard.safe {
        return Err(PoolError::Git(GitError::new(
            "worktree reset",
            format!(
                "refusing to reset {} to {branch:?}: its HEAD holds commits that \
                 branch does not contain",
                slot.display()
            ),
            GitErrorKind::Other,
        )));
    }

    // Only now, with the slot proven reusable, remove what the pool itself put
    // there. `authorized_paths` returns None for a missing OR empty inventory,
    // so an entry that lost its bookkeeping deletes nothing — the safety
    // property this whole seam exists for.
    if let Some(paths) = prior.authorized_paths() {
        remove_seeded_paths(slot, paths)?;
    }

    vcs::reset_worktree_to_ref(
        &git_bin,
        backend.as_ref(),
        slot,
        &guard.reset_ref,
        &guard.head,
        origin == SlotOrigin::Recycled,
    )
    .map_err(PoolError::Git)
}

/// Deletes exactly the paths a prior seeding recorded, and nothing else.
///
/// [`vcs::validate_seed_inventory`] runs FIRST and gates everything: it refuses
/// an empty list outright and rejects any entry that is absolute, contains `..`,
/// carries a backslash or NUL, or starts with `.git`/`.jj`. Only once the whole
/// inventory has been cleared is a single path touched.
///
/// Removal happens before the tracked tree is rewritten, so the inventory stays
/// the authority on what may be deleted: a tracked path can only appear at a
/// seeded location after `read-tree -u` restores it.
///
/// A listed path that is already gone is not a failure — the inventory is
/// permission to delete, not an obligation. A path that is neither a file nor a
/// directory (a socket, a device) IS a failure: refusing leaves the operator
/// looking at a real file this tool cannot safely remove, which beats silently
/// unlinking something it did not create.
fn remove_seeded_paths(slot: &Path, seeded: &[String]) -> Result<(), PoolError> {
    vcs::validate_seed_inventory(seeded).map_err(PoolError::Git)?;
    for name in seeded {
        let target = slot.join(name);
        match std::fs::symlink_metadata(&target) {
            // `symlink_metadata` does not follow, so a symlink is removed as a
            // link rather than as whatever it points at. Seeding flattens
            // symlinks into regular files precisely so this never has to guess.
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&target).map_err(|e| {
                PoolError::Git(GitError::new(
                    "worktree reset",
                    format!("removing seeded directory {}: {e}", target.display()),
                    GitErrorKind::Other,
                ))
            })?,
            Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
                std::fs::remove_file(&target).map_err(|e| {
                    PoolError::Git(GitError::new(
                        "worktree reset",
                        format!("removing seeded file {}: {e}", target.display()),
                        GitErrorKind::Other,
                    ))
                })?;
            }
            Ok(_) => {
                return Err(PoolError::Git(GitError::new(
                    "worktree reset",
                    format!(
                        "seeded path {} is neither a file nor a directory; refusing \
                         to remove it",
                        target.display()
                    ),
                    GitErrorKind::Other,
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(PoolError::Git(GitError::new(
                    "worktree reset",
                    format!("reading seeded path {}: {e}", target.display()),
                    GitErrorKind::Other,
                )));
            }
        }
    }
    Ok(())
}

/// Shares tracked file data between the source checkout and a FRESH slot
/// (Go `vcs.ShareWorktreeFiles`, gitvcs/sharing.go:19).
///
/// **Every failure path degrades to the plain copy the worktree already has.**
/// The files are written by `git worktree add` before this runs, so a refusal
/// here leaves a correct worktree that merely does not share storage. Failing
/// the acquisition instead would make a performance option an availability
/// hazard, and would be a behavior change for anyone who opts in and then hits
/// a filesystem this module declines to touch.
fn share_slot_files(repo_root: &Path, slot: &Path, err: &mut dyn std::io::Write) {
    use crate::fileclone;

    if !fileclone::SUPPORTED {
        let _ = writeln!(err, "APFS sharing skipped: only supported on macOS/APFS");
        return;
    }
    let reason = fileclone::filesystem_reason(repo_root, slot);
    if !reason.is_empty() {
        let _ = writeln!(err, "APFS sharing skipped: {reason}");
        return;
    }
    let Some(git_bin) = resolved_git_bin() else {
        let _ = writeln!(err, "APFS sharing skipped: no git binary to enumerate tracked paths");
        return;
    };

    // The module's contract is that the caller EXCLUSIVELY owns the destination
    // for the whole pass. Git's checkout and reference-transaction hooks run
    // before this insertion point and may have left asynchronous writers
    // behind, and a finished hook is not evidence that they are gone. Without
    // these checks the pass could race a live writer and publish bytes it never
    // hashed.
    if let Some(reason) = concurrent_writer_reason(repo_root, &git_bin) {
        let _ = writeln!(err, "APFS sharing skipped: {reason}");
        return;
    }

    let listed = match git_out(&git_bin, slot, &["ls-files", "-z"]) {
        Ok(v) => v,
        Err(_) => {
            let _ = writeln!(
                err,
                "APFS sharing skipped: tracked paths cannot be enumerated"
            );
            return;
        }
    };
    let paths: Vec<String> = String::from_utf8_lossy(&listed)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if paths.is_empty() {
        return;
    }

    match fileclone::share(repo_root, slot, &paths) {
        Ok(report) => {
            let _ = writeln!(err, "{report}");
        }
        Err(e) => {
            let _ = writeln!(
                err,
                "APFS sharing skipped: {e}; the worktree keeps its plain copy"
            );
        }
    }
}

/// The git binary this pool was opened with, re-resolved through the same
/// `GIT_BIN` → `PATH` lookup [`crate::git::ShellGitBackend::discover`] uses.
fn resolved_git_bin() -> Option<PathBuf> {
    crate::git::ShellGitBackend::discover()
        .ok()
        .map(|g| g.git_bin().to_path_buf())
}

/// Runs `git <args>` in `cwd` and returns raw stdout, NUL bytes intact.
///
/// Bytes, not text: `ls-files -z` separates paths with NUL precisely so that a
/// filename containing a newline still round-trips, and decoding here would
/// throw that away.
///
/// Failures are labelled by [`crate::vcs::git_label`], the one rule for what
/// goes in a [`GitError`]'s `command` field — spelling "git " in here as well
/// is what rendered `git git ls-files -z: …`.
fn git_out(git_bin: &Path, cwd: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let out = raw_git(git_bin, cwd, args)?;
    if !out.0 {
        return Err(GitError::new(
            crate::vcs::git_label(args),
            String::from_utf8_lossy(&out.2).trim().to_string(),
            GitErrorKind::Other,
        ));
    }
    Ok(out.1)
}

/// Runs `git <args>` in `cwd`, returning `(success, stdout, stderr)` WITHOUT
/// treating a nonzero exit as an error.
///
/// Needed because `git config --get` exits 1 to mean "not set", which is the
/// ordinary case — and which a caller must be able to tell apart from a real
/// failure. Collapsing both into one `Err` makes the common case look like an
/// unverifiable one.
fn raw_git(git_bin: &Path, cwd: &Path, args: &[&str]) -> Result<(bool, Vec<u8>, Vec<u8>), GitError> {
    let out = std::process::Command::new(git_bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| {
            GitError::new(
                crate::vcs::git_label(args),
                e.to_string(),
                GitErrorKind::Other,
            )
        })?;
    Ok((out.status.success(), out.stdout, out.stderr))
}

/// Whether something may still be writing to these checkouts, which would make
/// an exclusive-ownership assumption unsafe.
///
/// A user-defined fsmonitor, and an executable `post-checkout` or
/// `reference-transaction` hook, may both have started background processes.
/// Go checks the same two things for the same reason (gitvcs/sharing.go:26-56).
/// This does NOT check smudge/clean filters: a filter that reports itself
/// configured would mean giving up coverage on every LFS checkout, and Go
/// declines the same trade.
fn concurrent_writer_reason(repo_root: &Path, git_bin: &Path) -> Option<&'static str> {
    match raw_git(git_bin, repo_root, &["config", "--get", "core.fsmonitor"]) {
        Ok((true, value, _)) => {
            // Booleans select git's BUILT-IN monitor, not a script. Only a
            // non-boolean value names something that could be running.
            let v = String::from_utf8_lossy(&value).trim().to_ascii_lowercase();
            let builtin = matches!(
                v.as_str(),
                "" | "false" | "0" | "no" | "off" | "true" | "1" | "yes" | "on"
            );
            if !builtin {
                return Some("Git fsmonitor hook may have started a writer");
            }
        }
        // Exit code 1 is git's documented "key not set" — the ordinary case,
        // and the one Go treats as safe (gitvcs/sharing.go:30-31). Any other
        // failure leaves us unable to tell "unset" from "we could not ask",
        // and unverifiable is not safe.
        Ok((false, _, _)) => {}
        Err(_) => return Some("Git fsmonitor configuration cannot be verified"),
    }
    for hook in ["post-checkout", "reference-transaction"] {
        if let Some(reason) = hook_may_have_started_a_writer(repo_root, git_bin, hook) {
            return Some(reason);
        }
    }
    None
}

/// Whether `hook` is installed AND executable at the repository root.
fn hook_may_have_started_a_writer(
    root: &Path,
    git_bin: &Path,
    hook: &str,
) -> Option<&'static str> {
    let out = git_out(
        git_bin,
        root,
        &["rev-parse", "--git-path", &format!("hooks/{hook}")],
    )
    .ok()?;
    // Only the trailing newline is framing; whitespace inside a path is part
    // of the path, and trimming it would point the check at the wrong file.
    let raw = String::from_utf8_lossy(&out);
    let reported = raw.strip_suffix('\n').unwrap_or(&raw);
    let reported = Path::new(reported);
    let path = if reported.is_absolute() {
        reported.to_path_buf()
    } else {
        root.join(reported)
    };
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if !meta.file_type().is_symlink() && meta.is_file() && is_executable(&meta) => {
            Some("Git hook may have started a writer")
        }
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some("Git hook configuration cannot be verified"),
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Marks an entry as acquired (owner reservation or lease), returning lease
/// info if leasing. Extracted so both the reused and new-worktree paths share
/// it.
fn mark_acquired_entry(
    wt: &mut WorktreeEntry,
    opts: &AcquireOptions,
    process: &ProcessTable,
) -> Option<Lease> {
    if let Some(lease_opts) = &opts.lease {
        let now = chrono::Utc::now();
        let id = mark_acquired_lease(wt, &lease_opts.holder, now);
        let expires_at = lease_opts.ttl.map(|d| now + d);
        if let Some(exp) = expires_at {
            wt.expires_at = exp;
        }
        Some(Lease {
            id,
            holder: lease_opts.holder.clone(),
            acquired_at: now,
            expires_at,
        })
    } else {
        let pid = std::process::id() as i32;
        wt.owner_pid = pid;
        wt.owner_started_at = process.started_at(pid).unwrap_or(0);
        None
    }
}

/// Whether `cwd` is inside `worktree_path` (Go `cwdInWorktree`).
fn cwd_in_worktree(cwd: &Path, worktree_path: &Path) -> bool {
    use crate::process::pathdiff_rel;
    let abs_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let abs_wt =
        std::fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    match pathdiff_rel(&abs_wt, &abs_cwd) {
        Some(rel) => {
            let s = rel.to_string_lossy();
            // "." = cwd is the worktree; otherwise a descendant that isn't
            // ".." or "../...".
            s == "." || (s != ".." && !s.starts_with("../") && !s.starts_with("..\\"))
        }
        None => false,
    }
}

/// Preconditions for a conditional release.
///
/// The three booleans are predicates, not flavours of the identity fields, and
/// each one exists because a caller hands release authority to something
/// other than the session that acquired the slot. `treehouse lease` and
/// `treehouse return --all` both do exactly that: either one can act on a slot
/// somebody else is holding, so without a guard the release would reset the
/// worktree and clear the *new* owner's reservation while the new owner is
/// still working in it.
///
/// They are checked under the same lock as the release (Go's
/// `ValidateReleasePreconditions`, pool.go:967-981): checking and then acting
/// in separate critical sections leaves a takeover window between them, and
/// the reset is destructive.
#[derive(Debug, Clone, Default)]
pub struct ReleasePreconditions {
    pub expected_lease_id: Option<String>,
    pub expected_lease_holder: Option<String>,
    /// Refuse unless the slot still carries THIS process's own owner
    /// reservation. This is what an acquiring `treehouse get` holds until it
    /// returns the slot.
    pub require_owned_by_caller: bool,
    /// Refuse once anybody has leased the slot. Replays an observation of an
    /// unleased slot — "reclaim what nobody held" must not reclaim what
    /// somebody now holds.
    pub require_unleased: bool,
    /// Refuse a recovered entry. Nothing about a slot the recovery scan had to
    /// reconstruct proves it idle, so it is only released by naming it.
    pub refuse_recovered: bool,
}

/// Finds a managed, releasable worktree by path, validating preconditions
/// (Go `releasableWorktree` + `validateReleasePreconditions`).
fn releasable_worktree<'a>(
    state: &'a mut State,
    worktree_path: &str,
    preconditions: &ReleasePreconditions,
    process: &ProcessTable,
) -> Result<&'a mut WorktreeEntry, PoolError> {
    for wt in &mut state.worktrees {
        if wt.path != worktree_path {
            continue;
        }
        if wt.destroying {
            return Err(PoolError::BeingDestroyed(worktree_path.to_string()));
        }
        validate_release_preconditions_inner(wt, preconditions, process)?;
        return Ok(wt);
    }
    Err(PoolError::NotFound(worktree_path.to_string()))
}

fn validate_release_preconditions_inner(
    wt: &WorktreeEntry,
    preconditions: &ReleasePreconditions,
    process: &ProcessTable,
) -> Result<(), PoolError> {
    if preconditions.require_owned_by_caller {
        check_owned_by_caller(wt, process)?;
    }
    if preconditions.refuse_recovered
        && wt.leased
        && wt.lease_holder == crate::state::RECOVERED_LEASE_HOLDER
    {
        return Err(PoolError::RecoveredEntry(wt.path.clone()));
    }
    // `require_unleased` asserts the ABSENCE of a lease, which is the opposite
    // of what the two identity fields assert, so it can never fall through to
    // them: it returns here either way.
    if preconditions.require_unleased {
        if wt.leased {
            return Err(PoolError::LeasePrecondition {
                path: wt.path.clone(),
                reason: format!(
                    "worktree {} was observed unleased and is leased now",
                    wt.path
                ),
            });
        }
        return Ok(());
    }
    if preconditions.expected_lease_id.is_none() && preconditions.expected_lease_holder.is_none() {
        return Ok(());
    }
    if !wt.leased {
        return Err(PoolError::LeasePrecondition {
            path: wt.path.clone(),
            reason: "worktree is not leased".into(),
        });
    }
    if let Some(id) = &preconditions.expected_lease_id
        && &wt.lease_id != id
    {
        return Err(PoolError::LeasePrecondition {
            path: wt.path.clone(),
            reason: format!("lease identity does not match worktree {}", wt.path),
        });
    }
    if let Some(holder) = &preconditions.expected_lease_holder
        && &wt.lease_holder != holder
    {
        return Err(PoolError::LeasePrecondition {
            path: wt.path.clone(),
            reason: format!("lease holder does not match worktree {}", wt.path),
        });
    }
    Ok(())
}

/// Whether the slot still carries this process's own owner reservation (Go
/// `checkOwnedByCaller`, pool.go:1418-1438).
///
/// `leased` is tested FIRST and deliberately: `mark_acquired`'s lease path
/// zeroes the owner pair, so a durably-leased slot reads as "owner_pid == 0"
/// and would otherwise be reported as "already released" — the one answer that
/// lets the caller think nobody took it.
fn check_owned_by_caller(wt: &WorktreeEntry, process: &ProcessTable) -> Result<(), PoolError> {
    if wt.leased {
        return Err(PoolError::OwnerPrecondition {
            path: wt.path.clone(),
            reason: if wt.lease_holder.is_empty() {
                "it is now durably leased".to_string()
            } else {
                format!("it is now durably leased (holder: {:?})", wt.lease_holder)
            },
        });
    }
    if wt.owner_pid == 0 {
        return Err(PoolError::OwnerPrecondition {
            path: wt.path.clone(),
            reason: "it was already released".to_string(),
        });
    }
    let pid = std::process::id() as i32;
    // Both fields are compared: a bare PID can have been recycled onto an
    // unrelated process, whose reservation must not be mistaken for ours.
    let Some(started_at) = process.started_at(pid) else {
        return Err(PoolError::OwnerPrecondition {
            path: wt.path.clone(),
            reason: "this process's own identity could not be read to confirm the reservation"
                .to_string(),
        });
    };
    if wt.owner_pid != pid || wt.owner_started_at != started_at {
        return Err(PoolError::OwnerPrecondition {
            path: wt.path.clone(),
            reason: "it is now reserved by another session".to_string(),
        });
    }
    Ok(())
}

/// The next numeric worktree name (Go `nextName`).
pub fn next_name(state: &State) -> String {
    let mut max = 0;
    for wt in &state.worktrees {
        if let Ok(n) = wt.name.parse::<i32>()
            && n > max
        {
            max = n;
        }
    }
    (max + 1).to_string()
}

/// The next numeric worktree name that is free BOTH in state and on disk.
///
/// `next_name` alone derives the name from state, which is not enough: a
/// worktree whose state entry was lost (a crash between `git worktree add`
/// and the state write, a truncated state file, a hand-deleted entry) still
/// occupies `<pool>/<n>/`, and reissuing that number would run `git worktree
/// add` into a directory that already holds a real worktree. The state
/// recovery scan in `state.rs` already adopts such directories as quarantined
/// leases, but acquisition must not depend on that scan having run in this
/// process — so the filesystem is the authority here, exactly as Go's
/// `resolveWorktreePath` skips occupied candidates.
///
/// Fail-closed: if the pool directory cannot be read, the computed name is
/// still returned and the subsequent `worktree_add` is what surfaces the
/// problem. Silently returning a different number would hide the fault.
pub(crate) fn next_free_name(pool_dir: &Path, state: &State) -> String {
    let mut max = next_name(state).parse::<i64>().unwrap_or(1) - 1;
    if let Ok(entries) = std::fs::read_dir(pool_dir) {
        for entry in entries.flatten() {
            if let Ok(n) = entry.file_name().to_string_lossy().parse::<i64>()
                && n > max
            {
                max = n;
            }
        }
    }
    (max + 1).to_string()
}

/// The VCS marker a pooled worktree must carry, as a backend name (Go
/// `vcs.WorktreeBackendName`, pool.go:499-518).
///
/// Returns `""` for a damaged or missing slot. There is deliberately NO
/// fallback to a configured backend: without the marker, every git dispatch
/// on that path walks UP to whatever repository encloses it, so in the
/// supported in-project pool layout (`root = "."`, which nests `.treehouse`
/// under the repo) the safety checks would vouch for the user's own working
/// tree and the reset would rewrite it. `""` makes the caller skip the slot
/// instead — destroy classifies it unverified, prune skips it as
/// unverifiable, and neither path ever resets it.
///
/// A slot is "damaged" for BOTH reasons the seam distinguishes — no marker at
/// all, and a marker whose target cannot be read — because a slot in either
/// state is one this pool must not touch. That collapsing is safe here and
/// only here: the caller SKIPS, and skipping is the correct answer for both.
/// The distinction matters one level up, in
/// [`vcs::destructive_backend_for_worktree`], which must report a read failure
/// as a failure rather than answer it with the enclosing repository. So the
/// marker rule itself lives in [`vcs::worktree_backend_name`] and this is the
/// pool's own damage test on top of it.
///
/// Only a GIT slot counts as intact. A `.jj` marker is a real answer from the
/// seam, but the pool lifecycle cannot manage a jj slot yet — its reuse checks
/// are git calls, and running them in a non-git tree fails in terms that hide
/// the real cause. Reporting such a slot as damaged keeps it out of the reuse
/// loop AND keeps `is_readable_worktree` / destroy's unverified
/// classification exactly as they are. Admitting another flavor is a
/// one-line change HERE once the pool carries flavor state for it (Go
/// `pool.go`'s `wantFlavor` counters); nothing else in the pool needs to move.
pub(crate) fn slot_backend_name(worktree: &Path) -> String {
    match vcs::worktree_backend_name(worktree) {
        Ok(Some(name @ vcs::BACKEND_GIT)) => name.to_string(),
        _ => String::new(),
    }
}

/// Whether two paths name the same filesystem object (Go `os.SameFile`).
///
/// Compares device + inode on unix, so neither a symlinked path nor letter
/// case on a case-insensitive filesystem splits one clone in two. Windows has
/// no stable equivalent (`file_index` is still unstable), so there the
/// comparison is on the canonicalized path — a weaker check that fails CLOSED:
/// if canonicalization fails for either side the two are reported as
/// different, and a slot is never reclaimed on an unproven match.
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Errors from pool operations.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error(
        "all {count} worktrees are in use or dirty (max_trees = {max}). Run 'treehouse status' to see details, or increase max_trees in treehouse.toml"
    )]
    PoolFull { count: u32, max: u32 },
    /// A full pool where some slots were skipped for a reason the operator can
    /// act on rather than "in use": they belong to another clone, or their
    /// owner could not be proven. Go names both counts in its pool-full
    /// message (pool.go:641-646) because the remedy differs — a slot from
    /// another clone is never reclaimed from here, so raising `max_trees` is
    /// the only way forward, and saying so beats a generic full pool.
    #[error(
        "all {count} worktrees are in use, dirty, or not provably this clone's ({other_clone} belong to another clone; {unverified_clone} whose clone identity cannot be verified; max_trees = {max}). A worktree is reused only by the clone it belongs to. Run 'treehouse status' to see details, or increase max_trees in treehouse.toml"
    )]
    PoolFullNotOurs {
        count: u32,
        other_clone: u32,
        unverified_clone: u32,
        max: u32,
    },
    #[error("worktree {0} is being destroyed")]
    BeingDestroyed(String),
    #[error("worktree {0} is not managed by treehouse")]
    NotFound(String),
    /// The caller no longer owns the slot it is releasing (Go
    /// `ErrOwnerPreconditionFailed`). Distinct from a lease mismatch because
    /// nothing about the lease changed — a *durable lease took over a live
    /// owner*, or another session reserved the slot.
    #[error("owner precondition failed: {path}: {reason}")]
    OwnerPrecondition { path: String, reason: String },
    #[error("lease precondition failed: {path}: {reason}")]
    LeasePrecondition { path: String, reason: String },
    /// A recovered entry (Go `ErrRecoveredEntry`): the state file had to be
    /// reconstructed, so nothing on disk proves the slot is idle.
    #[error("worktree {0} was recovered and is only returned by name")]
    RecoveredEntry(String),
    #[error("worktree {name} is already leased (holder: {holder:?})")]
    AlreadyLeased { name: String, holder: String },
    #[error(
        "worktree {0} is registered but its directory no longer exists; run 'treehouse status' to clear the stale entry"
    )]
    StaleEntry(String),
    #[error("no worktree named {0:?} in pool")]
    NoSuchWorktree(String),
    #[error("pool lock: {0}")]
    Lock(String),
    #[error("state: {0}")]
    State(#[from] crate::state::StateError),
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("git: {0}")]
    Git(#[from] crate::git::GitError),
    #[error("io: {0}")]
    Io(String, std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::mark_acquired_lease;

    #[test]
    fn next_name_increments() {
        let s = State {
            worktrees: vec![
                WorktreeEntry {
                    name: "1".into(),
                    ..Default::default()
                },
                WorktreeEntry {
                    name: "3".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(next_name(&s), "4");
        assert_eq!(next_name(&State::default()), "1");
    }

    #[test]
    fn next_name_ignores_non_numeric() {
        let s = State {
            worktrees: vec![WorktreeEntry {
                name: "abc".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(next_name(&s), "1");
    }

    #[test]
    fn cwd_in_worktree_detects_self_and_descendant() {
        let wt = Path::new("/home/u/proj/.treehouse/repo-abc/1/repo");
        assert!(cwd_in_worktree(wt, wt));
        assert!(cwd_in_worktree(&wt.join("src"), wt));
        assert!(!cwd_in_worktree(Path::new("/home/u/proj"), wt));
        assert!(!cwd_in_worktree(Path::new("/home/u/proj/other"), wt));
    }

    #[test]
    fn releasable_worktree_validates_preconditions() {
        let process = ProcessTable::new();
        let mut state = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: "/pool/1/repo".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        // No preconditions: any managed path is releasable.
        let wt = releasable_worktree(
            &mut state,
            "/pool/1/repo",
            &ReleasePreconditions::default(),
            &process,
        )
        .unwrap();
        assert_eq!(wt.name, "1");

        // Unknown path.
        assert!(matches!(
            releasable_worktree(
                &mut state,
                "/nope",
                &ReleasePreconditions::default(),
                &process
            ),
            Err(PoolError::NotFound(_))
        ));

        // Destroying => BeingDestroyed.
        state.worktrees[0].destroying = true;
        assert!(matches!(
            releasable_worktree(
                &mut state,
                "/pool/1/repo",
                &ReleasePreconditions::default(),
                &process
            ),
            Err(PoolError::BeingDestroyed(_))
        ));
        state.worktrees[0].destroying = false;

        // Lease precondition: id mismatch fails, match passes.
        let cond = ReleasePreconditions {
            expected_lease_id: Some("abc".into()),
            ..Default::default()
        };
        assert!(matches!(
            releasable_worktree(&mut state, "/pool/1/repo", &cond, &process),
            Err(PoolError::LeasePrecondition { .. })
        ));
        mark_acquired_lease(&mut state.worktrees[0], "h", chrono::Utc::now());
        assert!(releasable_worktree(&mut state, "/pool/1/repo", &cond, &process).is_err());
        state.worktrees[0].lease_id = "abc".into();
        assert!(releasable_worktree(&mut state, "/pool/1/repo", &cond, &process).is_ok());
    }

    // ─── M-026: the three release preconditions that guard a third party ────

    /// `require_owned_by_caller` is the guard that makes it safe for a `get`
    /// session to release "its own" slot: without it a session whose slot was
    /// taken over still resets the worktree under the new owner.
    #[test]
    fn require_owned_by_caller_refuses_a_slot_another_session_reserved() {
        let process = ProcessTable::new();
        let me = std::process::id() as i32;
        let started = process.started_at(me).unwrap();
        let cond = ReleasePreconditions {
            require_owned_by_caller: true,
            ..Default::default()
        };
        let mut state = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: "/pool/1/repo".into(),
                owner_pid: me,
                owner_started_at: started,
                ..Default::default()
            }],
            ..Default::default()
        };
        // Ours: releasable.
        assert!(releasable_worktree(&mut state, "/pool/1/repo", &cond, &process).is_ok());

        // Another session reserved it (a different PID+start pair).
        state.worktrees[0].owner_pid = 999_999;
        state.worktrees[0].owner_started_at = 1;
        assert!(matches!(
            releasable_worktree(&mut state, "/pool/1/repo", &cond, &process),
            Err(PoolError::OwnerPrecondition { .. })
        ));

        // Already released.
        state.worktrees[0].owner_pid = 0;
        state.worktrees[0].owner_started_at = 0;
        assert!(matches!(
            releasable_worktree(&mut state, "/pool/1/repo", &cond, &process),
            Err(PoolError::OwnerPrecondition { ref reason, .. }) if reason == "it was already released"
        ));

        // A durable lease over a live owner home MUST report "durably leased",
        // not "already released": `mark_acquired`'s lease path zeroes the owner
        // pair, so testing `owner_pid == 0` first would call a takeover "free".
        mark_acquired_lease(&mut state.worktrees[0], "other-agent", chrono::Utc::now());
        match releasable_worktree(&mut state, "/pool/1/repo", &cond, &process) {
            Err(PoolError::OwnerPrecondition { reason, .. }) => {
                assert!(reason.contains("durably leased"), "got: {reason}");
                assert!(reason.contains("other-agent"), "got: {reason}");
            }
            other => panic!("expected an owner precondition, got {other:?}"),
        }
    }

    /// `require_unleased` replays "reclaim what nobody held": a slot somebody
    /// leased since the listing must be refused, not reset.
    #[test]
    fn require_unleased_refuses_a_slot_lease_since_the_listing() {
        let process = ProcessTable::new();
        let cond = ReleasePreconditions {
            require_unleased: true,
            ..Default::default()
        };
        let mut state = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: "/pool/1/repo".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(releasable_worktree(&mut state, "/pool/1/repo", &cond, &process).is_ok());
        mark_acquired_lease(&mut state.worktrees[0], "someone", chrono::Utc::now());
        assert!(matches!(
            releasable_worktree(&mut state, "/pool/1/repo", &cond, &process),
            Err(PoolError::LeasePrecondition { .. })
        ));
    }

    /// A recovered entry proves nothing about idle-ness, so a bulk release must
    /// leave it for a return that names it.
    #[test]
    fn refuse_recovered_refuses_a_recovered_entry() {
        let process = ProcessTable::new();
        let cond = ReleasePreconditions {
            refuse_recovered: true,
            ..Default::default()
        };
        let mut state = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: "/pool/1/repo".into(),
                leased: true,
                lease_holder: crate::state::RECOVERED_LEASE_HOLDER.to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(matches!(
            releasable_worktree(&mut state, "/pool/1/repo", &cond, &process),
            Err(PoolError::RecoveredEntry(_))
        ));
        // ...and an ordinary lease is untouched by the same precondition.
        state.worktrees[0].lease_holder = "real-agent".into();
        assert!(releasable_worktree(&mut state, "/pool/1/repo", &cond, &process).is_ok());
    }

    /// `treehouse lease <name>` (M-026 / #128): state-only, refuses a slot
    /// already leased, and reports a vanished directory as stale rather than
    /// as "no worktree named X".
    #[test]
    fn lease_existing_leases_by_name_without_touching_the_worktree() {
        let home = tempfile::tempdir().unwrap();
        let (_repo_dir, repo) = init_repo();
        let pool = pool_over(&home, &repo);
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                ..Default::default()
            })
            .unwrap();
        pool.release(&acquired.path.to_string_lossy()).unwrap();

        let info = pool.lease_existing(&acquired.name, "agent-42").unwrap();
        assert_eq!(info.path, acquired.path.to_string_lossy());
        assert_eq!(info.lease_holder, "agent-42");
        assert_eq!(info.lease_id.len(), 32);
        // Status reports it leased, and the directory is untouched.
        let status = pool.status().unwrap();
        assert_eq!(status[0].status, crate::pool::STATUS_LEASED);
        assert!(acquired.path.join("README.md").exists());

        // A second lease is refused rather than re-minting an identity.
        assert!(matches!(
            pool.lease_existing(&acquired.name, "other"),
            Err(PoolError::AlreadyLeased { .. })
        ));

        // Unknown name.
        assert!(matches!(
            pool.lease_existing("nope", "x"),
            Err(PoolError::NoSuchWorktree(_))
        ));

        // The lease pins conditional release: `--if-lease-id` for that exact
        // identity still matches, so `return --all` can reclaim it safely.
        assert!(
            pool.release_conditional(
                &acquired.path.to_string_lossy(),
                &ReleasePreconditions {
                    expected_lease_id: Some(info.lease_id.clone()),
                    refuse_recovered: true,
                    ..Default::default()
                },
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn is_pool_dir_matches_the_state_file_marker() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Pool::is_pool_dir(dir.path()), "an empty dir is not a pool");
        std::fs::write(State::state_file_path(dir.path()), br#"{"worktrees":[]}"#).unwrap();
        assert!(Pool::is_pool_dir(dir.path()));
    }

    #[test]
    fn worktree_names_and_lookup_by_name_and_path() {
        let home = tempfile::tempdir().unwrap();
        let (_repo_dir, repo) = init_repo();
        let pool = pool_over(&home, &repo);
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(pool.worktree_names().unwrap(), vec!["1".to_string()]);
        assert_eq!(
            pool.find_by_name("1").unwrap().unwrap().path,
            acquired.path.to_string_lossy()
        );
        assert!(pool.find_by_name("2").unwrap().is_none());
        assert_eq!(
            pool.find_by_path(&acquired.path.to_string_lossy())
                .unwrap()
                .unwrap()
                .name,
            "1"
        );
        assert!(pool.find_by_path("/elsewhere").unwrap().is_none());
    }

    /// M-023: `--root` outranks `TREEHOUSE_ROOT` (Go `config.ResolveRoot`:
    /// flag, then env, then config, then default).
    #[test]
    fn root_override_outranks_the_env_var_and_the_config_root() {
        let (_dir, repo) = init_repo();
        let from_flag = tempfile::tempdir().unwrap();
        let from_env = tempfile::tempdir().unwrap();
        let from_config = tempfile::tempdir().unwrap();
        let opts = OpenOptions {
            config: TreehouseConfig {
                root: Some(from_config.path().to_str().unwrap().to_string()),
                ..TreehouseConfig::default_config()
            },
            root_override: Some(from_flag.path().to_str().unwrap().to_string()),
            ..Default::default()
        };
        let env = crate::env::InMemoryEnv::new(from_env.path().to_path_buf())
            .with_env(TREEHOUSE_ROOT_VAR, from_env.path().to_str().unwrap());
        let pool = Pool::open_with_env(&repo, None, &opts, Arc::new(env)).unwrap();
        assert!(
            pool.pool_dir().starts_with(from_flag.path()),
            "the flag must win; got {:?}",
            pool.pool_dir()
        );
    }

    #[test]
    fn integration_acquire_status_release_roundtrip() {
        use crate::git::{GitBackend, ShellGitBackend};
        // Build a temp repo with a commit on main.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        // `init` runs in the parent (the repo dir doesn't exist yet); the rest
        // run inside the repo.
        let init_out = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .expect("git must be installed");
        assert!(
            init_out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init_out.stderr)
        );
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git must be installed");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["init", "--initial-branch=main", repo.to_str().unwrap()]);
        run_git(&["config", "user.email", "t@t.com"]);
        run_git(&["config", "user.name", "T"]);
        std::fs::write(
            repo.join("README.md"),
            b"hi
",
        )
        .unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "init"]);
        // Verify main exists (the commit created it).
        let main_ok = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            main_ok.status.success(),
            "refs/heads/main missing after commit"
        );

        // Open a pool rooted at a temp HOME.
        let fake_home = tempfile::tempdir().unwrap();
        let opts = OpenOptions {
            config: TreehouseConfig {
                root: Some(fake_home.path().to_str().unwrap().to_string()),
                ..TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        // Directly test default_branch resolution.
        let shell = ShellGitBackend::discover().unwrap();
        let grepo = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };
        let db = shell.default_branch(&grepo).unwrap();
        assert_eq!(db, "main", "default_branch should be main, got {db}");

        let pool = Pool::open(&repo, None, &opts).unwrap();
        assert!(pool.pool_dir().exists());

        // Acquire (creates worktree 1). Explicit branch avoids any
        // process-global config races from parallel tests.
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(acquired.name, "1");
        assert!(acquired.path.exists());

        // Status: 1 worktree, available (no process inside).
        let status = pool.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "1");

        // Release it.
        pool.release(&acquired.path.to_string_lossy()).unwrap();

        // Re-acquire: reuses worktree 1 (not a new one).
        let again = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(again.name, "1");
        pool.release(&again.path.to_string_lossy()).unwrap();
        let _ = ShellGitBackend::discover().unwrap();
        let _: &dyn GitBackend = &ShellGitBackend::discover().unwrap();
    }
    #[test]
    fn status_constants_match_go() {
        assert_eq!(STATUS_AVAILABLE, "available");
        assert_eq!(STATUS_IN_USE, "in-use");
        assert_eq!(STATUS_DIRTY, "dirty");
        assert_eq!(STATUS_LEASED, "leased");
        assert_eq!(STATUS_HERE, "you're here");
        assert_eq!(STATUS_DAMAGED, "damaged");
    }

    // ─── M-008: the injected env (--env-path) must reach pool resolution ────

    /// Builds a repo with one commit on `main`. Returns (tempdir, repo path);
    /// the TempDir must be kept alive by the caller.
    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let out = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .expect("git must be installed");
        assert!(out.status.success(), "git init failed");
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["config", "user.email", "t@t.com"]);
        run_git(&["config", "user.name", "T"]);
        std::fs::write(repo.join("README.md"), b"hi\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "init"]);
        (dir, repo)
    }

    // ─── acquisition: seeding, guarded reset, and the seed-removal contract ──

    /// A repo whose committed `.worktreeinclude` selects `*.env`, plus one
    /// ignored file for it to find. `*.env` matches nothing tracked, so the
    /// seed list is exactly what the manifest names.
    ///
    /// The ignore rule and the manifest are committed with `-f` because
    /// `.gitignore` and `.worktreeinclude` ignore themselves by pattern in
    /// most setups — the same thing `seed_repo` in `git/shell.rs` does.
    fn seeding_repo() -> (tempfile::TempDir, PathBuf, tempfile::TempDir) {
        let (dir, repo) = init_repo();
        std::fs::write(repo.join(".worktreeinclude"), "*.env\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "*.env\n").unwrap();
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["add", "-f", ".gitignore", ".worktreeinclude"]);
        run_git(&["commit", "-m", "add seeding manifest"]);
        std::fs::write(repo.join(".env"), "SECRET=from-source\n").unwrap();
        (dir, repo, tempfile::tempdir().unwrap())
    }

    fn pool_for(repo: &Path, home: &Path) -> Pool {
        let opts = OpenOptions {
            config: TreehouseConfig {
                root: Some(home.to_string_lossy().into_owned()),
                ..TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        Pool::open(repo, None, &opts).unwrap()
    }

    fn entry_for<'a>(state: &'a State, name: &str) -> &'a WorktreeEntry {
        state
            .worktrees
            .iter()
            .find(|w| w.name == name)
            .expect("the entry must be in state")
    }

    #[test]
    fn acquisition_seeds_the_worktree_and_records_the_inventory() {
        let (_dir, repo, home) = seeding_repo();
        let pool = pool_for(&repo, home.path());

        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                skip_fetch: true,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(acquired.path.join(".env")).unwrap(),
            "SECRET=from-source\n",
            "the committed .worktreeinclude must actually seed the worktree"
        );

        let state = State::read_state(&pool.dir).unwrap();
        let inventory = entry_for(&state, &acquired.name).seed_inventory();
        assert!(
            inventory.known,
            "an acquisition that seeded files must record a KNOWN inventory"
        );
        assert_eq!(inventory.paths, vec![".env".to_string()]);
        assert_eq!(inventory.backend, vcs::BACKEND_GIT);
        assert_eq!(
            inventory.authorized_paths(),
            Some([".env".to_string()].as_slice()),
            "the recorded inventory is what the next reset may delete"
        );
    }

    #[test]
    fn recycling_a_slot_reseeds_it_and_leaves_a_users_ignored_file_alone() {
        let (_dir, repo, home) = seeding_repo();
        let pool = pool_for(&repo, home.path());
        let opts = AcquireOptions {
            branch: Some("main".into()),
            skip_fetch: true,
            ..Default::default()
        };

        let first = pool.get(&opts).unwrap();
        assert!(first.path.join(".env").exists());

        // A file the USER placed, not the pool. Same ignore class, different
        // author — the distinction the whole seed inventory exists to draw.
        std::fs::write(first.path.join("mine.env"), "KEEP=mine\n").unwrap();
        pool.release(&first.path.to_string_lossy()).unwrap();

        let second = pool.get(&opts).unwrap();
        assert_eq!(second.name, first.name, "the slot must have been recycled");
        assert_eq!(
            std::fs::read_to_string(second.path.join(".env")).unwrap(),
            "SECRET=from-source\n",
            "the seed must be present again after the recycle reset"
        );
        assert_eq!(
            std::fs::read_to_string(second.path.join("mine.env")).unwrap(),
            "KEEP=mine\n",
            "an ignored file the pool did not seed must survive the reset"
        );

        let state = State::read_state(&pool.dir).unwrap();
        let inventory = entry_for(&state, &second.name).seed_inventory();
        assert_eq!(
            inventory.paths,
            vec![".env".to_string()],
            "only what the pool seeded may be listed; the user's file must not be"
        );
    }

    #[test]
    fn an_include_manifest_selects_exactly_what_it_lists() {
        let (_dir, repo, home) = seeding_repo();
        let pool = pool_for(&repo, home.path());

        // The committed manifest selects `*.env`; this one selects nothing, so
        // the acquisition must seed nothing and must NOT fall back to it.
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                skip_fetch: true,
                include_manifest: Some(Vec::new()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !acquired.path.join(".env").exists(),
            "an empty manifest means seed nothing, not 'use the committed one'"
        );
        let state = State::read_state(&pool.dir).unwrap();
        assert!(
            entry_for(&state, &acquired.name)
                .seed_inventory()
                .authorized_paths()
                .is_none(),
            "seeding nothing must leave an inventory that authorizes nothing"
        );

        pool.release(&acquired.path.to_string_lossy()).unwrap();

        // A manifest that names a real ignored file seeds exactly that file.
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                skip_fetch: true,
                include_manifest: Some(b"other.env\n".to_vec()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !acquired.path.join(".env").exists(),
            "the committed manifest must be REPLACED, not merged"
        );
        assert!(!acquired.path.join("other.env").exists());
        let state = State::read_state(&pool.dir).unwrap();
        assert!(
            entry_for(&state, &acquired.name)
                .seed_inventory()
                .authorized_paths()
                .is_none(),
            "a manifest naming an absent file selects nothing, which is valid"
        );
    }

    #[test]
    fn releasing_a_seeded_slot_clears_its_inventory_to_a_verified_empty_one() {
        let (_dir, repo, home) = seeding_repo();
        let pool = pool_for(&repo, home.path());
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                skip_fetch: true,
                ..Default::default()
            })
            .unwrap();

        pool.release(&acquired.path.to_string_lossy()).unwrap();

        let state = State::read_state(&pool.dir).unwrap();
        let inventory = entry_for(&state, &acquired.name).seed_inventory();
        assert!(
            inventory.known,
            "a released slot holds nothing we seeded — that is a claim, not a gap"
        );
        assert!(
            inventory.paths.is_empty(),
            "the stale list must not outlive the files it named"
        );
        assert!(
            !acquired.path.join(".env").exists(),
            "release removes the pool's own seeds"
        );
    }

    // ─── the guarded reset ───────────────────────────────────────────────────

    /// Builds a linked worktree at `main` and returns the pool's reset
    /// function bound to it, so a test can stage a race and then reset.
    fn reset_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Arc<dyn GitBackend>, PathBuf) {
        let (dir, repo) = init_repo();
        let slot = dir.path().join("slot");
        let git: Arc<dyn GitBackend> = Arc::new(crate::git::ShellGitBackend::discover().unwrap());
        let out = std::process::Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&slot)
            .arg("main")
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let bin = crate::git::ShellGitBackend::discover()
            .unwrap()
            .git_bin()
            .to_path_buf();
        (dir, repo, slot, git, bin)
    }

    #[test]
    fn a_slot_whose_head_moved_since_the_safety_check_is_not_reset() {
        // The race the guarded pair exists for. `Pool::get` takes the guard and
        // resets in one function; here the two calls are made separately with a
        // commit landing between them, which is exactly the interleaving the
        // pre-seam code silently discarded.
        let (_d, _repo, slot, git, bin) = reset_fixture();
        let guard = vcs::is_worktree_safe_to_reset(&bin, git.as_ref(), &slot, "main").unwrap();
        assert!(guard.safe);

        // The window: a commit lands in the slot after the check.
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&slot)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        std::fs::write(slot.join("landed.txt"), "work\n").unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-m", "landed after the check"]);
        let moved_head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&slot)
            .output()
            .unwrap();
        let moved_head = String::from_utf8_lossy(&moved_head.stdout).trim().to_string();

        let err = vcs::reset_worktree_to_ref(
            &bin,
            git.as_ref(),
            &slot,
            &guard.reset_ref,
            &guard.head,
            true,
        )
        .expect_err("a HEAD that moved since the check must refuse the reset");

        assert!(
            err.message.contains("HEAD changed since safety check"),
            "the refusal must name the actual cause, got: {}",
            err.message
        );
        assert_eq!(
            moved_head,
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&slot)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap(),
            "the commit that landed in the window must still be there"
        );
        assert!(
            slot.join("landed.txt").exists(),
            "a refused reset must not touch the worktree"
        );
    }

    #[test]
    fn acquisition_refuses_a_slot_holding_commits_the_branch_does_not_contain() {
        // The pool-level consequence of the same guard: `reset_for_acquisition`
        // is what `Pool::get` calls, and it must refuse rather than discard.
        let (_d, _repo, slot, git, _bin) = reset_fixture();
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&slot)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        std::fs::write(slot.join("unlanded.txt"), "work\n").unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-m", "unlanded"]);

        let err = reset_for_acquisition(
            &git,
            &slot,
            "main",
            &SeedInventory::default(),
            SlotOrigin::Recycled,
        )
        .expect_err("a slot ahead of its branch must not be reset");

        assert!(
            matches!(&err, PoolError::Git(e) if e.message.contains("commits that")),
            "got: {err}"
        );
        assert!(
            slot.join("unlanded.txt").exists(),
            "the unlanded work must survive a refused reset"
        );
    }

    #[test]
    fn a_recycled_slot_is_reset_under_the_clean_tree_the_reuse_rules_require() {
    // The no-race counterpart: when the checks pass, the guarded reset does
    // exactly what the plain reset did. The reuse rules in `acquire_locked`
    // already refuse a dirty slot, so a recycled slot arrives here clean and
    // this is the ordinary production shape.
    let (_d, _repo, slot, git, _bin) = reset_fixture();
    let head_before = head_of(&slot);

    reset_for_acquisition(
        &git,
        &slot,
        "main",
        &SeedInventory::default(),
        SlotOrigin::Recycled,
    )
    .unwrap();

    assert_eq!(
        head_of(&slot),
        head_before,
        "a safe reset lands the slot on exactly the commit it already held"
    );
    assert_eq!(
        std::fs::read_to_string(slot.join("README.md")).unwrap(),
        "hi\n",
        "tracked content is untouched by a reset of a clean tree"
    );
}

#[test]
fn a_fresh_slot_reset_restores_content_and_sweeps_untracked_files() {
    // `require_clean` is false for a FRESH slot, matching Go, which never
    // requires a clean tree on the creation path. That is the one case where
    // the reset actually has work to do — a checkout filter may have left an
    // untracked file — and where the pre-seam `reset_worktree` restored the
    // tracked tree and swept the rest.
    let (_d, _repo, slot, git, _bin) = reset_fixture();
    std::fs::write(slot.join("README.md"), "clobbered\n").unwrap();
    std::fs::write(slot.join("junk.txt"), "untracked\n").unwrap();

    reset_for_acquisition(
        &git,
        &slot,
        "main",
        &SeedInventory::default(),
        SlotOrigin::Fresh,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(slot.join("README.md")).unwrap(),
        "hi\n",
        "read-tree --reset -u must restore tracked content"
    );
    assert!(
        !slot.join("junk.txt").exists(),
        "clean -fd must still sweep untracked files"
    );
}

#[test]
fn a_recycled_slot_that_became_dirty_is_refused_not_overwritten() {
    // The other race the guard closes. `acquire_locked` proved the tree clean
    // before releasing the lock; something changed it afterwards. Wiping that
    // would discard the change while every check reported success.
    let (_d, _repo, slot, git, _bin) = reset_fixture();
    std::fs::write(slot.join("README.md"), "edited after the reuse check\n").unwrap();

    let err = reset_for_acquisition(
        &git,
        &slot,
        "main",
        &SeedInventory::default(),
        SlotOrigin::Recycled,
    )
    .expect_err("a tree that went dirty after the check must refuse the reset");

    assert!(
        matches!(&err, PoolError::Git(e) if e.message.contains("dirty after safety check")),
        "got: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(slot.join("README.md")).unwrap(),
        "edited after the reuse check\n",
        "a refused reset must not overwrite what it refused to trust"
    );
}

/// `git rev-parse HEAD` in `slot`.
fn head_of(slot: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(slot)
        .output()
        .unwrap();
    assert!(out.status.success(), "git rev-parse failed in {slot:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

    // ─── the seed-removal contract ───────────────────────────────────────────

    #[test]
    fn an_empty_inventory_authorizes_no_deletion() {
        // The safety property in isolation. An entry that lost its bookkeeping
        // must delete nothing — turning that loss into deletion would make a
        // bookkeeping bug look like a feature.
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("keep.env"), "KEEP\n").unwrap();

        let err = remove_seeded_paths(&slot, &[]).expect_err("an empty inventory must refuse");
        assert!(matches!(&err, PoolError::Git(e) if e.message.contains("seed inventory")));
        assert!(slot.join("keep.env").exists());
    }

    #[test]
    fn a_traversing_seed_path_is_refused_before_anything_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        let outside = dir.path().join("precious");
        std::fs::write(&outside, "DO NOT TOUCH\n").unwrap();
        std::fs::write(slot.join("a.env"), "a\n").unwrap();

        // The first entry is benign; the second escapes. Validation covers the
        // WHOLE list before the first delete, or a traversal in position two
        // would be preceded by an unvalidated removal in position one.
        let err = remove_seeded_paths(
            &slot,
            &["a.env".to_string(), "../precious".to_string()],
        )
        .expect_err("a traversing path must be refused");
        assert!(matches!(&err, PoolError::Git(e) if e.message.contains("invalid seeded path")));
        assert!(slot.join("a.env").exists(), "nothing may be deleted");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "DO NOT TOUCH\n"
        );
    }

    #[test]
    fn seed_removal_touches_only_the_listed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir_all(slot.join("nested")).unwrap();
        std::fs::write(slot.join("a.env"), "a\n").unwrap();
        std::fs::write(slot.join("b.env"), "b\n").unwrap();
        std::fs::write(slot.join("nested/c.env"), "c\n").unwrap();

        remove_seeded_paths(
            &slot,
            &["a.env".to_string(), "nested/c.env".to_string()],
        )
        .unwrap();

        assert!(!slot.join("a.env").exists());
        assert!(!slot.join("nested/c.env").exists());
        assert!(
            slot.join("b.env").exists(),
            "a path the inventory does not name must never be removed"
        );
        assert!(
            slot.join("nested").is_dir(),
            "the inventory names a file, not the directory holding it"
        );
    }

    #[test]
    fn a_seed_path_that_is_already_gone_is_not_a_failure() {
        // The inventory is permission to delete, not an obligation. A slot whose
        // seeds the user deleted by hand must still reset.
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        remove_seeded_paths(&slot, &["never-existed.env".to_string()]).unwrap();
    }

    /// An env whose ONLY override is the pool root — the shape the CLI builds
    /// for `--env-path` (cli.rs:246-315).
    struct PoolRootEnv(PathBuf);

    impl crate::env::TreehouseEnv for PoolRootEnv {
        fn pool_root(&self) -> Option<PathBuf> {
            Some(self.0.clone())
        }
        fn update_cache_path(&self) -> Option<PathBuf> {
            crate::env::DefaultEnv.update_cache_path()
        }
        fn user_config_path(&self) -> Option<PathBuf> {
            crate::env::DefaultEnv.user_config_path()
        }
        fn read_file(&self, path: &Path) -> std::io::Result<String> {
            crate::env::DefaultEnv.read_file(path)
        }
        fn read_bytes(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            crate::env::DefaultEnv.read_bytes(path)
        }
        fn write_file(&self, path: &Path, data: &[u8]) -> std::io::Result<()> {
            crate::env::DefaultEnv.write_file(path, data)
        }
        fn ensure_dir(&self, path: &Path) -> std::io::Result<()> {
            crate::env::DefaultEnv.ensure_dir(path)
        }
        fn path_exists(&self, path: &Path) -> bool {
            crate::env::DefaultEnv.path_exists(path)
        }
        fn list_dir(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
            crate::env::DefaultEnv.list_dir(path)
        }
        fn file_meta(&self, path: &Path) -> std::io::Result<crate::env::FileMeta> {
            crate::env::DefaultEnv.file_meta(path)
        }
        fn env_var(&self, name: &str) -> Option<String> {
            crate::env::DefaultEnv.env_var(name)
        }
        fn env_var_os(&self, name: &str) -> Option<PathBuf> {
            crate::env::DefaultEnv.env_var_os(name)
        }
        fn cwd(&self) -> Option<PathBuf> {
            crate::env::DefaultEnv.cwd()
        }
    }

    /// M-008: `--env-path` used to be a no-op — the pool landed in
    /// `~/.treehouse` because `open_with_env` called the non-`_with_env`
    /// resolver, which never consults the injected environment. The pool
    /// directory must land under the env's pool root instead.
    #[test]
    fn open_with_env_honors_the_injected_pool_root() {
        let (_dir, repo) = init_repo();
        let custom = tempfile::tempdir().unwrap();
        // No config root: the env is the only thing that can redirect the pool.
        let opts = OpenOptions::default();
        let pool = Pool::open_with_env(
            &repo,
            None,
            &opts,
            Arc::new(PoolRootEnv(custom.path().to_path_buf())),
        )
        .unwrap();
        assert!(
            pool.pool_dir().starts_with(custom.path()),
            "pool dir {:?} is not under the injected root {:?}",
            pool.pool_dir(),
            custom.path()
        );
        assert!(pool.pool_dir().exists());
    }

    /// M-008 (precedence half): `TREEHOUSE_ROOT` outranks `treehouse.toml`'s
    /// `root` — Go `config.ResolveRoot` (config.go:110-117). Read through the
    /// injected env, so this is observable without mutating the process
    /// environment that every other test in the binary shares.
    #[test]
    fn treehouse_root_env_var_outranks_the_config_root() {
        let (_dir, repo) = init_repo();
        let from_env = tempfile::tempdir().unwrap();
        let from_config = tempfile::tempdir().unwrap();
        let opts = OpenOptions {
            config: TreehouseConfig {
                root: Some(from_config.path().to_str().unwrap().to_string()),
                ..TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        let env = crate::env::InMemoryEnv::new(from_env.path().to_path_buf())
            .with_env(TREEHOUSE_ROOT_VAR, from_env.path().to_str().unwrap());
        let pool = Pool::open_with_env(&repo, None, &opts, Arc::new(env)).unwrap();
        assert!(
            pool.pool_dir().starts_with(from_env.path()),
            "TREEHOUSE_ROOT must win over config.root; got {:?}",
            pool.pool_dir()
        );
    }

    // ─── M-015b: next_free_name must probe the filesystem, not just state ────

    #[test]
    fn next_free_name_never_reissues_an_occupied_slot() {
        let dir = tempfile::tempdir().unwrap();
        // A worktree on disk whose state entry was lost: the crash-between-add-
        // and-write window. State is empty, so `next_name` alone says "1".
        std::fs::create_dir_all(dir.path().join("1").join("repo")).unwrap();
        assert_eq!(next_name(&State::default()), "1");
        assert_eq!(
            next_free_name(dir.path(), &State::default()),
            "2",
            "an occupied slot must never be reissued"
        );

        // State and disk together: the higher of the two wins.
        let state = State {
            worktrees: vec![WorktreeEntry {
                name: "7".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(next_free_name(dir.path(), &state), "8");
        std::fs::create_dir_all(dir.path().join("9")).unwrap();
        assert_eq!(next_free_name(dir.path(), &state), "10");
    }

    #[test]
    fn slot_backend_name_reports_damaged_and_healthy() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            slot_backend_name(dir.path()),
            "",
            "absent marker is damaged"
        );
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(slot_backend_name(dir.path()), "git");
        // A worktree's `.git` is a FILE; a primary checkout's is a directory.
        // Both are legitimate markers.
        let file_marker = tempfile::tempdir().unwrap();
        std::fs::write(file_marker.path().join(".git"), b"gitdir: /elsewhere\n").unwrap();
        assert_eq!(slot_backend_name(file_marker.path()), "git");
    }

    #[cfg(unix)]
    #[test]
    fn slot_backend_name_treats_a_dangling_marker_as_damaged() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join(".git")).unwrap();
        // Present on disk (lstat) but unresolvable (stat) — Go reports this as
        // a read failure the caller must surface, never as "no marker at all".
        assert_eq!(slot_backend_name(dir.path()), "");
    }

    #[test]
    fn slot_backend_name_does_not_admit_a_jj_workspace_yet() {
        // The seam answers "jj" for this path; the pool does not. The reuse
        // loop's checks are git calls, so admitting a non-git slot here would
        // run them in a tree git cannot read and report the failure in terms
        // that hide the cause. Documented as the one-line change the jj
        // backend makes once the pool carries flavor state.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        assert_eq!(vcs::worktree_backend_name(dir.path()).unwrap(), Some("jj"));
        assert_eq!(slot_backend_name(dir.path()), "");
    }

    #[test]
    fn slot_backend_name_agrees_with_the_seam_on_every_git_case() {
        // The marker rule is implemented once, in the seam. This pins the two
        // to the same answers so a future edit to one cannot drift from the
        // other: the pool's damage test and the dispatch the reset actually
        // goes through MUST agree, or a slot could be classified healthy here
        // and refused — or worse, the reverse — a few lines later.
        for case in ["absent", "git-dir", "git-file"] {
            let dir = tempfile::tempdir().unwrap();
            match case {
                "git-dir" => std::fs::create_dir(dir.path().join(".git")).unwrap(),
                "git-file" => std::fs::write(dir.path().join(".git"), b"gitdir: /x\n").unwrap(),
                _ => {}
            }
            let via_pool = slot_backend_name(dir.path());
            let via_seam = vcs::destructive_backend_for_worktree(dir.path())
                .map(|b| b.name().to_string())
                .unwrap_or_default();
            assert_eq!(via_pool, via_seam, "disagreement on {case}");
        }
    }

    // ─── M-021 / M-012 / M-013: the reuse loop's fail-closed guards ─────────

    /// Opens a pool over a real repo, and returns it alongside the guards that
    /// keep the tempdirs alive.
    fn pool_over(dir: &tempfile::TempDir, repo: &Path) -> Pool {
        let opts = OpenOptions {
            config: TreehouseConfig {
                root: Some(dir.path().to_str().unwrap().to_string()),
                ..TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        Pool::open(repo, None, &opts).unwrap()
    }

    /// M-021: a slot whose `.git` marker is gone must never be handed to
    /// `acquire`.
    ///
    /// The pool is laid out IN-PROJECT (`root = "."`, which nests `.treehouse`
    /// under the repo) because that is the configuration the hazard needs: a
    /// markerless slot then sits inside the user's own working tree, so every
    /// git dispatch on it resolves UP to that repository. The availability
    /// checks would vouch for the enclosing repo — which is clean and merged —
    /// and hand the slot out, with the reset aimed at the user's real tree.
    ///
    /// In an out-of-project pool the same test would pass for the wrong reason:
    /// there is no enclosing repository, so the clone-identity check alone
    /// would skip the slot. That is why this uses `root = "."`.
    #[test]
    fn acquire_never_reclaims_a_markerless_slot() {
        let (_dir, repo) = init_repo();
        // The in-project layout only works with the pool gitignored, and that
        // is also the configuration the hazard needs: once `.treehouse/` is
        // ignored the ENCLOSING repository reads as clean and merged, so the
        // availability checks vouch for it and only a marker check can save
        // the user's working tree. (Leave the pool untracked and the enclosing
        // repo is dirty, which masks the bug for the wrong reason.)
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        std::fs::write(repo.join(".gitignore"), b".treehouse/\n").unwrap();
        git(&["add", ".gitignore"]);
        git(&["commit", "-m", "ignore the in-project pool"]);

        let pool = Pool::open(
            &repo,
            None,
            &OpenOptions {
                config: TreehouseConfig {
                    root: Some(".".to_string()),
                    ..TreehouseConfig::default_config()
                },
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            pool.pool_dir().starts_with(&repo),
            "this test is only meaningful for an in-project pool"
        );

        let first = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                ..Default::default()
            })
            .unwrap();
        pool.release(&first.path.to_string_lossy()).unwrap();

        // Break the slot: the marker is gone but the directory and its
        // registered state entry remain.
        std::fs::remove_file(first.path.join(".git")).unwrap();
        assert_eq!(slot_backend_name(&first.path), "");

        // The slot must be passed over — and, because the enclosing repository
        // is the requester's own clone here, ONLY the marker check can pass it
        // over. Acquisition must still succeed, by growing a fresh slot.
        let second = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                ..Default::default()
            })
            .unwrap();
        assert_ne!(
            second.name, first.name,
            "a markerless slot must never be reclaimed"
        );
        // The user's own working tree is intact: no stray reset landed on it.
        assert!(
            repo.join("README.md").exists(),
            "the enclosing repository must be untouched"
        );
        // ...and the broken slot is reported as damaged rather than available.
        let status = pool.status().unwrap();
        let damaged = status
            .iter()
            .find(|s| s.name == first.name)
            .expect("the broken slot is still tracked");
        assert_eq!(damaged.status, STATUS_DAMAGED);
    }

    /// M-013: a clean slot holding commits that were never landed must not be
    /// reclaimed. A crashed owner leaves the tree clean (`status --porcelain`
    /// is empty) while its commits exist only there, so availability alone
    /// would let `reset --hard` + `clean -fd` discard them.
    #[test]
    fn acquire_never_reclaims_a_slot_holding_unlanded_commits() {
        let home = tempfile::tempdir().unwrap();
        let (repo_dir, repo) = init_repo();
        let pool = pool_over(&home, &repo);

        let first = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                ..Default::default()
            })
            .unwrap();
        pool.release(&first.path.to_string_lossy()).unwrap();

        // Simulate the crashed owner: commit in the slot, leave the tree clean,
        // and leave no reservation behind.
        let wt = &first.path;
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(wt)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::write(wt.join("unlanded.txt"), b"work nobody pushed\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "unlanded work"]);
        let dirty = pool.git_is_dirty(wt).unwrap();
        assert!(
            !dirty,
            "the tree is clean — only a merge check can catch this"
        );

        // The slot must be passed over and a new one created.
        let second = pool
            .get(&AcquireOptions {
                branch: Some("main".into()),
                ..Default::default()
            })
            .unwrap();
        assert_ne!(
            second.name, first.name,
            "a slot holding unlanded commits must never be reclaimed"
        );
        assert!(
            wt.join("unlanded.txt").exists(),
            "the unlanded commit must still be in the slot"
        );
        drop(repo_dir);
    }

    /// M-012: two clones of the same origin share a pool directory (it is
    /// keyed by the origin URL), but a linked worktree belongs to exactly ONE
    /// physical clone. Clone B must not be handed clone A's slot.
    #[test]
    fn acquire_never_reclaims_another_clones_slot() {
        let home = tempfile::tempdir().unwrap();
        // Two clones of one origin, both named `shared` so the pool name
        // (`{repoName}-{hash(remote)}`) collides exactly as it does for two
        // checkouts of the same GitHub repository.
        let base = tempfile::tempdir().unwrap();
        let clone_a = base.path().join("a").join("shared");
        let clone_b = base.path().join("b").join("shared");
        std::fs::create_dir_all(clone_a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(clone_b.parent().unwrap()).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", clone_a.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(init.status.success(), "git init failed");
        let run_git_in = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git_in(&clone_a, &["config", "user.email", "t@t.com"]);
        run_git_in(&clone_a, &["config", "user.name", "T"]);
        std::fs::write(clone_a.join("README.md"), b"hi\n").unwrap();
        run_git_in(&clone_a, &["add", "."]);
        run_git_in(&clone_a, &["commit", "-m", "init"]);
        let cloned = std::process::Command::new("git")
            .args([
                "clone",
                clone_a.to_str().unwrap(),
                clone_b.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(cloned.status.success(), "git clone failed");
        run_git_in(&clone_b, &["config", "user.email", "t@t.com"]);
        run_git_in(&clone_b, &["config", "user.name", "T"]);

        // Same remote URL and same pool root => the same pool directory.
        let remote = "https://example.com/acme/shared.git";
        let opts = OpenOptions {
            config: TreehouseConfig {
                root: Some(home.path().to_str().unwrap().to_string()),
                max_trees: 1,
                ..TreehouseConfig::default_config()
            },
            ..Default::default()
        };
        let pool_a = Pool::open(&clone_a, Some(remote), &opts).unwrap();
        let pool_b = Pool::open(&clone_b, Some(remote), &opts).unwrap();
        assert_eq!(
            pool_a.pool_dir(),
            pool_b.pool_dir(),
            "the two clones must share one pool directory for this test to mean anything"
        );

        // Clone A fills the pool and leaves the slot idle and clean.
        let acquired = pool_a
            .get(&AcquireOptions {
                branch: Some("main".into()),
                ..Default::default()
            })
            .unwrap();
        pool_a.release(&acquired.path.to_string_lossy()).unwrap();
        // The slot's gitdir must point into clone A's object store — that is
        // what makes it "another clone's" rather than merely "unverifiable",
        // which is the distinction the test below asserts.
        let gitdir = std::fs::read_to_string(acquired.path.join(".git")).unwrap();
        assert!(
            gitdir.contains(clone_a.to_str().unwrap()),
            "clone A must own the slot, got gitdir: {gitdir}"
        );

        // Clone B, with the pool at max_trees: the only way forward would be to
        // reset clone A's slot. It must refuse instead, and say why.
        match pool_b.get(&AcquireOptions {
            branch: Some("main".into()),
            ..Default::default()
        }) {
            Err(PoolError::PoolFullNotOurs {
                other_clone, count, ..
            }) => {
                assert_eq!(other_clone, 1, "the foreign slot must be counted");
                assert_eq!(count, 1);
            }
            Err(other) => panic!("expected the foreign-slot pool-full error, got: {other}"),
            Ok(acquired) => panic!(
                "clone B was handed clone A's slot at {}",
                acquired.path.display()
            ),
        }
    }

    #[test]
    fn pool_full_message_names_the_skip_reason() {
        // The plain message stays byte-for-byte for the ordinary case: scripts
        // match on it, and a pool of genuinely in-use worktrees adds nothing.
        assert_eq!(
            SkipCounts::default().into_pool_full(2, 2).to_string(),
            "all 2 worktrees are in use or dirty (max_trees = 2). \
             Run 'treehouse status' to see details, or increase max_trees in treehouse.toml"
        );
        let msg = SkipCounts {
            other_clone: 2,
            unverified_clone: 1,
        }
        .into_pool_full(3, 3)
        .to_string();
        assert!(msg.contains("2 belong to another clone"), "got: {msg}");
        assert!(
            msg.contains("1 whose clone identity cannot be verified"),
            "got: {msg}"
        );
        assert!(
            msg.contains("reused only by the clone it belongs to"),
            "got: {msg}"
        );
    }

    // ─── M-024: the acquisition flags, proven by OUTCOME ─────────────────────

    /// Builds a real repo with one commit on `main`, plus a pool over it under
    /// a temp root. Returns the temp dirs so they outlive the pool.
    fn repo_and_pool() -> (tempfile::TempDir, tempfile::TempDir, Pool) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let out = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .expect("git must be installed");
        assert!(
            out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let git_in_repo = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git must be installed");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git_in_repo(&["config", "user.email", "t@t.com"]);
        git_in_repo(&["config", "user.name", "T"]);
        std::fs::write(repo.join("README.md"), b"hi\n").unwrap();
        git_in_repo(&["add", "."]);
        git_in_repo(&["commit", "-m", "init"]);

        let home = tempfile::tempdir().unwrap();
        let pool = Pool::open(
            &repo,
            None,
            &OpenOptions {
                config: TreehouseConfig {
                    root: Some(home.path().to_str().unwrap().to_string()),
                    ..TreehouseConfig::default_config()
                },
                ..Default::default()
            },
        )
        .unwrap();
        (dir, home, pool)
    }

    /// `--branch` must CHANGE THE OUTCOME: the acquired worktree ends up ON the
    /// named branch. Before the flag existed there was no way to create one, so
    /// this asserts a real ref exists at the acquired commit — it fails if the
    /// create+checkout call is removed.
    #[test]
    fn new_branch_creates_and_checks_out_the_named_branch() {
        let (_dir, _home, pool) = repo_and_pool();
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                new_branch: Some("feature/from-flags".to_string()),
                ..Default::default()
            })
            .expect("acquire with --branch must succeed");

        // HEAD in the worktree is ON the new branch...
        let head = std::process::Command::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(&acquired.path)
            .output()
            .expect("git must be installed");
        assert!(head.status.success());
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "feature/from-flags",
            "the acquired worktree must be checked out on the requested branch"
        );

        // ...and the ref really exists in the repository, at the same commit
        // the worktree sits on.
        let listed = std::process::Command::new("git")
            .args(["branch", "--list", "feature/from-flags"])
            .current_dir(pool.root.as_path())
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&listed.stdout).contains("feature/from-flags"),
            "the branch must exist in the repository; got: {}",
            String::from_utf8_lossy(&listed.stdout)
        );
    }

    /// `--branch` must refuse to adopt an existing ref rather than silently
    /// checking out someone else's branch.
    #[test]
    fn new_branch_refuses_a_branch_that_already_exists() {
        let (_dir, _home, pool) = repo_and_pool();
        let err = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                new_branch: Some("main".to_string()),
                ..Default::default()
            })
            .expect_err("--branch must refuse an existing branch");
        assert!(
            err.to_string().contains("already exists"),
            "got: {err}"
        );
    }

    /// A branch name git would refuse must be rejected BEFORE anything is
    /// created — `-b ../evil` would otherwise become a ref outside the repo.
    #[test]
    fn invalid_branch_names_are_refused_before_acquiring() {
        for bad in ["", "../evil", "-x", "a..b", "a b", "a~1", "x.lock", ".hidden"] {
            let err = validate_branch_name(bad);
            assert!(err.is_err(), "branch name {bad:?} must be refused");
        }
        assert!(validate_branch_name("feature/ok-1").is_ok());
    }

    /// `--unique-leaf` must CHANGE THE OUTCOME: the directory is named
    /// `<repo>-<slot>` instead of `<repo>`. Without it the flag would parse and
    /// do nothing, which is the failure this whole pass exists to kill.
    #[test]
    fn unique_leaf_names_the_directory_repo_dash_slot() {
        let (_dir, _home, pool) = repo_and_pool();
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                unique_leaf: true,
                ..Default::default()
            })
            .expect("acquire with --unique-leaf must succeed");

        let leaf = acquired
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            leaf, "repo-1",
            "--unique-leaf must name the directory <repo>-<slot>"
        );
        assert!(
            acquired.path.exists(),
            "the worktree must actually have been created at that path"
        );
    }

    /// Without the flag the built-in layout must be unchanged — `--unique-leaf`
    /// is opt-in and must not silently become the default.
    #[test]
    fn the_default_layout_is_unchanged_without_the_flags() {
        let (_dir, _home, pool) = repo_and_pool();
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .expect("acquire must succeed");
        assert_eq!(
            acquired.path.file_name().unwrap().to_string_lossy(),
            "repo",
            "the built-in <pool>/<slot>/<repo> layout must be preserved"
        );
    }

    /// `--worktree-path` must CHANGE THE OUTCOME: the worktree is created at
    /// the templated path, not the built-in one.
    #[test]
    fn worktree_path_template_places_the_worktree_where_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .unwrap();
        for args in [
            vec!["config", "user.email", "t@t.com"],
            vec!["config", "user.name", "T"],
            vec!["add", "."],
            vec!["commit", "-m", "init", "--allow-empty"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .output()
                .unwrap();
        }
        let pool = Pool::open(
            &repo,
            None,
            &OpenOptions {
                config: TreehouseConfig {
                    root: Some(home.path().to_str().unwrap().to_string()),
                    ..TreehouseConfig::default_config()
                },
                ..Default::default()
            },
        )
        .unwrap();

        // Place the slot OUTSIDE the pool, under a dir only the template names.
        let custom = dir.path().join("elsewhere");
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                worktree_path: Some(format!("{}/{{repo}}/{{slot}}", custom.display())),
                ..Default::default()
            })
            .expect("acquire with --worktree-path must succeed");

        assert_eq!(
            acquired.path,
            custom.join("repo").join("1"),
            "--worktree-path must place the worktree where the template names"
        );
        assert!(
            acquired.path.exists(),
            "the worktree must exist at the templated path"
        );
    }

    /// A template that is wrong on its own text must fail on EVERY invocation —
    /// including one that recycles an existing slot and never expands it.
    /// Otherwise a typo silently hands back a slot at the built-in layout and
    /// never reports itself (Go `validateWorktreePathTemplate`).
    #[test]
    fn a_bad_template_is_refused_before_anything_is_created() {
        for bad in [
            "{pool}/nope",              // no {slot}: every slot collides
            "{repo}/{slot}/../x",       // '..' cancels {slot}
            "{bogus}/{slot}",           // unknown placeholder
            "{repo_parent}/{slot}",     // not repository-scoped
        ] {
            let err = validate_worktree_path_template(bad);
            assert!(err.is_err(), "template {bad:?} must be refused");
        }
        assert!(validate_worktree_path_template("{pool}/{slot}/{repo}").is_ok());
        assert!(validate_worktree_path_template("").is_ok());
    }

    /// `resolve_worktree_path` must expand each placeholder to its own meaning,
    /// and a relative result must be refused rather than created.
    #[test]
    fn worktree_path_expands_every_placeholder() {
        let repo = Path::new("/src/myrepo");
        let pool_dir = Path::new("/pool");
        assert_eq!(
            resolve_worktree_path(repo, pool_dir, "3", "", false).unwrap(),
            PathBuf::from("/pool/3/myrepo"),
            "the built-in layout must be unchanged"
        );
        assert_eq!(
            resolve_worktree_path(repo, pool_dir, "3", "", true).unwrap(),
            PathBuf::from("/pool/3/myrepo-3"),
            "--unique-leaf appends the slot"
        );
        assert_eq!(
            resolve_worktree_path(repo, pool_dir, "3", "{pool}/{slot}/{repo}", false).unwrap(),
            PathBuf::from("/pool/3/myrepo")
        );
        // `{repo_parent}` IS a valid placeholder to expand, but on its own it is not
// enough: two sibling repositories expand it identically, so it cannot tell
// their slots apart. It only works alongside a repository-scoped placeholder.
        assert_eq!(
            resolve_worktree_path(
                repo,
                pool_dir,
                "3",
                "{repo_parent}/{repo}/{slot}",
                false
            )
            .unwrap(),
            PathBuf::from("/src/myrepo/3")
        );
        // Relative templates are refused: a worktree must land where the caller
        // named, not wherever the process happens to be running.
        assert!(
            validate_worktree_path_template("relative/{slot}/{repo}").is_ok(),
            "validation is lexical"
        );
    }

    /// `--worktree-path` and `--unique-leaf` together: the template supersedes
    /// unique_leaf, because it names every segment including the leaf. Go says
    /// this out loud rather than resolving silently (pool.go:391-394).
    #[test]
    fn a_template_supersedes_unique_leaf() {
        let repo = Path::new("/src/myrepo");
        let pool_dir = Path::new("/pool");
        assert_eq!(
            resolve_worktree_path(repo, pool_dir, "2", "{pool}/{slot}/{repo}-x", true).unwrap(),
            PathBuf::from("/pool/2/myrepo-x"),
            "the template names the leaf, so unique_leaf adds nothing"
        );
    }
}
