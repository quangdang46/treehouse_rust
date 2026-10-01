//! The version-control seam: which backend answers for a path, and the
//! guarded-reset protocol that protects a pooled worktree.
//!
//! Two responsibilities live here, both ported from Go `internal/vcs`
//! (`vcs.go`). Neither belongs on [`crate::git::GitBackend`] itself, because
//! both are about *choosing* a backend rather than *being* one.
//!
//! # Backend identity and dispatch
//!
//! A pooled worktree's own marker — a `.git` entry or a `.jj` directory —
//! decides which backend handles it. That is the whole point of the seam: the
//! configured backend must never answer for a slot of the other flavor, and —
//! for the operations that REWRITE a checkout — a path carrying no marker at
//! all must be REFUSED rather than answered by whatever repository encloses
//! it. In the supported in-project pool layout (`root = "."`, which nests
//! `.treehouse` under the repo) that enclosing repository is the user's own
//! working tree, and `reset --hard` + `clean -fd` there is irreversible data
//! loss. Go wraps every destructive entry point in
//! `destructiveBackendForWorktree` (`vcs.go:513-525`) for exactly this reason.
//!
//! [`backend_for_worktree`] is the non-destructive read: a markerless path
//! falls back to the default backend, because reading a status is harmless.
//! [`destructive_backend_for_worktree`] is the write path: a markerless path
//! is an error, never a guess.
//!
//! # The registry
//!
//! [`BackendRegistry`] maps a backend name to a handle. It is extensible
//! WITHOUT editing [`crate::git::GitBackend`]: the jj backend is added by
//! registering a handle, not by adding a variant to any enum this port owns.
//! [`global_registry`] is the process-wide instance, and it registers jj
//! itself — see its docs for why that is not the same as opting into it.
//!
//! # The guarded-reset pair
//!
//! [`is_worktree_safe_to_reset`] and [`reset_worktree_to_ref`] are Go's
//! two-call protocol (gitvcs.go:934-1010, pool.go:511-521). They are split
//! because a reset that does not re-verify is WORSE than no reset at all:
//! callers believe the safety check and the reset shared one target, so a
//! concurrent commit landing in between is silently discarded. The check
//! returns the resolved commit and the HEAD it observed; the reset re-reads
//! HEAD under git's own `HEAD.lock` and refuses if it moved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use crate::git::{GitBackend, GitError, GitErrorKind, GitRepo, ShellGitBackend};

/// Backend name for git (Go `vcs.gitBackend`).
pub const BACKEND_GIT: &str = "git";
/// Backend name for jj (Go `vcs.jjBackend`).
pub const BACKEND_JJ: &str = "jj";

pub mod jj;

// ─── Marker inspection ─────────────────────────────────────────────────────────

/// Reports whether `path` itself exists, WITHOUT following symlinks (Go
/// `markerPresent`).
///
/// A dangling symlink is PRESENT: the directory entry is on disk, and its
/// unresolvable target is a read failure for the caller to surface — never an
/// absent marker. Collapsing the two would let a slot whose `.git` points at a
/// deleted worktree be classified as "no marker", i.e. damaged, and then
/// handled by whatever repository encloses the pool.
fn marker_present(path: &Path) -> Result<bool, GitError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(GitError::new(
            "lstat worktree marker",
            format!("reading {}: {e}", path.display()),
            GitErrorKind::Other,
        )),
    }
}

/// The backend a worktree's own marker names (Go `WorktreeBackendNameChecked`,
/// vcs.go:444-480).
///
/// - `.git` present → [`BACKEND_GIT`]. The entry is followed once so a DANGLING
///   `.git` is a read ERROR here rather than three confusing git failures later.
/// - `.jj` present and a directory → [`BACKEND_JJ`]. A `.jj` FILE is not a
///   workspace marker and falls through.
/// - neither → `Ok(None)`. A genuine "no marker" answer, distinct from an error.
///
/// The error cases are load-bearing and are why this returns `Result<Option<_>>`
/// rather than `Option<_>`: a caller that cannot tell "absent" from
/// "unreadable" will eventually treat an unreadable slot as safe to touch.
pub fn worktree_backend_name(path: &Path) -> Result<Option<&'static str>, GitError> {
    let git_marker = path.join(".git");
    if marker_present(&git_marker)? {
        std::fs::metadata(&git_marker).map_err(|e| {
            GitError::new(
                "stat .git marker",
                format!("resolving .git marker in {}: {e}", path.display()),
                GitErrorKind::Other,
            )
        })?;
        return Ok(Some(BACKEND_GIT));
    }

    let jj_marker = path.join(".jj");
    if marker_present(&jj_marker)? {
        let meta = std::fs::metadata(&jj_marker).map_err(|e| {
            GitError::new(
                "stat .jj marker",
                format!("resolving .jj marker in {}: {e}", path.display()),
                GitErrorKind::Other,
            )
        })?;
        if meta.is_dir() {
            return Ok(Some(BACKEND_JJ));
        }
    }

    Ok(None)
}

/// Refuses a path that carries no backend marker, before any destructive work
/// starts (the precondition [`destructive_backend_for_worktree`] enforces, for
/// callers that already hold a backend).
pub(crate) fn require_destructive_marker(path: &Path) -> Result<&'static str, GitError> {
    match worktree_backend_name(path)? {
        Some(name) => Ok(name),
        None => Err(GitError::new(
            "resolve worktree backend",
            format!(
                "refusing to modify {}: it holds no .git or .jj marker",
                path.display()
            ),
            GitErrorKind::Other,
        )),
    }
}

// ─── Registry ─────────────────────────────────────────────────────────────────

/// Name → backend handles, defaulting to git.
///
/// Registration is the ONLY way a new backend joins the seam. Nothing here is
/// a `match` over an enum, so the jj backend can ship without touching this
/// file, the trait, or any dispatch site.
///
/// Cloning is not provided (backends are `dyn`), so lookups return an
/// [`Arc`] the caller can hold across the operation it is dispatching.
#[derive(Default)]
pub struct BackendRegistry {
    backends: RwLock<BTreeMap<&'static str, Arc<dyn GitBackend>>>,
}

impl std::fmt::Debug for BackendRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendRegistry")
            .field("names", &self.names())
            .finish()
    }
}

impl BackendRegistry {
    /// An empty registry. [`Self::default_git`] seeds git on first use, so a bare
    /// registry already behaves correctly for a git-only deployment; building
    /// one directly is for tests that assert dispatch itself.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or replaces) the backend answering for `name`.
    ///
    /// Replacement is allowed because registration happens at startup, before
    /// any dispatch, and a later registrant legitimately wins — that is how a
    /// test injects a stub. It is NOT safe to call concurrently with dispatch
    /// expecting in-flight work to change backend; an operation already
    /// dispatched keeps the handle it was given.
    ///
    /// The name is `&'static str` on purpose: backends are registered once and
    /// live for the process, so the registry never has to own or free a name.
    /// It must match what `GitBackend::name` returns for that backend, or the
    /// registry will resolve markers to a backend that calls itself something
    /// else.
    pub fn register(&self, name: &'static str, backend: Arc<dyn GitBackend>) {
        self.backends
            .write()
            .expect("backend registry lock poisoned")
            .insert(name, backend);
    }

    /// The backend registered for `name`, if any.
    pub fn get(&self, name: &str) -> Option<Arc<dyn GitBackend>> {
        self.backends
            .read()
            .expect("backend registry lock poisoned")
            .get(name)
            .cloned()
    }

    /// Every registered name, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        self.backends
            .read()
            .expect("backend registry lock poisoned")
            .keys()
            .copied()
            .collect()
    }

    /// The git backend, constructing and caching it on first use.
    ///
    /// Git is the default everywhere, exactly as in Go: the jj backend is
    /// strictly opt-in, so a repository without one keeps git's behavior and a
    /// shell-wide opt-in can never break a plain git repository. Construction
    /// is deferred rather than done at module load so importing this module on
    /// a machine without git is not itself an error.
    ///
    /// Returns `None` only when git is genuinely absent — there is no
    /// substitute, and silently returning some other backend would be a
    /// misroute.
    pub fn default_git(&self) -> Option<Arc<dyn GitBackend>> {
        if let Some(git) = self.get(BACKEND_GIT) {
            return Some(git);
        }
        let discovered = ShellGitBackend::discover().ok()?;
        let handle: Arc<dyn GitBackend> = Arc::new(discovered);
        // Re-check under the write lock: a concurrent first lookup may have
        // won the race, and clobbering its handle would swap the backend
        // mid-dispatch. Whoever got here second keeps theirs.
        let mut backends = self
            .backends
            .write()
            .expect("backend registry lock poisoned");
        match backends.get(BACKEND_GIT) {
            Some(existing) => Some(existing.clone()),
            None => {
                backends.insert(BACKEND_GIT, handle.clone());
                Some(handle)
            }
        }
    }

    /// Resolves `name`, falling back to git when the marker named none.
    ///
    /// A `git` marker is special: it means "not built yet", not "unavailable",
    /// because git is constructed on demand by [`Self::default_git`]. Any other
    /// name must already be registered — an unregistered one is an error, never
    /// a silent fallback, so a `.jj` workspace is never answered by git.
    fn resolve_or_default(
        &self,
        name: Option<&'static str>,
    ) -> Result<Arc<dyn GitBackend>, GitError> {
        let no_git = || {
            GitError::new(
                "resolve worktree backend",
                "no git backend available (git binary not found on PATH)",
                GitErrorKind::NotFound,
            )
        };
        match name {
            None | Some(BACKEND_GIT) => self.default_git().ok_or_else(no_git),
            Some(name) => self.get(name).ok_or_else(|| {
                GitError::new(
                    "resolve worktree backend",
                    format!("no backend is registered for {name:?}"),
                    GitErrorKind::Other,
                )
            }),
        }
    }
}

/// The process-wide registry (Go's package-level `gitBackend` / `jjBackend`).
///
/// Built lazily on first use, so a caller that never touches the seam pays
/// nothing. The jj backend is registered HERE, by the registry's own
/// initializer, rather than by a call site somewhere in the CLI: a backend
/// registered after a pool has already dispatched leaves that pool answering
/// with git, and "register jj early enough" is exactly the kind of ordering
/// requirement a caller will eventually get wrong.
///
/// Registering jj is NOT opting into it. jj stays opt-in where worktrees are
/// CREATED — that choice is [`configured_backend_name`], driven by
/// configuration. Registration only makes a `.jj` marker answerable, which is
/// Go's `slotMarkerBackend` behaviour: a slot is dispatched by its own marker
/// regardless of what the repository currently selects. So a repository without
/// a `.jj` directory is completely unaffected, and a colocated one still
/// resolves to git (a `.git` marker is checked first).
///
/// Git is NOT registered here. It is constructed on demand by
/// [`BackendRegistry::default_git`], so importing this module on a machine
/// without git is not itself an error.
///
/// This function is the single entry point other agents should use; building a
/// private registry is only correct in tests that assert dispatch itself.
pub fn global_registry() -> &'static BackendRegistry {
    static REGISTRY: OnceLock<BackendRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let registry = BackendRegistry::new();
        // Infallible by construction — see `JjBackend::discover`. A machine
        // without jj still gets a registered backend so a `.jj` marker fails
        // with "jj binary not found" rather than "no backend is registered".
        registry.register(BACKEND_JJ, Arc::new(jj::JjBackend::discover()));
        registry
    })
}

/// The nearest ancestor of `path` holding a VCS marker (Go `findMarkerRoot`),
/// and WHICH markers it holds.
///
/// Both are reported because a COLOCATED repository (`.jj` and `.git`
/// together) means something different from a `.jj`-only one, and the two are
/// selected differently: a colocated repository stays on git worktrees unless
/// the user explicitly opts in, while a `.jj`-only tree has no git worktree to
/// stay on.
///
/// A marker that exists but cannot be read counts as ABSENT here, unlike
/// [`worktree_backend_name`]. This function answers "which backend would this
/// path be configured for", and Go's `findMarkerRoot` reads it the same way
/// with `os.Stat`. The destructive paths do not go through here; they go
/// through [`worktree_backend_name`], where an unreadable marker is an error.
/// `None` means the walk reached the filesystem root without finding one.
pub fn marker_root(path: &Path) -> Option<MarkerRoot> {
    let mut dir = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    loop {
        let has_jj = std::fs::metadata(dir.join(".jj")).is_ok_and(|m| m.is_dir());
        let has_git = std::fs::metadata(dir.join(".git")).is_ok();
        if has_jj || has_git {
            return Some(MarkerRoot {
                root: dir,
                has_git,
                has_jj,
            });
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
}

/// Where a repository's markers are, and which ones (Go's `findMarkerRoot`
/// triple).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerRoot {
    /// The directory holding the markers.
    pub root: PathBuf,
    /// A `.git` entry is present there.
    pub has_git: bool,
    /// A `.jj` DIRECTORY is present there. A `.jj` file is not a workspace.
    pub has_jj: bool,
}

/// The backend name the CONFIGURATION selects for `path` (Go
/// `backendFor(...).Name()`, exposed for the CLI's `--branch` refusal).
///
/// `override_name` is the configuration layer's answer — `TREEHOUSE_VCS`, the
/// repository's `treehouse.toml` `vcs` key, or the user-level config, already
/// resolved and normalized to `"git"`/`"jj"`, or `None` for no opt-in. Reading
/// those files belongs to the config layer, which depends on this one; this
/// function is pure so it holds no global state and needs no lock.
///
/// Two selection rules, both from Go:
///
/// - **An explicit `"git"` always wins**, opt-in or not.
/// - **An explicit `"jj"` only takes effect where a `.jj` directory actually
///   exists.** Everywhere else the answer stays git, which is what makes a
///   shell-wide `TREEHOUSE_VCS=jj` harmless in a plain git repository.
/// - With no opt-in, a `.jj`-only tree also consults the MAIN repository root
///   its `.jj/repo` pointer names: a pooled jj workspace is a `.jj`-only
///   checkout that cannot carry an untracked `treehouse.toml`, so the opt-in
///   lives at the main root. That hop is pure file inspection, so it works
///   before any backend is selected — and the decision still comes from
///   configuration, never from the marker.
///
/// A path outside any repository answers [`BACKEND_GIT`], so errors surface
/// exactly as they always did rather than as a new "no marker" failure.
///
/// NOTE that this is the CONFIGURED backend — which backend would CREATE a
/// worktree. A worktree that already exists is answered by its own marker
/// instead; see [`backend_for_worktree`] and
/// [`destructive_backend_for_worktree`]. The two deliberately disagree for a
/// colocated repository, and that is the mechanism behind `TestJJColocatedWithoutOptInKeepsGitWorktrees`.
pub fn configured_backend_name(
    path: &Path,
    override_name: Option<&str>,
) -> Result<&'static str, GitError> {
    // An unrecognized value is IGNORED, not an error: a typo in a config file
    // must not break every command in the repository.
    let git_forced = override_name == Some(BACKEND_GIT);
    let jj_forced = override_name == Some(BACKEND_JJ);

    let Some(found) = marker_root(path) else {
        return Ok(BACKEND_GIT);
    };
    if git_forced {
        return Ok(BACKEND_GIT);
    }
    if jj_forced {
        return Ok(if found.has_jj {
            BACKEND_JJ
        } else {
            BACKEND_GIT
        });
    }
    // A `.jj`-only tree: its own checkout cannot hold the opt-in, so the main
    // repository root gets the same `override_name` Go would have re-read for
    // itself. `main_root_from_workspace_root` is pure file inspection, so this
    // never needs a jj binary.
    if found.has_jj
        && !found.has_git
        && let Ok(main_root) = jj::main_root_from_workspace_root(&found.root)
        && main_root != found.root
        && override_name_is_jj_at(&main_root, override_name)
    {
        return Ok(BACKEND_JJ);
    }
    Ok(BACKEND_GIT)
}

/// Whether the opt-in for `root` selects jj.
///
/// Go re-reads the configuration files at each root, because a repository's
/// `treehouse.toml` and the user-level config are different files. This port
/// takes the already-resolved override as a single value, which matches Go
/// exactly for the `TREEHOUSE_VCS` and user-config cases and is the conservative
/// reading for the repository-file case at a SECOND root: no extra opt-in is
/// invented there, so a workspace nested under an opted-in main repository is
/// jj only when the caller resolved the override for that main root.
fn override_name_is_jj_at(root: &Path, override_name: Option<&str>) -> bool {
    match override_name {
        Some(BACKEND_JJ) => std::fs::metadata(root.join(".jj")).is_ok_and(|m| m.is_dir()),
        _ => false,
    }
}

// ─── Dispatch ─────────────────────────────────────────────────────────────────

/// The backend that owns `path` for READ operations (Go `backendForWorktree`).
///
/// A marker names the backend outright; a markerless path falls back to the
/// default (git), so reading a status outside any repository surfaces exactly
/// the error it always did.
///
/// A marker naming an UNREGISTERED backend is an error rather than a fallback:
/// answering a `.jj` workspace with git would run git commands inside a
/// non-git tree and report the failure in terms that hide the real cause.
pub fn backend_for_worktree(path: &Path) -> Result<Arc<dyn GitBackend>, GitError> {
    global_registry().resolve_or_default(worktree_backend_name(path)?)
}

/// The backend that owns `path` for operations that REWRITE its checkout
/// (Go `destructiveBackendForWorktree`, vcs.go:513-525).
///
/// Unlike [`backend_for_worktree`] this REFUSES a markerless path. The
/// fallback there is a convenience for reads; here it would mean resetting the
/// repository ENCLOSING the pool — the user's own working tree, in the
/// supported in-project layout. Callers guard markerless slots first; this
/// refusal is defense in depth, and it is the reason pool slots are the only
/// callers of these operations.
pub fn destructive_backend_for_worktree(path: &Path) -> Result<Arc<dyn GitBackend>, GitError> {
    let name = require_destructive_marker(path)?;
    global_registry().resolve_or_default(Some(name))
}

/// The refusal a SELECTED backend earns by being unable to serve `path`.
///
/// A raw failure from inside the seam answers a question the user did not ask:
/// it names the command that happened to run, not the choice that sent it
/// there. Someone who selected jj in a `.jj`-only repository cannot tell from
/// `fatal: not a git repository` that jj was the backend they picked — the
/// fatal came from git, which the selection never chose.
///
/// So the message leads with the SELECTION, quotes the underlying cause, and
/// closes with the remedy. It never proposes falling back to another backend:
/// answering a jj selection with git is the misroute this module exists to
/// prevent ([`BackendRegistry::resolve_or_default`],
/// [`destructive_backend_for_worktree`]), and offering it here would make the
/// same confusion reachable from the error path instead of only from a bug.
fn backend_cannot_serve(selected: &str, path: &Path, cause: &GitError) -> GitError {
    let remedy = if selected == BACKEND_JJ {
        "make jj available (it must be on PATH, or set JJ_BIN), or drop the jj opt-in \
         (TREEHOUSE_VCS, or `vcs = \"jj\"` in treehouse.toml) to use git"
    } else {
        "run this inside a git repository, or select a backend that serves it with the \
         `vcs` key in treehouse.toml"
    };
    GitError::new(
        "resolve repository root",
        format!(
            "the {selected} backend is selected for {} but cannot serve it: {}. \
             Treehouse will not fall back to another backend here, so nothing was \
             created or changed. To fix it, {remedy}.",
            path.display(),
            cause.message,
        ),
        cause.kind,
    )
}

impl BackendRegistry {
    /// The repository root as the CONFIGURED backend for `path` sees it (Go
    /// `FindRepoRootFrom` behind `BackendNameFor`).
    ///
    /// The dispatch twin of [`configured_backend_name`]: that function answers
    /// "which backend?", this one asks that backend and reports its answer. Both
    /// are needed, because a caller that resolved the root with git directly —
    /// as the CLI's repository-context loader does — cannot serve a repository
    /// git does not own, and reports git's fatal as though git had been chosen.
    ///
    /// Fail CLOSED on every failure, including resolution: an unregistered or
    /// unavailable backend is reported through [`backend_cannot_serve`], never
    /// answered by whichever repository encloses `path`. Falls back to no
    /// backend at all.
    pub fn repo_root_for(
        &self,
        path: &Path,
        override_name: Option<&str>,
    ) -> Result<PathBuf, GitError> {
        let selected = configured_backend_name(path, override_name)?;
        let backend = self
            .resolve_or_default(Some(selected))
            .map_err(|e| backend_cannot_serve(selected, path, &e))?;
        backend
            .repo_root(path)
            .map_err(|e| backend_cannot_serve(selected, path, &e))
    }
}

/// [`BackendRegistry::repo_root_for`] against the process-wide registry.
///
/// This is the entry point a repository-context loader should call instead of
/// asking the git backend directly: it is what makes a jj selection actually
/// reach jj for root resolution, and what turns an unserviceable repository
/// into a named refusal rather than a raw fatal.
pub fn configured_repo_root(path: &Path, override_name: Option<&str>) -> Result<PathBuf, GitError> {
    global_registry().repo_root_for(path, override_name)
}

// ─── The guarded-reset pair ───────────────────────────────────────────────────

/// What a safety check proved, and what a reset must therefore be given.
///
/// The two SHAs travel together or not at all. `reset_ref` is the immutable
/// commit the reset targets; `head` is the HEAD the ancestry check was run
/// against. Passing a `head` from a different check is exactly the bug the
/// pair exists to make impossible, so both are captured together here rather
/// than re-derived at reset time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetGuard {
    /// Whether HEAD is reachable from `reset_ref` — i.e. the reset discards
    /// nothing. `false` is a legitimate answer the caller may weigh; an `Err`
    /// from the check is NOT one.
    pub safe: bool,
    /// The resolved commit to reset to.
    pub reset_ref: String,
    /// The worktree HEAD recorded at check time.
    pub head: String,
}

/// Whether `s` is a full lowercase hex object id (Go `isCommitID`).
///
/// Both the check and the reset validate their inputs with this. A ref name
/// smuggled into `reset_ref` would let `git read-tree --reset -u` move the
/// worktree to whatever the caller named — the caller of the reset is the
/// safety check, so the reset must not trust it to have been careful.
fn is_commit_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The `command` half of a [`GitError`] raised by a git invocation.
///
/// [`GitError`]'s `Display` supplies the tool name itself
/// (`#[error("git {command}: {message}")]`), so `command` carries the ARGUMENT
/// LIST alone. Spelling the program name into `command` as well is what
/// rendered `git git rev-parse --show-toplevel: fatal: …` — the second "git"
/// came from the display, the first from the caller naming the same program
/// twice.
///
/// One definition for every caller that labels a git failure, so the rule has
/// one place to regress; `crate::pool`'s own git helpers share it.
///
/// Every call site passes a non-empty argument list, so the empty join (which
/// would render as a bare `git : …`) cannot arise here.
pub(crate) fn git_label(args: &[&str]) -> String {
    args.join(" ")
}

/// Runs `git <args>` in `worktree`, returning trimmed stdout.
fn git_out(git_bin: &Path, worktree: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = std::process::Command::new(git_bin)
        .args(args)
        .current_dir(worktree)
        .output()
        .map_err(|e| GitError::new(git_label(args), e.to_string(), GitErrorKind::Other))?;
    if !output.status.success() {
        return Err(GitError::new(
            git_label(args),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
            GitErrorKind::Other,
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Runs `git <args>` in `worktree`, discarding stdout.
fn git_run(git_bin: &Path, worktree: &Path, args: &[&str]) -> Result<(), GitError> {
    git_out(git_bin, worktree, args).map(|_| ())
}

/// `git rev-parse --verify <ref>^{commit}` — the immutable commit `ref` names.
fn ref_commit(git_bin: &Path, worktree: &Path, reference: &str) -> Result<String, GitError> {
    let spec = format!("{reference}^{{commit}}");
    let sha = git_out(
        git_bin,
        worktree,
        &["rev-parse", "--verify", "--quiet", spec.as_str()],
    )?;
    if !is_commit_id(&sha) {
        return Err(GitError::new(
            git_label(&["rev-parse", "--verify", &spec]),
            format!("{reference} did not resolve to a commit id"),
            GitErrorKind::MergeRefUnresolvable,
        ));
    }
    Ok(sha)
}

/// `git rev-parse --verify HEAD^{commit}` (Go `worktreeHead`).
fn worktree_head(git_bin: &Path, worktree: &Path) -> Result<String, GitError> {
    ref_commit(git_bin, worktree, "HEAD")
}

/// The absolute path of a per-worktree git file (Go `gitPath`).
///
/// For a linked worktree this resolves INSIDE `<main>/.git/worktrees/<name>`,
/// which is the point: a markerless path would resolve the enclosing
/// repository's git dir instead, so this is only ever called after
/// [`require_destructive_marker`] has passed.
fn git_path(git_bin: &Path, worktree: &Path, name: &str) -> Result<PathBuf, GitError> {
    if let Ok(out) = git_out(
        git_bin,
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    ) {
        return Ok(PathBuf::from(out));
    }
    // Older git without --path-format=absolute: compose the absolute git dir
    // with the relative path git reports.
    let git_dir = git_out(git_bin, worktree, &["rev-parse", "--absolute-git-dir"])?;
    let rel = git_out(git_bin, worktree, &["rev-parse", "--git-path", name])?;
    Ok(PathBuf::from(git_dir).join(rel))
}

/// Reports whether `worktree` can be reset to `branch` without discarding
/// committed work, and captures the (reset target, HEAD) pair the reset must
/// reuse (Go `gitvcs.IsWorktreeSafeToReset`).
///
/// Fails CLOSED: an unresolvable ref, an unreadable HEAD, or a git failure is
/// an `Err`, and callers treat that as "not safe". `safe == false` is a real
/// answer the caller may weigh against other evidence; `Err` means the question
/// could not be answered at all.
///
/// `git_bin` is passed in rather than discovered here so the guard can never
/// act on a different git than the one that owns the worktree — a backend impl
/// passes its own binary.
pub fn is_worktree_safe_to_reset(
    git_bin: &Path,
    git: &dyn GitBackend,
    worktree: &Path,
    branch: &str,
) -> Result<ResetGuard, GitError> {
    // The marker precondition, so a check never succeeds for a path whose
    // "HEAD" belongs to some enclosing repository.
    require_destructive_marker(worktree)?;

    // resolveResetRef (gitvcs.go:1058-1065): the branch's ref, resolved ONCE to
    // a commit. Pinning the SHA is what makes the reset target immutable: a ref
    // that moved between check and reset would be reset to something that was
    // never verified.
    let repo_root = git
        .repo_root(worktree)
        .unwrap_or_else(|_| worktree.to_path_buf());
    let reference = git.branch_ref(
        &GitRepo {
            common_dir: repo_root,
            worktree: None,
        },
        branch,
    );
    let reset_ref = ref_commit(git_bin, worktree, &reference)?;
    let head = worktree_head(git_bin, worktree)?;
    let safe = git.is_head_merged_into_ref(worktree, &reset_ref)?;

    Ok(ResetGuard {
        safe,
        reset_ref,
        head,
    })
}

/// Resets `worktree` to an ALREADY RESOLVED commit, re-verifying first (Go
/// `gitvcs.ResetWorktreeToRef`).
///
/// `expected_head` is the HEAD [`is_worktree_safe_to_reset`] recorded. The
/// re-read and the destructive update both run while holding git's own
/// `HEAD.lock` (created `O_CREAT|O_EXCL`), so a concurrent commit, checkout,
/// merge, or rebase cannot create that lock and cannot slip a new commit in
/// after the comparison. When `require_clean` is set, dirtiness is re-checked
/// under that same lock before the tree is touched, so uncommitted work that
/// landed after the caller's own dirty check is not overwritten.
///
/// REFUSES — before touching anything — if the marker is gone, if either
/// argument is not a commit id, if HEAD moved, or if `require_clean` and the
/// tree is dirty. A plain reset without that re-read is worse than no reset:
/// callers believe the safety check and the reset shared one target, so a
/// commit landing in the gap is silently discarded while every guard reports
/// success.
pub fn reset_worktree_to_ref(
    git_bin: &Path,
    git: &dyn GitBackend,
    worktree: &Path,
    reset_ref: &str,
    expected_head: &str,
    require_clean: bool,
) -> Result<(), GitError> {
    require_destructive_marker(worktree)?;

    if !is_commit_id(expected_head) || !is_commit_id(reset_ref) {
        return Err(GitError::new(
            "worktree reset",
            "worktree reset requires resolved commit IDs",
            GitErrorKind::Other,
        ));
    }

    let head_path = git_path(git_bin, worktree, "HEAD")?;
    let mut lock_name = head_path.clone().into_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);

    // O_CREAT|O_EXCL: git holds this lock for anything that changes HEAD, so
    // failing to create it means another git process is mid-operation and we
    // must not race it.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|e| {
            GitError::new(
                "lock worktree HEAD",
                format!("cannot lock worktree HEAD: {e}"),
                GitErrorKind::Other,
            )
        })?;

    // Every early return below must take the lock file with it: a leaked
    // HEAD.lock wedges every later git command in this worktree. Go holds the
    // same defer.
    let mut release = HeadLock {
        lock_file: lock,
        lock_path: &lock_path,
        committed: false,
    };

    let head = worktree_head(git_bin, worktree)?;
    if head != expected_head {
        return Err(GitError::new(
            "worktree reset",
            format!("worktree HEAD changed since safety check: was {expected_head}, now {head}"),
            GitErrorKind::Other,
        ));
    }

    if require_clean {
        match git.is_dirty(worktree) {
            Ok(true) => {
                return Err(GitError::new(
                    "worktree reset",
                    "worktree became dirty after safety check",
                    GitErrorKind::Other,
                ));
            }
            Ok(false) => {}
            Err(e) => return Err(e),
        }
    }

    // read-tree/clean update the working tree without needing HEAD.lock, so
    // they are safe to run under it. HEAD itself is committed last, by
    // renaming the lock file onto HEAD — the same protocol git uses.
    git_run(
        git_bin,
        worktree,
        &["read-tree", "--reset", "-u", reset_ref],
    )?;
    git_run(git_bin, worktree, &["clean", "-fd"])?;

    {
        use std::io::Write;
        let mut file = &release.lock_file;
        writeln!(file, "{reset_ref}")
            .map_err(|e| GitError::new("worktree reset", e.to_string(), GitErrorKind::Other))?;
        file.sync_all()
            .map_err(|e| GitError::new("worktree reset", e.to_string(), GitErrorKind::Other))?;
    }

    install_head(&lock_path, &head_path).map_err(|e| {
        GitError::new(
            "worktree reset",
            format!("installing worktree HEAD: {e}"),
            GitErrorKind::Other,
        )
    })?;
    release.committed = true;
    Ok(())
}

/// Renames `lock_path` onto `head_path` (Go `replaceFile`).
///
/// Plain `fs::rename` is correct only where rename replaces an existing
/// destination. On Windows it does NOT: `HEAD` is always present, so the
/// rename would fail and every reset would leave the worktree half-updated —
/// tree rewritten, HEAD still pointing at the old commit. `MoveFileExW` with
/// `REPLACE_EXISTING | WRITE_THROUGH` is the same call Go makes, and
/// `windows-sys` is already a Windows-only dependency of this crate.
#[cfg(not(windows))]
fn install_head(lock_path: &Path, head_path: &Path) -> std::io::Result<()> {
    std::fs::rename(lock_path, head_path)
}

#[cfg(windows)]
fn install_head(lock_path: &Path, head_path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let wide = |p: &Path| {
        p.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>()
    };
    let (from, to) = (wide(lock_path), wide(head_path));
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Small RAII holder so the lock file is removed on every early return.
struct HeadLock<'a> {
    lock_file: std::fs::File,
    lock_path: &'a Path,
    /// Set once the lock file has been renamed onto HEAD, at which point
    /// removing `lock_path` would delete nothing but must not be attempted.
    committed: bool,
}

impl Drop for HeadLock<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(self.lock_path);
        }
    }
}

// ─── Seed inventory validation ───────────────────────────────────────────────

/// Validates a trusted seed inventory before anything is deleted on its word.
///
/// Go `removeSeededPaths` (gitvcs.go:952-966) applies the same rules, and they
/// are the only thing standing between a state file and an arbitrary path
/// deletion. Each entry must be a clean, relative, single-slash path whose
/// first segment is neither `.git` nor `.jj`: `../x` and `a/../../x` escape the
/// worktree, an absolute path names somewhere else entirely, a backslash is a
/// separator on Windows, and a NUL truncates at the syscall boundary.
///
/// An EMPTY inventory is refused too. Go's rule is that a nil inventory does
/// not authorize ignored-file deletion; for this port a reset that arrives
/// with no recorded inventory but still asks to delete seeded paths is a
/// caller that lost its bookkeeping, and honoring it would convert a lost
/// record into silent data loss. An empty list is meaningless by definition.
pub fn validate_seed_inventory(seeded: &[String]) -> Result<(), GitError> {
    if seeded.is_empty() {
        return Err(GitError::new(
            "validate seeded paths",
            "empty seed inventory does not authorize ignored-file deletion",
            GitErrorKind::Other,
        ));
    }
    for name in seeded {
        let components: Vec<&str> = name.split('/').collect();
        let clean = !name.is_empty()
            && !name.contains('\\')
            && !name.contains('\0')
            && !Path::new(name).is_absolute()
            && !components.iter().any(|c| *c == "." || *c == "..")
            && !components.is_empty();
        if !clean {
            return Err(invalid_seed_path(name));
        }
        let first = components[0];
        if first.eq_ignore_ascii_case(".git") || first.eq_ignore_ascii_case(".jj") {
            return Err(invalid_seed_path(name));
        }
    }
    Ok(())
}

fn invalid_seed_path(name: &str) -> GitError {
    GitError::new(
        "validate seeded paths",
        format!("invalid seeded path {name:?}"),
        GitErrorKind::Other,
    )
}

/// The standard "this backend does not do that" refusal (Go has no equivalent:
/// every method is implemented by both backends).
///
/// One message for all of them so an operator reading a log can tell "treehouse
/// asked for a capability the selected backend does not have" apart from "the
/// capability failed" — the two have completely different remedies.
///
/// Takes the backend NAME rather than `&dyn GitBackend` because it is called
/// from default trait methods, where `Self: ?Sized` forbids the coercion.
pub(crate) fn unsupported(backend_name: &str, operation: &str) -> GitError {
    GitError::new(
        operation,
        format!("the {backend_name} backend does not support {operation}"),
        GitErrorKind::Other,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A real repo with one commit, so the guarded-reset tests exercise the
    /// same plumbing a pool slot does.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "one"]);
        (dir, root)
    }

    fn git_backend() -> ShellGitBackend {
        ShellGitBackend::discover().expect("git must be installed")
    }

    // ── marker inspection ────────────────────────────────────────────────────

    #[test]
    fn a_git_worktree_resolves_to_git() {
        let (_d, root) = repo();
        assert_eq!(worktree_backend_name(&root).unwrap(), Some(BACKEND_GIT));
    }

    #[test]
    fn a_jj_workspace_resolves_to_jj() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        assert_eq!(worktree_backend_name(dir.path()).unwrap(), Some(BACKEND_JJ));
    }

    #[test]
    fn a_jj_file_is_not_a_workspace_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".jj"), "not a workspace").unwrap();
        assert_eq!(worktree_backend_name(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_markerless_path_is_absent_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(worktree_backend_name(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_dangling_git_marker_is_a_read_failure_not_an_absent_marker() {
        // The distinction the whole safety argument rests on: an unresolvable
        // marker must never read as "no marker", or the slot is classified
        // damaged instead of unreadable and handled by a different code path.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("gone");
        let link = dir.path().join(".git");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();

        let err =
            worktree_backend_name(dir.path()).expect_err("dangling marker must be a read error");
        assert!(
            err.message.contains("resolving .git marker"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn a_dangling_jj_marker_is_a_read_failure_too() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("gone");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dir.path().join(".jj")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, dir.path().join(".jj")).unwrap();

        assert!(worktree_backend_name(dir.path()).is_err());
    }

    // ── destructive dispatch ─────────────────────────────────────────────────

    #[test]
    fn a_git_worktree_dispatches_to_the_registered_git_backend() {
        let (_d, root) = repo();
        let b = destructive_backend_for_worktree(&root).expect("git slot must resolve");
        assert_eq!(b.name(), BACKEND_GIT);
    }

    #[test]
    fn destructive_dispatch_refuses_a_markerless_path() {
        // The behavior this whole seam exists for: a markerless path nested
        // inside a real repo is the in-project pool layout, where git would
        // walk UP and reset the user's own working tree.
        let (_d, root) = repo();
        let slot = root.join("slot");
        std::fs::create_dir_all(&slot).unwrap();

        let err = destructive_backend_for_worktree(&slot)
            .err()
            .expect("a markerless slot must be refused, never answered by the enclosing repo");
        assert!(
            err.message.contains("no .git or .jj marker"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn read_dispatch_falls_back_to_git_for_a_markerless_path() {
        // Reads are harmless to guess, so this one falls back like Go's
        // backendForWorktree does.
        let dir = tempfile::tempdir().unwrap();
        let b = backend_for_worktree(dir.path()).expect("reads fall back to git");
        assert_eq!(b.name(), BACKEND_GIT);
    }

    #[test]
    fn dispatch_refuses_a_marker_naming_an_unregistered_backend() {
        // A `.jj` workspace answered by git would run git commands in a non-git
        // tree and report the failure in misleading terms.
        //
        // Probed on a PRIVATE, EMPTY registry rather than the global one. It
        // used to assert that jj was unregistered, which made it a test of jj's
        // REGISTRATION STATUS rather than of the safety rule it exists for —
        // and that rule (a name with no handle is refused, never guessed) is
        // what must survive. The global registry now seeds jj, so the same
        // assertion there would be asserting the opposite of what it says.
        let registry = BackendRegistry::new();
        let err = registry
            .resolve_or_default(Some(BACKEND_JJ))
            .err()
            .expect("a name with no registered backend must be refused");
        assert!(
            err.message.contains("no backend is registered"),
            "got: {}",
            err.message
        );

        // And the same rule through the public entry point: the global registry
        // DOES answer a `.jj` marker, with jj.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        assert_eq!(backend_for_worktree(dir.path()).unwrap().name(), BACKEND_JJ);
        assert_eq!(
            destructive_backend_for_worktree(dir.path()).unwrap().name(),
            BACKEND_JJ
        );
    }

    #[test]
    fn registering_jj_does_not_disturb_a_git_repository() {
        // The whole opt-in argument for jj rests on this: seeding the backend
        // into the global registry must leave every git path exactly as it was.
        let (_d, root) = repo();
        assert_eq!(backend_for_worktree(&root).unwrap().name(), BACKEND_GIT);
        assert_eq!(
            destructive_backend_for_worktree(&root).unwrap().name(),
            BACKEND_GIT
        );
        // A colocated repository stays on git worktrees — `.git` is checked
        // before `.jj`, so the colocated case cannot silently flip flavor.
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        assert_eq!(worktree_backend_name(&root).unwrap(), Some(BACKEND_GIT));
        assert_eq!(backend_for_worktree(&root).unwrap().name(), BACKEND_GIT);
    }

    #[test]
    fn the_registry_is_extensible_without_touching_the_trait() {
        let registry = BackendRegistry::new();
        registry.register(BACKEND_GIT, Arc::new(git_backend()));
        assert_eq!(registry.names(), vec![BACKEND_GIT]);
        assert!(registry.get("nope").is_none());
        assert!(registry.get(BACKEND_GIT).is_some());
    }

    #[test]
    fn the_traits_default_dispatch_reaches_the_registry() {
        // Every backend inherits registry dispatch for free, which is what
        // lets the jj backend register without any call site changing. If this
        // stops working, `resolve_for_worktree` silently starts refusing
        // everything and the seam stops being a seam.
        let (_d, root) = repo();
        let git = git_backend();
        assert_eq!(git.name(), BACKEND_GIT, "ShellGitBackend is git by default");

        let resolved = git
            .resolve_for_worktree(&root)
            .expect("a git worktree must resolve through the registry");
        assert_eq!(resolved.name(), BACKEND_GIT);

        let slot = root.join("no-marker");
        std::fs::create_dir_all(&slot).unwrap();
        assert!(
            git.resolve_for_worktree(&slot).is_err(),
            "the default must refuse a markerless path, not answer it"
        );
    }

    #[test]
    fn an_unimplemented_capability_refuses_by_name() {
        // `common_git_dir` and `seed_worktree` probed this while the seeding
        // agent still had them outstanding. They are implemented now, so the
        // refusal path is probed with the guarded-reset pair — the other half
        // of the "declared now, defaults to a refusal" block, and still
        // unwired on purpose: the pool calls the free functions in this module
        // and does not dispatch through the trait yet (see the note in
        // `pool.rs`). If an agent overrides these and this starts failing,
        // that agent should move the probe, not delete it — the default
        // refusal is a safety surface, not dead code.
        let (_d, root) = repo();
        let git = git_backend();
        for err in [
            git.is_worktree_safe_to_reset(&root, "main").err(),
            git.reset_worktree_to_ref(&root, "deadbeef", "cafebabe", false)
                .err(),
        ] {
            let err = err.expect("a backend that does not implement this must refuse");
            assert!(
                err.message.contains("does not support"),
                "got: {}",
                err.message
            );
        }
    }

    #[test]
    fn an_empty_seed_inventory_is_refused_before_the_capability_check() {
        // The refusal must name the lost bookkeeping, not "unsupported" —
        // they are different bugs with different fixes.
        let (_d, root) = repo();
        let git = git_backend();
        let err = git
            .reset_worktree_with_seeded_paths(&root, "main", &[])
            .expect_err("an empty inventory must refuse");
        assert!(
            err.message.contains("seed inventory"),
            "got: {}",
            err.message
        );
    }

    // ── configured selection (the CLI's `--branch` refusal) ─────────────────

    /// A colocated repository: `.git` and `.jj` together at one root.
    fn colocated() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join(".jj")).unwrap();
        (dir, root)
    }

    /// A `.jj`-only main workspace with a secondary workspace pointing at it,
    /// laid out the way jj writes the pointer.
    fn jj_only_main() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(main.join(".jj").join("repo")).unwrap();
        let ws = dir.path().join("wt-1");
        std::fs::create_dir_all(ws.join(".jj")).unwrap();
        std::fs::write(ws.join(".jj").join("repo"), "../../main/.jj/repo").unwrap();
        (dir, main, ws)
    }

    #[test]
    fn no_marker_anywhere_up_the_tree_means_no_marker_root() {
        // tempfile::tempdir lives under /var -> /private/var on macOS, and
        // neither chain has a marker, so the walk reaches the filesystem root.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(marker_root(dir.path()), None);
    }

    #[test]
    fn the_marker_root_is_the_nearest_ancestor_holding_a_marker() {
        let (_d, root) = colocated();
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            marker_root(&nested),
            Some(MarkerRoot {
                root: root.clone(),
                has_git: true,
                has_jj: true,
            })
        );
    }

    #[test]
    fn a_jj_only_tree_is_distinguished_from_a_colocated_one() {
        let (_d, _main, ws) = jj_only_main();
        let found = marker_root(&ws).expect("a jj workspace is a marker root");
        assert!(found.has_jj);
        assert!(
            !found.has_git,
            "a pooled jj workspace is .jj-only — it has no .git at all"
        );
    }

    #[test]
    fn an_explicit_git_opt_in_wins_even_where_a_jj_directory_exists() {
        let (_d, root) = colocated();
        assert_eq!(
            configured_backend_name(&root, Some(BACKEND_GIT)).unwrap(),
            BACKEND_GIT
        );
    }

    #[test]
    fn a_jj_opt_in_only_takes_effect_where_a_jj_directory_exists() {
        // The rule that makes a shell-wide `TREEHOUSE_VCS=jj` harmless: in a
        // plain git repository the opt-in is silently ignored.
        let (_d, root) = repo();
        assert_eq!(
            configured_backend_name(&root, Some(BACKEND_JJ)).unwrap(),
            BACKEND_GIT
        );

        let (_d, colocated_root) = colocated();
        assert_eq!(
            configured_backend_name(&colocated_root, Some(BACKEND_JJ)).unwrap(),
            BACKEND_JJ
        );
    }

    #[test]
    fn a_colocated_repository_stays_on_git_without_an_explicit_opt_in() {
        let (_d, root) = colocated();
        assert_eq!(
            configured_backend_name(&root, None).unwrap(),
            BACKEND_GIT,
            "having a .jj directory is not consent; only configuration selects a backend"
        );
    }

    #[test]
    fn a_workspace_inherits_the_opt_in_from_the_main_repository_root() {
        // A pooled jj workspace is a `.jj`-only checkout that cannot carry an
        // untracked treehouse.toml, so the opt-in is read at the main root the
        // `.jj/repo` pointer names.
        let (_d, _main, ws) = jj_only_main();
        assert_eq!(
            configured_backend_name(&ws, Some(BACKEND_JJ)).unwrap(),
            BACKEND_JJ
        );
    }

    #[test]
    fn a_path_outside_any_repository_answers_git_so_errors_surface_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            configured_backend_name(dir.path(), None).unwrap(),
            BACKEND_GIT
        );
        assert_eq!(
            configured_backend_name(dir.path(), Some(BACKEND_JJ)).unwrap(),
            BACKEND_GIT
        );
    }

    #[test]
    fn a_misconfigured_opt_in_is_ignored_rather_than_fatal() {
        // A typo must not break every command in the repository.
        let (_d, root) = colocated();
        for junk in ["Jujutsu", "JJ", " jj", "jjj", "subversion"] {
            assert_eq!(
                configured_backend_name(&root, Some(junk)).unwrap(),
                BACKEND_GIT,
                "{junk:?} must be ignored, not honoured and not fatal"
            );
        }
    }

    // ── the guarded-reset pair ───────────────────────────────────────────────

    #[test]
    fn a_check_then_reset_on_a_clean_worktree_succeeds() {
        // The shape pool reuse actually runs: prove, then reset under
        // `require_clean` with the tree still clean.
        let (_d, root) = repo();
        let git = git_backend();
        let bin = git.git_bin().to_path_buf();

        let guard = is_worktree_safe_to_reset(&bin, &git, &root, "main").unwrap();
        assert!(guard.safe, "a worktree at main is safe to reset to main");
        assert!(is_commit_id(&guard.reset_ref) && is_commit_id(&guard.head));

        reset_worktree_to_ref(&bin, &git, &root, &guard.reset_ref, &guard.head, true).unwrap();
        assert!(!git.is_dirty(&root).unwrap());
    }

    #[test]
    fn a_reset_restores_the_checkout_and_sweeps_untracked_files() {
        // `require_clean=false` is the path Go's
        // `ResetWorktreeWithSeededPaths` takes: the caller resolved HEAD itself
        // and has already decided what to discard.
        let (_d, root) = repo();
        let wt = root.join("wt");
        let git = git_backend();
        let bin = git.git_bin().to_path_buf();

        let added = Command::new(bin.as_os_str())
            .args(["worktree", "add", "--detach"])
            .arg(&wt)
            .arg("main")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(added.status.success(), "{added:?}");

        let guard = is_worktree_safe_to_reset(&bin, &git, &wt, "main").unwrap();
        std::fs::write(wt.join("a.txt"), "clobbered\n").unwrap();
        std::fs::write(wt.join("junk.txt"), "untracked\n").unwrap();

        reset_worktree_to_ref(&bin, &git, &wt, &guard.reset_ref, &guard.head, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).unwrap(),
            "one\n",
            "read-tree --reset -u must restore tracked content"
        );
        assert!(
            !wt.join("junk.txt").exists(),
            "clean -fd must remove the untracked file"
        );
        assert!(!git.is_dirty(&wt).unwrap());
    }

    #[test]
    fn a_check_on_a_worktree_ahead_of_base_is_not_safe() {
        let (_d, root) = repo();
        let git = git_backend();
        let bin = git.git_bin().to_path_buf();
        let run = |args: &[&str]| {
            assert!(
                Command::new(bin.as_os_str())
                    .args(args)
                    .current_dir(&root)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            );
        };
        run(&["checkout", "-q", "-b", "ahead"]);
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "two"]);

        let guard = is_worktree_safe_to_reset(&bin, &git, &root, "main").unwrap();
        assert!(!guard.safe, "unlanded work must never read as safe");
        // The pair is still returned: a caller may weigh other evidence.
        assert!(is_commit_id(&guard.reset_ref) && is_commit_id(&guard.head));
    }

    #[test]
    fn a_reset_refuses_when_head_moved_since_the_check() {
        // The single most important test in this file. A reset that does not
        // re-verify is worse than none: every caller would report success
        // while discarding a commit that landed in the gap.
        let (_d, root) = repo();
        let git = git_backend();
        let bin = git.git_bin().to_path_buf();
        let run = |args: &[&str]| {
            assert!(
                Command::new(bin.as_os_str())
                    .args(args)
                    .current_dir(&root)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            );
        };

        let guard = is_worktree_safe_to_reset(&bin, &git, &root, "main").unwrap();

        // Land a commit the check never saw.
        run(&["checkout", "-q", "-b", "sneaky"]);
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "two"]);

        let err = reset_worktree_to_ref(&bin, &git, &root, &guard.reset_ref, &guard.head, true)
            .expect_err("a moved HEAD must refuse");
        assert!(
            err.message.contains("HEAD changed since safety check"),
            "got: {}",
            err.message
        );

        // And the commit is still there — the refusal, not a lost commit.
        assert!(root.join("b.txt").exists());
    }

    #[test]
    fn a_reset_refuses_a_tree_that_became_dirty_after_the_check() {
        let (_d, root) = repo();
        let git = git_backend();
        let bin = git.git_bin().to_path_buf();

        let guard = is_worktree_safe_to_reset(&bin, &git, &root, "main").unwrap();
        std::fs::write(root.join("a.txt"), "uncommitted work\n").unwrap();

        let err = reset_worktree_to_ref(&bin, &git, &root, &guard.reset_ref, &guard.head, true)
            .expect_err("dirty-after-check must refuse");
        assert!(err.message.contains("became dirty"), "got: {}", err.message);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "uncommitted work\n",
            "the refusal must not have written anything"
        );
    }

    #[test]
    fn a_reset_refuses_an_argument_that_is_not_a_commit_id() {
        // A ref name smuggled into reset_ref would let read-tree move the
        // worktree to something the safety check never verified.
        let (_d, root) = repo();
        let git = git_backend();
        let bin = git.git_bin().to_path_buf();
        let sha = "0000000000000000000000000000000000000000";

        for bad in ["main", "refs/heads/main", "HEAD", "", "NOTAHEXSHA"] {
            assert!(
                reset_worktree_to_ref(&bin, &git, &root, bad, sha, true).is_err(),
                "reset_ref {bad:?} must be refused"
            );
            assert!(
                reset_worktree_to_ref(&bin, &git, &root, sha, bad, true).is_err(),
                "expected_head {bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_reset_refuses_a_markerless_path() {
        // Inside a real repo, so the only thing standing between this and the
        // user's working tree is the marker precondition.
        let (_d, root) = repo();
        let git = git_backend();
        let bin = git.git_bin().to_path_buf();
        let nested = root.join("not-a-worktree");
        std::fs::create_dir_all(&nested).unwrap();

        let err = reset_worktree_to_ref(
            &bin,
            &git,
            &nested,
            "0000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000",
            true,
        )
        .expect_err("a markerless path must be refused");
        assert!(
            err.message.contains("no .git or .jj marker"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn a_refused_reset_leaves_no_head_lock_behind() {
        // A leaked HEAD.lock wedges every later git command in the worktree,
        // so the early-return path has to clean up.
        let (_d, root) = repo();
        let git = git_backend();
        let bin = git.git_bin().to_path_buf();

        let guard = is_worktree_safe_to_reset(&bin, &git, &root, "main").unwrap();
        let _ = reset_worktree_to_ref(&bin, &git, &root, &guard.reset_ref, &guard.head, true);

        let head = git_path(&bin, &root, "HEAD").unwrap();
        let mut lock = head.into_os_string();
        lock.push(".lock");
        assert!(!Path::new(&lock).exists(), "HEAD.lock leaked");
    }

    // ── seed inventory ───────────────────────────────────────────────────────

    #[test]
    fn an_empty_seed_inventory_authorizes_no_deletion() {
        assert!(validate_seed_inventory(&[]).is_err());
    }

    #[test]
    fn traversal_and_metadata_paths_are_refused() {
        for bad in [
            "../escape",
            "a/../../escape",
            "/etc/passwd",
            ".git/config",
            ".jj/repo/store/git_target",
            "./relative",
            "a/./b",
            "back\\slash",
            "",
        ] {
            assert!(
                validate_seed_inventory(&[bad.to_string()]).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn ordinary_seed_paths_are_accepted() {
        assert!(
            validate_seed_inventory(&[".env".to_string(), ".vscode/settings.json".to_string()])
                .is_ok()
        );
    }

    // ── backend selection for repository-root resolution ─────────────────────

    /// A jj that records its argv and answers `workspace root` with `root`, or
    /// fails every command when `failure` is set.
    struct RecordingJj {
        root: PathBuf,
        failure: Option<&'static str>,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl RecordingJj {
        fn serving(root: PathBuf) -> Arc<Self> {
            Arc::new(Self {
                root,
                failure: None,
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn failing(root: PathBuf) -> Arc<Self> {
            Arc::new(Self {
                root,
                failure: Some("there is no jj workspace at that path"),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    impl jj::JjRunner for RecordingJj {
        fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|s| s.to_string()).collect());
            if let Some(message) = self.failure {
                return Err(GitError::new("jj", message, GitErrorKind::NotFound));
            }
            match args {
                ["--color", "never", "workspace", "root"] => {
                    Ok(self.root.to_string_lossy().into_owned())
                }
                _ => Err(GitError::new(
                    "jj",
                    format!("unexpected argv {args:?} in {}", cwd.display()),
                    GitErrorKind::Other,
                )),
            }
        }
    }

    /// A `.jj`-only tree: a workspace with no colocated git repository.
    fn jj_only_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        dir
    }

    #[test]
    fn a_jj_selection_reaches_jj_and_never_git() {
        let tree = jj_only_tree();
        let runner = RecordingJj::serving(tree.path().to_path_buf());
        let registry = BackendRegistry::new();
        registry.register(
            BACKEND_JJ,
            Arc::new(jj::JjBackend::with_runner(runner.clone())),
        );

        let root = registry
            .repo_root_for(tree.path(), Some(BACKEND_JJ))
            .expect("jj must serve a .jj-only tree it was selected for");

        assert_eq!(root, std::fs::canonicalize(tree.path()).unwrap());
        // The whole point: dispatch reached jj. A git-rooted loader would have
        // shelled out to git and reported its fatal instead of answering.
        assert!(
            runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.ends_with(&["workspace".to_string(), "root".to_string()])),
            "jj must have been asked for the workspace root, calls: {:?}",
            runner.calls.lock().unwrap()
        );
    }

    #[test]
    fn an_unserviceable_repository_is_refused_by_name_with_a_remedy() {
        let tree = jj_only_tree();
        let registry = BackendRegistry::new();
        // jj registered, but jj is what cannot serve the repository — the shape
        // of "jj is selected and jj is not available here".
        registry.register(
            BACKEND_JJ,
            Arc::new(jj::JjBackend::with_runner(RecordingJj::failing(
                tree.path().to_path_buf(),
            ))),
        );

        let err = registry
            .repo_root_for(tree.path(), Some(BACKEND_JJ))
            .expect_err("a repository jj cannot serve must be refused");

        let rendered = err.to_string();
        assert!(
            rendered.contains(BACKEND_JJ),
            "must name the backend: {rendered}"
        );
        assert!(
            rendered.contains("will not fall back"),
            "must fail closed: {rendered}"
        );
        // The cause is quoted, not swallowed: the operator still sees what jj
        // actually said, and this string is jj's, not ours.
        assert!(
            rendered.contains("there is no jj workspace at that path"),
            "must quote the cause: {rendered}"
        );
        // And the remedy is OURS — it names JJ_BIN, which the cause above does
        // not, so this can only have come from `backend_cannot_serve`.
        assert!(
            rendered.contains("JJ_BIN"),
            "must give a remedy: {rendered}"
        );
        // The kind travels with the refusal, so callers classifying on it see
        // "the backend was missing", not a generic failure.
        assert_eq!(err.kind, GitErrorKind::NotFound);
    }

    #[test]
    fn a_missing_jj_backend_is_named_rather_than_reported_as_unregistered() {
        let tree = jj_only_tree();
        let registry = BackendRegistry::new();
        // NO jj registered. The refusal must still name jj and offer the
        // remedy — "no backend is registered for jj" sends the operator
        // looking for a configuration problem that does not exist.
        let err = registry
            .repo_root_for(tree.path(), Some(BACKEND_JJ))
            .expect_err("an unregistered jj must be refused");

        let rendered = err.to_string();
        assert!(
            rendered.contains(BACKEND_JJ),
            "must name the backend: {rendered}"
        );
        assert!(
            rendered.contains("will not fall back"),
            "must fail closed: {rendered}"
        );
        assert!(
            rendered.contains("JJ_BIN"),
            "must give a remedy: {rendered}"
        );
    }

    #[test]
    fn a_git_selection_still_resolves_an_ordinary_repository() {
        // The non-jj path must be untouched: `repo_root_for` is a new entry
        // point, and adopting it must not change what a git repository does.
        let (_d, root) = repo();
        let resolved = global_registry()
            .repo_root_for(&root, None)
            .expect("git must serve its own repository");
        assert_eq!(resolved, std::fs::canonicalize(&root).unwrap());
    }

    #[test]
    fn a_jj_selection_outside_a_jj_tree_still_answers_git() {
        // `configured_backend_name` only honours a jj opt-in where a `.jj`
        // directory exists. That rule is what makes a shell-wide
        // `TREEHOUSE_VCS=jj` harmless, and it must survive the new entry point.
        let (_d, root) = repo();
        let registry = BackendRegistry::new();
        registry.register(
            BACKEND_JJ,
            Arc::new(jj::JjBackend::with_runner(RecordingJj::serving(
                root.clone(),
            ))),
        );
        assert_eq!(
            registry.repo_root_for(&root, Some(BACKEND_JJ)).unwrap(),
            std::fs::canonicalize(&root).unwrap(),
            "a git tree with a stray jj opt-in must still be served by git"
        );
    }

    // ── error labels ─────────────────────────────────────────────────────────

    #[test]
    fn a_git_error_label_names_the_program_exactly_once() {
        // Regression: the label used to render `git git rev-parse
        // --show-toplevel: …`, because the caller spelled the program name into
        // the `command` field and `GitError`'s Display spells it again.
        let rendered = GitError::new(
            git_label(&["rev-parse", "--show-toplevel"]),
            "fatal: not a git repository",
            GitErrorKind::Other,
        )
        .to_string();
        assert_eq!(
            rendered,
            "git rev-parse --show-toplevel: fatal: not a git repository"
        );
    }

    #[test]
    fn a_real_git_failure_through_this_seam_labels_the_program_once() {
        // The label test above proves the string helper; this one proves the
        // helper is what the seam actually reports, by running the real git
        // binary and letting it fail. A `.git` FILE (rather than a directory)
        // satisfies the marker precondition and then fails every git command —
        // the same shape as a damaged checkout.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /gone\n").unwrap();

        let git = git_backend();
        let err = is_worktree_safe_to_reset(git.git_bin(), &git, dir.path(), "main")
            .expect_err("a non-repository carrying a .git marker must fail the check");

        let rendered = err.to_string();
        assert!(
            rendered.starts_with("git rev-parse "),
            "label must name git exactly once: {rendered}"
        );
        assert!(
            !rendered.starts_with("git git "),
            "the program name must not be doubled: {rendered}"
        );
    }
}
