//! The jj (Jujutsu) backend — Go `internal/vcs/jjvcs`.
//!
//! # jj is not git, and this does not pretend otherwise
//!
//! The mapping this file implements, verbatim from the Go package doc:
//!
//! | git backend            | jj backend                                              |
//! |------------------------|---------------------------------------------------------|
//! | worktree               | jj workspace (`jj workspace add` / `forget`)            |
//! | detached HEAD          | nothing — jj working copies are anonymous commits        |
//! | dirty                  | the working-copy commit `@` is non-empty or described   |
//! | `reset --hard`         | `jj abandon -r @` + `jj new <ref>` (recoverable via `jj op restore`) |
//! | `merge-base --is-ancestor` | the revset `@- & ~::<ref>` is empty                 |
//!
//! Three consequences run through every method below:
//!
//! * **A jj workspace is not a git worktree and has no `.git` entry.** Pooled
//!   secondary workspaces are `.jj`-only trees. Nothing here may resolve a
//!   workspace's git dir by walking UP — in a COLOCATED repository that walk
//!   lands on the user's own working tree. See [`require_jj_workspace`].
//! * **A bookmark is mutable** where a branch ref is not. The reset target is
//!   therefore resolved to a commit id ([`resolve_reset_ref`]) before anything
//!   destructive runs, exactly as the git backend pins `reset_ref`, and the
//!   pair travels together in a [`ResetGuard`].
//! * **jj is lock-free.** `jj` snapshots the operation log and always commits,
//!   so no `flock` can exclude a parallel `jj commit`. The guarded reset is
//!   therefore a SINGLE `jj rebase` whose revset pins `@` by commit id: a
//!   concurrent move of `@` empties the revset, the rebase becomes a no-op, and
//!   the parked-on-target re-check that follows turns that into a REFUSAL
//!   rather than a silent success. See [`JjBackend::reset_worktree_to_ref`].
//!
//! # Testability
//!
//! `jj` is not installed on every machine that builds this crate, and a backend
//! whose commands cannot be faked is a backend whose logic is untestable.
//! Every invocation goes through [`JjRunner`], so the argv construction, the
//! marker/flavor resolution, the fail-closed refusals and the whole
//! clean-vs-dirty reset protocol are unit-tested against a scripted runner.
//! What that does NOT cover is jj's own output format; see the coverage note
//! on [`JjBackend::discover`].
//!
//! # Opt-in
//!
//! jj stays strictly opt-in (`TREEHOUSE_VCS` / `treehouse.toml` `vcs` / user
//! config, in that precedence order) and only where a `.jj` directory actually
//! exists. That precedence lives in the config layer. REGISTERING this backend
//! is not opting into it: registration only makes a `.jj` marker answerable, and
//! which backend CREATES a worktree is the configured backend's business.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::git::{GitBackend, GitError, GitErrorKind, GitRepo, ResetGuard};

use super::{BACKEND_GIT, BACKEND_JJ};

/// jj's own trunk preference order (Go `defaultBranchCandidates`).
const DEFAULT_BRANCH_CANDIDATES: [&str; 3] = ["main", "master", "trunk"];

/// Template printing the commit id followed by a newline. jj's template language
/// uses Rust-style string escapes, so the bytes handed to jj contain a literal
/// backslash-n — a Rust `"\n"` here would hand jj a real newline inside a string
/// literal and fail to parse.
const COMMIT_ID_TEMPLATE: &str = r#"commit_id ++ "\n""#;

/// Template printing `clean` or `dirty` for the working-copy commit.
///
/// `empty` is jj's emptiness predicate and `description` the commit message;
/// a workspace is dirty when either is set. Running any jj command snapshots
/// the working copy first, so filesystem changes are always reflected here.
const DIRTY_TEMPLATE: &str = r#"if(empty && description == "", "clean", "dirty")"#;

// ─── The command seam ─────────────────────────────────────────────────────────

/// Runs `jj --color never <args>` in a directory and returns trimmed stdout.
///
/// Split out so the backend's LOGIC is testable without jj installed. The argv
/// handed here is the complete command line, `--color never` included: a runner
/// must not add flags of its own, or the argv a test asserts is not the argv
/// jj would have seen.
pub(crate) trait JjRunner: Send + Sync {
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError>;
}

/// Runs the real `jj` binary, found by `JJ_BIN` → `PATH`.
///
/// `bin` is `None` when discovery found nothing. That is NOT a reason to refuse
/// construction: the backend is still the right answer for a `.jj` marker, and
/// every command then fails with the true cause ("jj binary not found") instead
/// of the seam reporting "no backend is registered for jj", which sends the
/// operator looking for a configuration problem that does not exist.
struct RealJjRunner {
    bin: Option<PathBuf>,
}

impl RealJjRunner {
    fn discover() -> Self {
        Self {
            bin: find_jj_binary(),
        }
    }
}

impl JjRunner for RealJjRunner {
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        let command = format!("jj {}", args.join(" "));
        let Some(bin) = &self.bin else {
            return Err(GitError::new(
                command,
                "jj binary not found on PATH (set JJ_BIN)",
                GitErrorKind::NotFound,
            ));
        };
        let output = std::process::Command::new(bin)
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|e| GitError::new(command.clone(), e.to_string(), GitErrorKind::Other))?;
        if !output.status.success() {
            return Err(GitError::new(
                command,
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
                GitErrorKind::Other,
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// `JJ_BIN` env override → `PATH`.
fn find_jj_binary() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("JJ_BIN") {
        let p = PathBuf::from(&v);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    let exe = if cfg!(windows) { "jj.exe" } else { "jj" };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ─── Pure helpers ─────────────────────────────────────────────────────────────

/// The absolute form of `path` (Go `filepath.Abs`).
fn absolute(path: &Path) -> Result<PathBuf, GitError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|e| {
        GitError::new(
            "resolve absolute path",
            format!("reading the working directory for {}: {e}", path.display()),
            GitErrorKind::Other,
        )
    })?;
    Ok(cwd.join(path))
}

/// Resolves symlinks, falling back to the input (Go `canonicalize`).
///
/// Root resolution must return ONE canonical form no matter which route
/// produced the path: the pool identity is derived from the path string, so a
/// repository reached through a symlink (macOS `/tmp`) would otherwise fork
/// into a real pool and a phantom one.
fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The bytes of a path, without a UTF-8 round trip.
///
/// The workspace name is a digest of the path, so a lossy conversion would give
/// two different paths the same name on a filesystem that permits non-UTF-8.
#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// A stable jj workspace name derived from a worktree path (Go
/// `workspaceNameFor`): `th-` followed by 16 bytes of sha256 of the ABSOLUTE
/// path.
///
/// The name is path-keyed, which is what makes the self-healing forget in
/// [`JjBackend::worktree_add`] safe: jj repositories do not record workspace
/// directory paths, so a registration under this name can only have been left
/// by a previous worktree at THIS path. A live workspace at another path holds a
/// different digest and can never be deregistered by mistake.
pub fn workspace_name_for(path: &Path) -> String {
    let abs = absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let digest = Sha256::digest(path_bytes(&abs));
    let mut name = String::from("th-");
    for b in &digest[..16] {
        name.push_str(&format!("{b:02x}"));
    }
    name
}

/// jj's commit id: 40 (git backend) or 64 (native backend) lowercase hex.
///
/// Stricter than the seam's git-side `is_commit_id`, which accepts 1..=64 hex
/// characters because git ids are always 40. Matching jj's real widths matters
/// because the value is interpolated into a revset
/// (`@ & commit_id("...")`), where a truncated or padded value is not a commit
/// id at all but a silent no-match.
fn is_commit_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `s` may be embedded in a revset expression.
///
/// jj revsets are a LANGUAGE, not a path grammar: `()`, `&`, `|`, `~`, `:`,
/// `"`, `\` and whitespace are operators, and a branch name reaching a revset
/// unquoted is revset injection — `foo" | ~root` would rewrite the very working
/// copy this backend exists to protect. Go interpolates branch names raw; this
/// port does not.
///
/// The allowlist keeps every name jj itself accepts in practice (letters,
/// digits, `.`, `_`, `-`, `/`, and the `@` that separates a bookmark from its
/// remote-tracking form, as in `main@origin`) and rejects everything else. The
/// first character must be alphanumeric so a name can never OPEN with a revset
/// symbol — notably so a bookmark can never be named exactly `@`, which would
/// make `present(@)` resolve to the working-copy commit instead of a bookmark.
///
/// An exotic-but-legal jj bookmark name is refused with a clear error rather
/// than silently rewritten. That is the fail-closed direction.
fn revset_atom(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'@'))
}

/// Reads the first line of a `commit_id ++ "\n"` template, erroring when empty.
///
/// Empty means the revset matched nothing. Reporting THAT — rather than handing
/// the empty string on as a ref — is what turns "the bookmark does not resolve"
/// into an error before a destructive command is built.
fn first_commit_id(out: &str, what: &str) -> Result<String, GitError> {
    let id = out.lines().next().unwrap_or("").trim();
    if id.is_empty() {
        return Err(GitError::new(
            "jj log",
            format!("cannot resolve {what}"),
            GitErrorKind::MergeRefUnresolvable,
        ));
    }
    Ok(id.to_string())
}

/// Rewrites a secondary workspace's `.jj/repo` store pointer to an absolute,
/// symlink-canonicalized path (Go `makeRepoPointerAbsolute`).
///
/// jj writes a RELATIVE pointer, which breaks when the pool directory and the
/// repository do not move together — and the pool usually lives under
/// `~/.treehouse`, far from the repo. Canonicalizing on write mirrors git,
/// which stores physical paths in a worktree's `.git` gitdir pointer, so every
/// later read agrees with `jj workspace root` (physical) and one repository
/// resolves to ONE pool identity.
///
/// A MAIN workspace's `.jj/repo` is the store itself, not a pointer; there is
/// nothing to rewrite and it is left untouched.
fn make_repo_pointer_absolute(ws_root: &Path) -> Result<(), GitError> {
    let repo_path = ws_root.join(".jj").join("repo");
    let meta = std::fs::metadata(&repo_path).map_err(|e| {
        GitError::new(
            "make .jj/repo pointer absolute",
            format!("inspecting {}: {e}", repo_path.display()),
            GitErrorKind::Other,
        )
    })?;
    if meta.is_dir() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(&repo_path).map_err(|e| {
        GitError::new(
            "make .jj/repo pointer absolute",
            format!("reading {}: {e}", repo_path.display()),
            GitErrorKind::Other,
        )
    })?;
    let store = contents.trim();
    let store_path = PathBuf::from(store);
    if store_path.is_absolute() {
        return Ok(());
    }
    let abs = canonicalize(&ws_root.join(".jj").join(store));
    std::fs::write(&repo_path, abs.to_string_lossy().as_bytes()).map_err(|e| {
        GitError::new(
            "make .jj/repo pointer absolute",
            format!("writing {}: {e}", repo_path.display()),
            GitErrorKind::Other,
        )
    })
}

/// Resolves a workspace root to the MAIN repository root by reading the
/// `.jj/repo` store pointer (Go `MainRootFromWorkspaceRoot`).
///
/// PURE FILE INSPECTION — no jj invocation — so it works for locating where a
/// repository's configuration lives before any backend has been selected, and
/// so it is unit-testable on a machine without jj.
///
/// - `.jj/repo` is a DIRECTORY: this is the main workspace; its own root is the
///   answer.
/// - `.jj/repo` is a FILE holding the store path (possibly relative to the
///   workspace's `.jj` directory). The store lives at `<main>/.jj/repo`, so the
///   main root is two levels up from the store.
///
/// Canonicalized on read as well as on write, so a pointer written before
/// canonicalization existed still resolves to the same pool identity.
pub fn main_root_from_workspace_root(ws_root: &Path) -> Result<PathBuf, GitError> {
    let repo_path = ws_root.join(".jj").join("repo");
    let meta = std::fs::metadata(&repo_path).map_err(|e| {
        GitError::new(
            "read .jj/repo pointer",
            format!("cannot inspect {}: {e}", repo_path.display()),
            GitErrorKind::Other,
        )
    })?;
    if meta.is_dir() {
        return Ok(canonicalize(ws_root));
    }
    let contents = std::fs::read_to_string(&repo_path).map_err(|e| {
        GitError::new(
            "read .jj/repo pointer",
            format!("cannot read {}: {e}", repo_path.display()),
            GitErrorKind::Other,
        )
    })?;
    let store = contents.trim();
    let store_path = if Path::new(store).is_absolute() {
        PathBuf::from(store)
    } else {
        ws_root.join(".jj").join(store)
    };
    // store_path is <main>/.jj/repo; the main root is two levels up.
    let main_root = store_path
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .ok_or_else(|| {
            GitError::new(
                "read .jj/repo pointer",
                format!("{} does not name a repository store", repo_path.display()),
                GitErrorKind::Other,
            )
        })?;
    Ok(canonicalize(&main_root))
}

/// Whether `detail` reads like a failure to reach or use the origin remote, in
/// jj's vocabulary (Go `jjvcs.IsOriginAccessError`).
///
/// jj shells out to git for network transport, so its errors WRAP git's
/// ("External git program failed" around "unable to access" / "Could not
/// resolve host"); a local-path remote fails with jj's own "Could not find
/// repository at". Patterns captured from real `jj git fetch` runs against
/// unreachable remotes.
///
/// Classification is by CONTENT, never by which backend produced the error: the
/// error already happened, and each backend owns its own vocabulary. Callers
/// use it to emit a category-tagged SKIP, never a deletion.
pub fn is_origin_access_error(detail: &str) -> bool {
    detail.contains("External git program failed")
        || detail.contains("unable to access")
        || detail.contains("Could not resolve host")
        || detail.contains("Could not find repository at")
}

// ─── The backend ──────────────────────────────────────────────────────────────

/// [`GitBackend`] for Jujutsu repositories (Go `jjvcs.Backend`).
///
/// Every jj invocation goes through `runner`, so the backend can be exercised
/// end-to-end against a scripted runner on a machine with no `jj` binary.
pub struct JjBackend {
    runner: Arc<dyn JjRunner>,
}

impl std::fmt::Debug for JjBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JjBackend").finish_non_exhaustive()
    }
}

impl JjBackend {
    /// Builds the backend against the discovered `jj` binary.
    ///
    /// Infallible by design: a missing `jj` must not remove the backend from the
    /// registry, because a `.jj` marker with no registered backend reports "no
    /// backend is registered for jj" — a configuration-shaped error for what is
    /// a missing-binary problem. Instead every command fails with
    /// `GitErrorKind::NotFound` and the true cause.
    ///
    /// # Coverage
    ///
    /// `jj` is not installed on every machine, so this backend is covered by
    /// UNIT tests against a fake runner: argument construction, marker and
    /// flavor resolution, the fail-closed refusals, and the clean/dirty reset
    /// protocol are all exercised. **No end-to-end run against a real jj
    /// repository is performed here**, and jj's own output formats are therefore
    /// unverified by this crate's test suite.
    pub fn discover() -> Self {
        Self {
            runner: Arc::new(RealJjRunner::discover()),
        }
    }

    /// Builds the backend against a known `jj` binary.
    pub fn with_bin(bin: PathBuf) -> Self {
        Self {
            runner: Arc::new(RealJjRunner { bin: Some(bin) }),
        }
    }

    /// Builds the backend against a scripted [`JjRunner`].
    ///
    /// Exists so the seam's own tests can assert that a `.jj` selection is
    /// dispatched to jj — the one claim that cannot be checked on a machine
    /// without a jj binary, and the one a raw git fatal used to hide.
    #[cfg(test)]
    pub(crate) fn with_runner(runner: Arc<dyn JjRunner>) -> Self {
        Self { runner }
    }

    /// Runs `jj --color never <args>` in `cwd`, returning trimmed stdout.
    ///
    /// `--color never` is prepended HERE rather than in each runner so the argv a
    /// test asserts is byte-for-byte the argv jj receives. Without it a jj
    /// invoked from a TTY emits escape sequences into every template and every
    /// commit id this backend parses.
    fn out(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        let mut full: Vec<&str> = Vec::with_capacity(args.len() + 2);
        full.push("--color");
        full.push("never");
        full.extend_from_slice(args);
        self.runner.run(cwd, &full)
    }

    /// Whether the revset resolves to at least one commit (Go `revsetNonEmpty`).
    ///
    /// A jj failure is `false`, never an error: every caller uses this as a
    /// yes/no test whose "no" is already a valid answer, and an unreadable
    /// revset must not abort a decision the caller can safely make the other
    /// way.
    fn revset_non_empty(&self, dir: &Path, revset: &str) -> bool {
        self.out(dir, &["log", "-r", revset, "--no-graph", "-T", COMMIT_ID_TEMPLATE])
            .map(|o| !o.is_empty())
            .unwrap_or(false)
    }

    /// Whether `a` is an ancestor of (or equal to) `b` (Go `isAncestor`).
    fn is_ancestor(&self, dir: &Path, a: &str, b: &str) -> bool {
        self.revset_non_empty(dir, &format!("({a}) & ::({b})"))
    }

    /// The working-copy commit id of `worktree` (Go `worktreeHead`).
    fn worktree_head(&self, worktree: &Path) -> Result<String, GitError> {
        let out = self.out(
            worktree,
            &["log", "-r", "@", "--no-graph", "-T", COMMIT_ID_TEMPLATE],
        )?;
        first_commit_id(&out, "the working-copy commit")
    }

    /// Resolves `branch` to the immutable commit a reset must target (Go
    /// `resolveResetRef`).
    ///
    /// The bookmark is pinned to a commit id HERE, before anything destructive.
    /// jj bookmarks are mutable: a reset built from the NAME would target
    /// whatever the bookmark had become by the time the command ran, which is
    /// exactly what the seam's `ResetGuard` pair exists to prevent on the git
    /// side.
    fn resolve_reset_ref(&self, worktree: &Path, branch: &str) -> Result<String, GitError> {
        // A sibling workspace may have moved the repository since this one was
        // last used; recover first so the commands below see current state.
        // Best-effort: a stale repo is not itself a failure here.
        let _ = self.out(worktree, &["workspace", "update-stale"]);
        let reference = self.branch_ref(&JjRepo::at(worktree), branch);
        if reference.is_empty() {
            return Err(GitError::new(
                "jj workspace reset",
                format!("cannot resolve bookmark {branch:?}: the name cannot be expressed as a jj revset"),
                GitErrorKind::MergeRefUnresolvable,
            ));
        }
        let out = self.out(
            worktree,
            &["log", "-r", &reference, "--no-graph", "-T", COMMIT_ID_TEMPLATE],
        )?;
        first_commit_id(&out, &format!("bookmark {branch:?} to a commit"))
    }

    /// Whether the workspace sits directly on `reference` with a clean working
    /// copy (Go `parkedOnRef`).
    ///
    /// This is the post-condition of `jj rebase -d <ref>`: the working copy is
    /// `@`, its parent is `@-`, and `@-` must be the reset target. A dirty
    /// workspace is never "parked" — its parent could match while uncommitted
    /// changes sit in `@`.
    fn parked_on_ref(&self, worktree: &Path, reference: &str) -> Result<bool, GitError> {
        if self.is_dirty(worktree)? {
            return Ok(false);
        }
        let out = self.out(
            worktree,
            &["log", "-r", "@-", "--no-graph", "-T", COMMIT_ID_TEMPLATE],
        )?;
        let parent = out.lines().next().unwrap_or("").trim();
        Ok(!parent.is_empty() && parent == reference)
    }

    /// The `@` pinned to the commit the safety check observed (Go `revset`).
    ///
    /// Pinned BY COMMIT ID rather than positionally, which is what makes the
    /// reset safe without a lock: jj always commits, so a concurrent change of
    /// `@` leaves this revset EMPTY rather than pointing at whatever `@` has
    /// become. The next command then either does nothing or is caught by the
    /// post-condition check.
    fn pinned_revset(expected_head: &str) -> String {
        format!("@ & commit_id(\"{expected_head}\")")
    }

    /// Refuses a path that is not a jj workspace (upstream #110, the reason
    /// jj gets its own fail-closed marker rule at all).
    ///
    /// A jj workspace has NO `.git` marker. Any git-shaped fallback therefore
    /// resolves the path by walking UP — and inside a COLOCATED repository that
    /// walk lands on the user's own working tree, where a rewrite is
    /// irreversible data loss. Go hardens every destructive entry point in
    /// `destructiveBackendForWorktree` for exactly this; this is the same
    /// guarantee stated in jj's own terms, checked before a single jj command
    /// runs.
    fn require_jj_workspace(path: &Path) -> Result<(), GitError> {
        match crate::vcs::worktree_backend_name(path)? {
            Some(BACKEND_JJ) => Ok(()),
            Some(BACKEND_GIT) => Err(GitError::new(
                "jj workspace",
                format!(
                    "refusing to run jj in {}: it is a git worktree, not a jj workspace",
                    path.display()
                ),
                GitErrorKind::Other,
            )),
            Some(other) => Err(GitError::new(
                "jj workspace",
                format!(
                    "refusing to run jj in {}: it is a {other} worktree",
                    path.display()
                ),
                GitErrorKind::Other,
            )),
            None => Err(GitError::new(
                "jj workspace",
                format!(
                    "refusing to modify {}: it holds no .git or .jj marker",
                    path.display()
                ),
                GitErrorKind::Other,
            )),
        }
    }

    /// The clean-workspace half of the guarded reset (Go `ResetWorktreeToRef`'s
    /// `else` branch).
    ///
    /// `jj rebase -d <ref> -r <pinned revset>` is ONE command, so it loads ONE
    /// snapshot: a concurrent move of `@` empties the revset and the rebase
    /// becomes a no-op rather than a wrong rewrite. The post-condition is then
    /// checked explicitly, because a no-op and a success are indistinguishable
    /// in jj's exit status — and reporting "reset succeeded" for a workspace
    /// that did not move is the failure this whole protocol exists to prevent.
    fn reset_clean(
        &self,
        worktree: &Path,
        reset_ref: &str,
        expected_head: &str,
    ) -> Result<(), GitError> {
        let revset = Self::pinned_revset(expected_head);
        if let Err(e) = self.out(worktree, &["rebase", "-d", reset_ref, "-r", &revset]) {
            // A rebase onto the commit @ already sits on is a no-op jj may
            // report as an error. That is only a SUCCESS if the workspace really
            // is parked on the target and @ never moved.
            if let Ok(true) = self.parked_on_ref(worktree, reset_ref)
                && let Ok(head) = self.worktree_head(worktree)
                && head == expected_head
            {
                return Ok(());
            }
            return Err(e);
        }
        if !self.parked_on_ref(worktree, reset_ref)? {
            return Err(self::head_moved(expected_head));
        }
        Ok(())
    }

    /// The dirty-workspace half of the guarded reset (Go's abandon path).
    ///
    /// `jj abandon` drops the working-copy commit and `jj new <ref>` recreates a
    /// fresh empty one on the target. Unlike `reset --hard` this stays
    /// recoverable through `jj op restore` — the abandoned work is out of view,
    /// not destroyed. Every step is verified before the next runs, so a
    /// concurrent commit that made the pinned revset non-empty again, or left
    /// the workspace dirty or unmerged, stops the reset instead of being
    /// discarded.
    fn reset_dirty(&self, worktree: &Path, reset_ref: &str, expected_head: &str) -> Result<(), GitError> {
        let revset = Self::pinned_revset(expected_head);
        self.out(worktree, &["abandon", "-r", &revset])?;
        // `@` still matches the pinned commit: it moved, or never was what the
        // safety check measured.
        if self.revset_non_empty(worktree, &revset) {
            return Err(head_moved(expected_head));
        }
        if self.is_dirty(worktree)? {
            return Err(head_moved(expected_head));
        }
        // The workspace must be merged into the reset target, or `jj new` would
        // strand unlanded work as the parent of a fresh commit.
        if !self.is_head_merged_into_ref(worktree, reset_ref)? {
            return Err(head_moved(expected_head));
        }
        self.out(worktree, &["new", reset_ref]).map(|_| ())
    }
}

/// The "HEAD moved" refusal, worded once so a log reader can match it.
fn head_moved(expected_head: &str) -> GitError {
    GitError::new(
        "jj workspace reset",
        format!("worktree HEAD changed since safety check: was {expected_head}"),
        GitErrorKind::Other,
    )
}

/// The revset named by [`head_moved`]'s condition, when the cause is a revset
/// that cannot be expressed — reported instead of a generic jj failure so the
/// operator sees the input problem rather than jj's parse error.
fn unexpressible_ref(what: &str) -> GitError {
    GitError::new(
        "jj revset",
        format!("{what} cannot be expressed as a jj revset and was refused"),
        GitErrorKind::MergeRefUnresolvable,
    )
}

/// A [`GitRepo`] view of a jj workspace.
struct JjRepo;

impl JjRepo {
    /// jj commands run in `dir`, which the trait reaches as `common_dir`.
    ///
    /// jj has no separate "common dir" — one store is shared by every workspace,
    /// so any of them can answer a repository-wide question. The caller picks
    /// which: the MAIN root for operations against the repository as a whole
    /// (remote listing, workspace creation), or a workspace when the answer is
    /// the same either way (bookmark probes during a reset).
    fn at(dir: &Path) -> GitRepo {
        GitRepo {
            common_dir: dir.to_path_buf(),
            worktree: None,
        }
    }
}

impl GitBackend for JjBackend {
    fn name(&self) -> &'static str {
        BACKEND_JJ
    }

    fn repo_root(&self, start: &Path) -> Result<PathBuf, GitError> {
        // jj prints a physical path today, but its output form is not
        // contractual; canonicalize so every root-resolution route agrees.
        Ok(canonicalize(&PathBuf::from(self.out(start, &["workspace", "root"])?)))
    }

    fn main_repo_root(&self, start: &Path) -> Result<PathBuf, GitError> {
        let ws_root = self.repo_root(start)?;
        main_root_from_workspace_root(&ws_root)
    }

    fn default_branch(&self, repo: &GitRepo) -> Result<String, GitError> {
        let root = &repo.common_dir;
        // Remote bookmarks first: a freshly cloned repository's local `main` can
        // be stale or missing entirely.
        if self.has_remote(repo, "origin") {
            for name in DEFAULT_BRANCH_CANDIDATES {
                if self.revset_non_empty(root, &format!("present({name}@origin)")) {
                    return Ok(name.to_string());
                }
            }
        }
        for name in DEFAULT_BRANCH_CANDIDATES {
            if self.revset_non_empty(root, &format!("present({name})")) {
                return Ok(name.to_string());
            }
        }
        Err(GitError::new(
            "jj default branch",
            format!(
                "cannot determine default bookmark: expected one of {} locally or on origin; try 'jj git fetch' or 'jj bookmark create main'",
                DEFAULT_BRANCH_CANDIDATES.join(", ")
            ),
            GitErrorKind::DefaultBranchUnresolvable,
        ))
    }

    fn has_remote(&self, repo: &GitRepo, name: &str) -> bool {
        self.remotes(repo).is_some_and(|r| r.iter().any(|(n, _)| n == name))
    }

    fn remote_url(&self, repo: &GitRepo, name: &str) -> Option<String> {
        self.remotes(repo)?
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, url)| url)
    }

    fn fetch(&self, repo: &GitRepo) -> Result<(), GitError> {
        if !self.has_remote(repo, "origin") {
            return Ok(());
        }
        self.out(&repo.common_dir, &["git", "fetch", "--remote", "origin"])
            .map(|_| ())
            .map_err(|e| {
                // Prune turns this kind into a category-tagged SKIP; every other
                // jj failure stays "other" so it is never mistaken for one.
                let kind = if is_origin_access_error(&e.message) {
                    GitErrorKind::OriginUnreachable
                } else {
                    e.kind
                };
                GitError::new(e.command, e.message, kind)
            })
    }

    fn worktree_add(&self, repo: &GitRepo, path: &Path, branch: &str) -> Result<(), GitError> {
        let abs = absolute(path)?;
        let name = workspace_name_for(&abs);
        let root = &repo.common_dir;

        // Best effort: forgetting a name that does not exist is a no-op in jj.
        // It is the prune equivalent for a RE-USED path — jj does not record
        // workspace directories, so this is the only way a stale registration
        // at this path can be cleared.
        let _ = self.out(root, &["workspace", "forget", &name]);

        let reference = self.branch_ref(repo, branch);
        if reference.is_empty() {
            return Err(unexpressible_ref(&format!("bookmark {branch:?}")));
        }
        self.out(
            root,
            &[
                "workspace",
                "add",
                "--name",
                &name,
                "--revision",
                &reference,
                &abs.to_string_lossy(),
            ],
        )?;
        make_repo_pointer_absolute(&abs)
    }

    fn worktree_remove(&self, repo: &GitRepo, path: &Path) -> Result<(), GitError> {
        let abs = absolute(path)?;
        // Refuse to delete a directory that exists but is not a jj workspace.
        // Removing a git worktree here would delete its files while leaving the
        // `.git/worktrees` registration stale. A path that is already gone is
        // fine and still gets its stale registration forgotten below.
        if abs.exists() {
            let jj_dir = abs.join(".jj");
            if !jj_dir.is_dir() {
                return Err(GitError::new(
                    "jj workspace remove",
                    format!("refusing to remove {}: not a jj workspace", abs.display()),
                    GitErrorKind::Other,
                ));
            }
            // A main workspace's `.jj/repo` IS the store, not a pointer file.
            // Forgetting the digest-named registration would no-op and the
            // recursive delete would destroy the entire repository.
            if abs.join(".jj").join("repo").is_dir() {
                return Err(GitError::new(
                    "jj workspace remove",
                    format!(
                        "refusing to remove {}: main jj workspace, not a pooled secondary workspace",
                        abs.display()
                    ),
                    GitErrorKind::Other,
                ));
            }
        }
        let _ = self.out(
            &repo.common_dir,
            &["workspace", "forget", &workspace_name_for(&abs)],
        );
        std::fs::remove_dir_all(&abs).map_err(|e| {
            GitError::new(
                "jj workspace remove",
                format!("removing {}: {e}", abs.display()),
                GitErrorKind::Other,
            )
        })
    }

    fn remove_clean_worktree(&self, repo: &GitRepo, path: &Path) -> Result<(), GitError> {
        if self.is_dirty(path)? {
            return Err(GitError::new(
                "jj workspace remove",
                format!("workspace at {} has local changes", path.display()),
                GitErrorKind::Other,
            ));
        }
        self.worktree_remove(repo, path)
    }

    fn create_branch(&self, worktree: &Path, branch: &str) -> Result<(), GitError> {
        // jj bookmarks have different ownership semantics — several workspaces
        // share one store and would all move together — so branch creation is
        // deliberately Git-only, exactly as in Go's `vcs.CreateBranch`.
        let _ = branch;
        let found = crate::vcs::worktree_backend_name(worktree)?;
        Err(GitError::new(
            "create branch",
            format!(
                "cannot create branch in {}: branch creation requires a git worktree{}",
                worktree.display(),
                match found {
                    Some(name) => format!(", found {name}"),
                    None => ", worktree has no .git marker".to_string(),
                }
            ),
            GitErrorKind::Other,
        ))
    }

    fn local_branch_exists(&self, repo: &GitRepo, branch: &str) -> bool {
        if !revset_atom(branch) {
            return false;
        }
        self.revset_non_empty(&repo.common_dir, &format!("present({branch})"))
    }

    fn is_dirty(&self, worktree: &Path) -> Result<bool, GitError> {
        let out = self.out(
            worktree,
            &["log", "-r", "@", "--no-graph", "-T", DIRTY_TEMPLATE],
        )?;
        Ok(out != "clean")
    }

    fn reset_worktree(&self, worktree: &Path, branch: &str) -> Result<(), GitError> {
        Self::require_jj_workspace(worktree)?;
        let reference = self.resolve_reset_ref(worktree, branch)?;
        let head = self.worktree_head(worktree)?;
        self.reset_worktree_to_ref(worktree, &reference, &head, false)
    }

    fn detach_worktree(&self, worktree: &Path) -> Result<(), GitError> {
        // jj working copies are anonymous commits and never hold a bookmark the
        // way a git worktree holds a branch, so there is nothing to release.
        // The marker check still runs: the trait documents every rewrite
        // operation as fail-closed, and a future jj version that does attach a
        // bookmark must inherit that guarantee rather than quietly lose it
        // behind a no-op.
        Self::require_jj_workspace(worktree)
    }

    fn is_head_merged_into_ref(&self, worktree: &Path, reference: &str) -> Result<bool, GitError> {
        if !revset_atom(reference) {
            return Err(unexpressible_ref(&format!("reference {reference:?}")));
        }
        // Every PARENT of the working-copy commit is an ancestor of `reference`.
        // `@` itself is excluded: a clean `@` is empty and a non-empty one is
        // already reported by `is_dirty`. Ancestry is the only proof used, so a
        // squash-merged head reads as UNMERGED — which fails safe, because the
        // destructive callers then refuse to delete it.
        Ok(self
            .out(
                worktree,
                &[
                    "log",
                    "-r",
                    &format!("@- & ~::({reference})"),
                    "--no-graph",
                    "-T",
                    COMMIT_ID_TEMPLATE,
                ],
            )?
            .is_empty())
    }

    fn default_branch_merge_ref(&self, repo: &GitRepo) -> Result<String, GitError> {
        let branch = self.default_branch(repo)?;
        let root = &repo.common_dir;
        let unavailable = |r: &str| {
            GitError::new(
                "jj merge ref",
                format!("{r} is unavailable"),
                GitErrorKind::MergeRefUnresolvable,
            )
        };
        if self.has_remote(repo, "origin") {
            let remote = format!("{branch}@origin");
            if !self.revset_non_empty(root, &format!("present({remote})")) {
                return Err(unavailable(&remote));
            }
            return Ok(remote);
        }
        if !self.revset_non_empty(root, &format!("present({branch})")) {
            return Err(unavailable(&branch));
        }
        Ok(branch)
    }

    fn branch_ref(&self, repo: &GitRepo, branch: &str) -> String {
        // The trait has no error channel here, so an inexpressible name returns
        // EMPTY rather than an unvalidated string. Every consumer refuses an
        // empty reference before it can reach a jj command — an empty revision
        // matches nothing, and refusing is the direction that cannot surprise.
        if !revset_atom(branch) {
            return String::new();
        }
        let root = &repo.common_dir;
        let remote = format!("{branch}@origin");
        let has_local = self.revset_non_empty(root, &format!("present({branch})"));
        let has_remote = self.revset_non_empty(root, &format!("present({remote})"));

        match (has_local, has_remote) {
            (true, true) => {
                // Freshest wins, matching the git backend: the strictly-ahead
                // ref, and on divergence origin.
                if self.is_ancestor(root, branch, &remote) {
                    remote
                } else if self.is_ancestor(root, &remote, branch) {
                    branch.to_string()
                } else {
                    remote
                }
            }
            (true, false) => branch.to_string(),
            (false, true) => remote,
            // Neither exists. The local name is returned so the caller reports
            // "bookmark not found" against the bookmark the user actually named,
            // instead of a second name they never mentioned.
            (false, false) => branch.to_string(),
        }
    }

    fn common_git_dir(&self, start: &Path) -> Result<PathBuf, GitError> {
        // A colocated repository keeps `.git/info/exclude` at its main root. A
        // non-colocated one has no usable git dir; callers degrade gracefully on
        // error, since an absent exclude file is normal rather than a failure.
        let main_root = self.main_repo_root(start)?;
        let git_dir = main_root.join(".git");
        if git_dir.is_dir() {
            Ok(git_dir)
        } else {
            Err(GitError::new(
                "jj common git dir",
                format!(
                    "jj repository at {} is not colocated: no .git directory",
                    main_root.display()
                ),
                GitErrorKind::Other,
            ))
        }
    }

    fn is_worktree_safe_to_reset(
        &self,
        worktree: &Path,
        branch: &str,
    ) -> Result<ResetGuard, GitError> {
        Self::require_jj_workspace(worktree)?;
        // Both ids travel together in the guard, and the caller must hand BOTH
        // to `reset_worktree_to_ref`. An unresolvable target or an unreadable
        // `@` is an `Err`, which callers treat as "not safe"; `safe == false` is
        // a real answer they may weigh.
        let reset_ref = self.resolve_reset_ref(worktree, branch)?;
        let head = self.worktree_head(worktree)?;
        let safe = self.is_head_merged_into_ref(worktree, &reset_ref)?;
        Ok(ResetGuard {
            safe,
            reset_ref,
            head,
        })
    }

    fn reset_worktree_to_ref(
        &self,
        worktree: &Path,
        reset_ref: &str,
        expected_head: &str,
        require_clean: bool,
    ) -> Result<(), GitError> {
        Self::require_jj_workspace(worktree)?;

        // A sibling workspace may have moved the repository since this one was
        // last used; recover before reading anything.
        let _ = self.out(worktree, &["workspace", "update-stale"]);

        // `expected_head` is interpolated into a revset, so it is validated as a
        // jj commit id before it is used as one. `reset_ref` is validated too —
        // Go checks only `expectedHead`, and an unvalidated `ref` reaches
        // `jj rebase -d` / `jj new` where it is a revset, not an opaque id.
        if !is_commit_id(expected_head) {
            return Err(GitError::new(
                "jj workspace reset",
                "worktree reset requires a resolved working-copy commit",
                GitErrorKind::Other,
            ));
        }
        if !is_commit_id(reset_ref) {
            return Err(GitError::new(
                "jj workspace reset",
                "worktree reset requires a resolved commit id",
                GitErrorKind::Other,
            ));
        }

        let dirty = self.is_dirty(worktree)?;
        if !dirty {
            return self.reset_clean(worktree, reset_ref, expected_head);
        }
        if require_clean {
            // Re-checked here, not trusted from the caller's own check: work
            // that landed in between must not be discarded.
            return Err(GitError::new(
                "jj workspace reset",
                "worktree became dirty after safety check",
                GitErrorKind::Other,
            ));
        }
        self.reset_dirty(worktree, reset_ref, expected_head)
    }
}

impl JjBackend {
    /// `jj git remote list` parsed into `(name, url)` pairs.
    ///
    /// Parsed lazily by [`JjBackend::has_remote`]/[`JjBackend::remote_url`]
    /// rather than cached: a remote added mid-process must be visible without a
    /// backend rebuild, and the listing is one cheap command.
    fn remotes(&self, repo: &GitRepo) -> Option<Vec<(String, String)>> {
        let out = self
            .out(&repo.common_dir, &["git", "remote", "list"])
            .ok()?;
        Some(
            out.lines()
                .filter_map(|line| {
                    let mut fields = line.split_whitespace();
                    let name = fields.next()?;
                    let url = fields.next()?;
                    Some((name.to_string(), url.to_string()))
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A scripted jj. Every invocation is appended to `calls` with the argv
    /// AFTER `--color never` is stripped, so an assertion reads as the jj
    /// subcommand a human would type.
    struct FakeJj {
        calls: Mutex<Vec<Vec<String>>>,
        /// Answers consumed in order, one per invocation.
        script: Mutex<Vec<Result<String, String>>>,
        /// The answer used once the script is exhausted — empty stdout, which is
        /// what jj prints for a revset that matches nothing.
        default: Mutex<Result<String, String>>,
    }

    impl FakeJj {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(Vec::new()),
                default: Mutex::new(Ok(String::new())),
            }
        }

        /// Queues answers for the next invocations. An `Err` becomes a jj
        /// failure whose message is the simulated stderr.
        fn then(mut self, answers: &[Result<&str, &str>]) -> Self {
            // `get_mut` rather than `lock`, so no guard is held across the move
            // back to the caller.
            let queued: Vec<Result<String, String>> = answers
                .iter()
                .map(|a| match a {
                    Ok(s) => Ok((*s).to_string()),
                    Err(e) => Err((*e).to_string()),
                })
                .collect();
            self.script.get_mut().unwrap().extend(queued);
            self
        }

        /// Sets the answer used once the script is exhausted.
        fn then_default(mut self, answer: Result<&str, &str>) -> Self {
            *self.default.get_mut().unwrap() = match answer {
                Ok(s) => Ok(s.to_string()),
                Err(e) => Err(e.to_string()),
            };
            self
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        /// Whether any recorded call starts with `prefix`.
        fn called(&self, prefix: &[&str]) -> bool {
            self.calls().iter().any(|c| c.len() >= prefix.len() && &c[..prefix.len()] == prefix)
        }
    }

    impl JjRunner for FakeJj {
        fn run(&self, _cwd: &Path, args: &[&str]) -> Result<String, GitError> {
            assert_eq!(
                args.first().copied(),
                Some("--color"),
                "every jj invocation must be --color never first"
            );
            self.calls
                .lock()
                .unwrap()
                .push(args[2..].iter().map(|s| (*s).to_string()).collect());
            let mut script = self.script.lock().unwrap();
            let answer = if script.is_empty() {
                self.default.lock().unwrap().clone()
            } else {
                script.remove(0)
            };
            match answer {
                Ok(s) => Ok(s),
                Err(stderr) => Err(GitError::new(
                    format!("jj {}", args[2..].join(" ")),
                    stderr,
                    GitErrorKind::Other,
                )),
            }
        }
    }

    fn backend(fake: Arc<FakeJj>) -> JjBackend {
        JjBackend {
            runner: fake,
        }
    }

    /// A directory carrying a `.jj` marker, so the fail-closed marker check
    /// passes. Real jj is never invoked against it — every test supplies a
    /// scripted runner.
    fn jj_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join(".jj")).unwrap();
        (dir, root)
    }

    /// A main workspace at `<tmp>/main` holding the store, and a secondary
    /// workspace at `<tmp>/wt-1` pointing at it exactly as jj writes it: a
    /// path RELATIVE to the workspace's own `.jj` directory.
    ///
    /// Everything lives inside the tempdir. An earlier draft built the main
    /// root by walking out of the tempdir with `..`, and two tests then shared
    /// one directory in the system temp root — so a `.git` created by one was
    /// visible to the other.
    fn main_and_workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("main");
        std::fs::create_dir_all(main.join(".jj").join("repo")).unwrap();
        let ws = d.path().join("wt-1");
        std::fs::create_dir_all(ws.join(".jj")).unwrap();
        std::fs::write(ws.join(".jj").join("repo"), "../../main/.jj/repo").unwrap();
        (d, main, ws)
    }

    /// The jj probes that precede resolving a LOCAL-ONLY bookmark `main`:
    /// `workspace update-stale`, `present(main)`, `present(main@origin)`.
    ///
    /// Every operation that resolves a branch goes through `branch_ref`, so
    /// most scripts in this file open with it. Factored out because counting
    /// those probes by hand is how a test ends up asserting against the wrong
    /// invocation.
    fn local_only_main() -> [Result<&'static str, &'static str>; 3] {
        [Ok(""), Ok(SHA), Ok("")]
    }

    /// Forty hex characters: a syntactically valid jj commit id.
    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // ── workspace naming and store pointers ─────────────────────────────────

    #[test]
    fn the_workspace_name_is_a_path_keyed_digest() {
        let name = workspace_name_for(Path::new("/tmp/pool/wt-1"));
        assert!(name.starts_with("th-"), "got: {name}");
        assert_eq!(name.len(), 3 + 32, "th- plus 16 bytes of hex");
        assert!(name[3..].bytes().all(|b| b.is_ascii_hexdigit()));
        // Stable across calls, and distinct paths never collide — which is what
        // lets `workspace add` forget a stale registration by name.
        assert_eq!(name, workspace_name_for(Path::new("/tmp/pool/wt-1")));
        assert_ne!(name, workspace_name_for(Path::new("/tmp/pool/wt-2")));
    }

    #[test]
    fn a_main_workspace_resolves_to_itself() {
        // `.jj/repo` is a DIRECTORY here: the store lives in this workspace.
        let (_d, root) = jj_dir();
        std::fs::create_dir(root.join(".jj").join("repo")).unwrap();
        assert_eq!(main_root_from_workspace_root(&root).unwrap(), canonicalize(&root));
    }

    #[test]
    fn a_relative_store_pointer_resolves_through_the_workspace() {
        // Relative, as jj writes it: the pointer breaks when the pool directory
        // and the repository do not move together, which is the normal case.
        let (_d, main, ws) = main_and_workspace();

        assert_eq!(
            main_root_from_workspace_root(&ws).unwrap(),
            canonicalize(&main),
            "a relative pointer must resolve to the same root regardless of where it is written"
        );
    }

    #[test]
    fn an_absolute_store_pointer_is_honoured() {
        let (d, ws) = jj_dir();
        let main = d.path().join("main");
        std::fs::create_dir_all(main.join(".jj").join("repo")).unwrap();
        std::fs::write(
            ws.join(".jj").join("repo"),
            main.join(".jj").join("repo").to_string_lossy().as_bytes(),
        )
        .unwrap();

        assert_eq!(
            main_root_from_workspace_root(&ws).unwrap(),
            canonicalize(&main)
        );
    }

    #[test]
    fn a_workspace_without_a_store_pointer_is_an_error() {
        let (_d, root) = jj_dir();
        let err = main_root_from_workspace_root(&root)
            .expect_err("a .jj directory with no repo pointer is damaged, not a main workspace");
        assert!(err.message.contains("cannot inspect"), "got: {}", err.message);
    }

    #[test]
    fn a_relative_store_pointer_is_rewritten_absolute() {
        let (_d, main, ws) = main_and_workspace();

        make_repo_pointer_absolute(&ws).unwrap();

        let written = std::fs::read_to_string(ws.join(".jj").join("repo")).unwrap();
        let written = PathBuf::from(written.trim());
        assert!(written.is_absolute(), "got: {written:?}");
        assert_eq!(main_root_from_workspace_root(&ws).unwrap(), canonicalize(&main));
    }

    #[test]
    fn an_absolute_store_pointer_is_left_alone() {
        let (d, ws) = jj_dir();
        let main = d.path().join("main");
        std::fs::create_dir_all(main.join(".jj").join("repo")).unwrap();
        let pointer = main.join(".jj").join("repo");
        std::fs::write(ws.join(".jj").join("repo"), pointer.to_string_lossy().as_bytes()).unwrap();

        make_repo_pointer_absolute(&ws).unwrap();

        assert_eq!(std::fs::read_to_string(ws.join(".jj").join("repo")).unwrap(), pointer.to_string_lossy());
    }

    // ── markerless fail-closed (upstream #110) ───────────────────────────────

    #[test]
    fn a_markerless_path_is_refused_before_any_jj_command_runs() {
        // THE point of a jj-specific marker rule: a jj workspace has no `.git`,
        // so a git-shaped fallback walks UP. Inside a colocated repository that
        // walk lands on the user's own working tree.
        let (_d, root) = jj_dir();
        let slot = root.join("markerless");
        std::fs::create_dir_all(&slot).unwrap();
        let fake = Arc::new(FakeJj::new().then_default(Ok("")));
        let b = backend(fake.clone());

        let err = b
            .reset_worktree_to_ref(&slot, SHA, SHA, false)
            .expect_err("a markerless path must be refused");
        assert!(err.message.contains("no .git or .jj marker"), "got: {}", err.message);
        assert!(fake.calls().is_empty(), "the refusal must precede every jj call");
    }

    #[test]
    fn a_git_worktree_is_refused_by_the_jj_backend() {
        // The inverse misroute: git commands in a jj workspace would run in a
        // non-git tree and report the failure in terms that hide the cause.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let b = backend(Arc::new(FakeJj::new().then_default(Ok(""))));

        let err = b.detach_worktree(dir.path()).expect_err("a git worktree is not a jj workspace");
        assert!(err.message.contains("git worktree"), "got: {}", err.message);
    }

    #[test]
    fn a_git_worktree_nested_in_a_jj_workspace_is_still_refused() {
        // The enclosing `.jj` must NOT rescue a slot that carries its own git
        // marker: markers are read from the path itself, never by walking up.
        let (_d, root) = jj_dir();
        let slot = root.join("slot");
        std::fs::create_dir_all(slot.join(".git")).unwrap();
        let b = backend(Arc::new(FakeJj::new().then_default(Ok(""))));

        assert!(b.reset_worktree(&slot, "main").is_err());
    }

    // ── argument construction ────────────────────────────────────────────────

    #[test]
    fn every_invocation_is_color_never() {
        // Without it a jj run from a TTY puts escape sequences into every
        // commit id and template result this backend parses.
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then_default(Ok("clean")));
        backend(fake.clone()).is_dirty(&root).unwrap();
        assert_eq!(fake.calls(), vec![vec![
            "log".to_string(),
            "-r".to_string(),
            "@".to_string(),
            "--no-graph".to_string(),
            "-T".to_string(),
            DIRTY_TEMPLATE.to_string(),
        ]]);
    }

    #[test]
    fn dirtiness_asks_jj_whether_the_working_copy_commit_is_empty() {
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[Ok("clean"), Ok("dirty")]));
        let b = backend(fake.clone());

        assert!(!b.is_dirty(&root).unwrap());
        assert!(b.is_dirty(&root).unwrap());
        assert!(fake.called(&["log", "-r", "@", "--no-graph"]));
    }

    #[test]
    fn merge_safety_excludes_the_working_copy_commit_itself() {
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[Ok(""), Ok("c0ffee")]));
        let b = backend(fake.clone());

        assert!(b.is_head_merged_into_ref(&root, "main@origin").unwrap());
        assert!(!b.is_head_merged_into_ref(&root, "main@origin").unwrap());
        assert_eq!(
            fake.calls()[0][2],
            "@- & ~::(main@origin)",
            "every PARENT must be an ancestor of the ref; @ is excluded"
        );
    }

    #[test]
    fn a_remote_listing_is_parsed_by_column() {
        let (_d, root) = jj_dir();
        let fake = Arc::new(
            FakeJj::new().then_default(Ok("origin git@github.com:o/r.git\nupstream git@github.com:o/u.git")),
        );
        let repo = JjRepo::at(&root);
        let b = backend(fake.clone());

        assert!(b.has_remote(&repo, "origin"));
        assert!(b.has_remote(&repo, "upstream"));
        assert!(!b.has_remote(&repo, "fork"));
        assert_eq!(b.remote_url(&repo, "origin").as_deref(), Some("git@github.com:o/r.git"));
        assert_eq!(b.remote_url(&repo, "fork"), None);
        assert!(fake.called(&["git", "remote", "list"]));
    }

    #[test]
    fn a_workspace_is_added_by_name_and_revision_then_its_pointer_is_absolute() {
        let (d, _root) = jj_dir();
        let main = d.path().join("main");
        std::fs::create_dir_all(main.join(".jj").join("repo")).unwrap();
        let target = d.path().join("pool").join("wt-1");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::create_dir_all(target.join(".jj")).unwrap();
        std::fs::write(target.join(".jj").join("repo"), "../../main/.jj/repo").unwrap();

        let name = workspace_name_for(&target);
        let target_arg = target.to_string_lossy().to_string();
        let fake = Arc::new(FakeJj::new().then(&local_only_main()).then_default(Ok(SHA)));
        let b = backend(fake.clone());
        b.worktree_add(&JjRepo::at(&main), &target, "main").unwrap();

        let calls = fake.calls();
        assert_eq!(calls[0], vec!["workspace", "forget", &name], "the stale same-path registration must be forgotten first");
        let add = calls
            .iter()
            .find(|c| c.first().map(String::as_str) == Some("workspace") && c.get(1).map(String::as_str) == Some("add"))
            .expect("the workspace must have been added");
        assert_eq!(
            *add,
            vec!["workspace", "add", "--name", &name, "--revision", "main", &target_arg],
            "the workspace is named by path digest so two paths never collide"
        );
        let pointer = std::fs::read_to_string(target.join(".jj").join("repo")).unwrap();
        assert!(PathBuf::from(pointer.trim()).is_absolute(), "got: {pointer:?}");
    }

    // ── revset safety ────────────────────────────────────────────────────────

    #[test]
    fn a_bookmark_name_that_is_really_a_revset_is_refused() {
        // A name reaching `present(...)` raw is revset injection: the caller
        // gets to choose which commits this backend considers "the branch".
        let (_d, root) = jj_dir();
        let repo = JjRepo::at(&root);
        let b = backend(Arc::new(FakeJj::new().then_default(Ok(SHA))));

        for hostile in [
            r#"foo" | ~root"#,
            "foo & bar",
            "foo(bar)",
            "main\n@",
            "",
            "@",
            "*",
            "main|root",
        ] {
            assert!(
                !revset_atom(hostile),
                "{hostile:?} must not be embeddable in a revset"
            );
            assert_eq!(b.branch_ref(&repo, hostile), "", "{hostile:?} must yield no reference at all");
            assert!(!b.local_branch_exists(&repo, hostile));
            assert!(b.is_head_merged_into_ref(&root, hostile).is_err());
        }
    }

    #[test]
    fn ordinary_bookmark_names_stay_embeddable() {
        // Including the remote-tracking form jj itself uses, which is the whole
        // reason `@` has to stay in the allowlist.
        for ok in ["main", "main@origin", "release/2.0", "fix_login", "a.b.c", "v1_2-3"] {
            assert!(revset_atom(ok), "{ok:?} must be embeddable");
        }
    }

    // ── bookmark selection ───────────────────────────────────────────────────

    #[test]
    fn the_freshest_bookmark_wins_and_origin_wins_a_divergence() {
        // Present(local) / present(local@origin) / (local & ::(remote)) /
        // (remote & ::(local)) — one per case, then the divergence default.
        struct Case {
            script: Vec<Result<&'static str, &'static str>>,
            want: &'static str,
            why: &'static str,
        }
        let cases = [
            Case { script: vec![Ok(SHA), Ok("")], want: "main", why: "local only" },
            Case { script: vec![Ok(""), Ok(SHA)], want: "main@origin", why: "origin only" },
            Case {
                script: vec![Ok(SHA), Ok(SHA), Ok(SHA)],
                want: "main@origin",
                why: "local is an ancestor of origin: origin is ahead",
            },
            Case {
                script: vec![Ok(SHA), Ok(SHA), Ok(""), Ok(SHA)],
                want: "main",
                why: "origin is an ancestor of local: local is ahead",
            },
            Case {
                script: vec![Ok(SHA), Ok(SHA), Ok(""), Ok(""), Ok(SHA)],
                want: "main@origin",
                why: "diverged: prefer origin",
            },
        ];
        for case in cases {
            let (_d, root) = jj_dir();
            let fake = Arc::new(FakeJj::new().then(&case.script).then_default(Ok(SHA)));
            let got = backend(fake).branch_ref(&JjRepo::at(&root), "main");
            assert_eq!(got, case.want, "{}", case.why);
        }
    }

    #[test]
    fn a_bookmark_that_exists_nowhere_still_names_what_the_caller_asked_for() {
        // The trait has no error channel, so the local name is returned and the
        // caller reports "cannot resolve bookmark main" — not a second name the
        // user never mentioned.
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then_default(Ok("")));
        assert_eq!(
            backend(fake).branch_ref(&JjRepo::at(&root), "trunk"),
            "trunk"
        );
    }

    #[test]
    fn the_default_bookmark_prefers_origin_and_fails_closed_when_absent() {
        let (_d, root) = jj_dir();
        let repo = JjRepo::at(&root);

        // origin present, but none of the candidates is on it: fall through to
        // the local scan, which finds `trunk`.
        let fake = Arc::new(
            FakeJj::new().then(&[
                Ok("origin git@host:o/r.git"),
                Ok(""),
                Ok(""),
                Ok(""),
                Ok(""),
                Ok(""),
                Ok(SHA),
            ]),
        );
        assert_eq!(backend(fake).default_branch(&repo).unwrap(), "trunk");

        // Nothing anywhere: an error naming the candidates, never a guess.
        let fake = Arc::new(FakeJj::new().then_default(Ok("")));
        let err = backend(fake).default_branch(&repo).expect_err("must fail closed");
        assert_eq!(err.kind, GitErrorKind::DefaultBranchUnresolvable);
        assert!(err.message.contains("main, master, trunk"), "got: {}", err.message);
    }

    #[test]
    fn the_merge_ref_names_the_remote_bookmark_when_origin_exists() {
        let (_d, root) = jj_dir();
        let repo = JjRepo::at(&root);
        // default_branch: remote list -> present(main@origin) -> "main";
        // then default_branch_merge_ref: remote list -> present(main@origin).
        let fake = Arc::new(
            FakeJj::new().then(&[
                Ok("origin git@host:o/r.git"),
                Ok(SHA),
                Ok("origin git@host:o/r.git"),
                Ok(SHA),
            ]),
        );
        assert_eq!(backend(fake).default_branch_merge_ref(&repo).unwrap(), "main@origin");
    }

    #[test]
    fn a_missing_merge_bookmark_is_refused_rather_than_substituted() {
        let (_d, root) = jj_dir();
        let repo = JjRepo::at(&root);
        // No origin, and `main` resolves as the default bookmark — then vanishes
        // before the merge-ref check, so the merge ref itself is unavailable.
        let fake = Arc::new(FakeJj::new().then(&[Ok(""), Ok(SHA)]).then_default(Ok("")));
        let err = backend(fake)
            .default_branch_merge_ref(&repo)
            .expect_err("an unresolvable merge ref must fail closed");
        assert_eq!(err.kind, GitErrorKind::MergeRefUnresolvable);
        assert!(err.message.contains("main is unavailable"), "got: {}", err.message);
    }

    // ── fetch classification ─────────────────────────────────────────────────

    #[test]
    fn an_unreachable_origin_is_tagged_so_prune_skips_rather_than_deletes() {
        let (_d, root) = jj_dir();
        let repo = JjRepo::at(&root);
        let fake = Arc::new(FakeJj::new().then(&[
            Ok("origin https://example.invalid/r.git"),
            Err("External git program failed (exit code 128)\n  unable to access 'https://example.invalid/r.git/': Could not resolve host"),
        ]));
        let err = backend(fake)
            .fetch(&repo)
            .expect_err("an unreachable origin must fail");
        assert_eq!(err.kind, GitErrorKind::OriginUnreachable);
    }

    #[test]
    fn a_fetch_without_origin_is_a_no_op_and_a_local_failure_is_not_tagged() {
        let (_d, root) = jj_dir();
        let repo = JjRepo::at(&root);
        let fake = Arc::new(FakeJj::new().then(&[Ok(""), Err("operation failed")]));
        let b = backend(fake.clone());
        b.fetch(&repo).unwrap();
        assert_eq!(fake.calls().len(), 1, "no origin means no fetch");

        let fake = Arc::new(FakeJj::new().then(&[Ok("origin u"), Err("operation failed")]));
        let err = backend(fake).fetch(&repo).expect_err("a real jj failure must surface");
        assert_eq!(
            err.kind,
            GitErrorKind::Other,
            "tagging a non-network failure as unreachable would make prune skip a fixable error"
        );
    }

    #[test]
    fn origin_failures_are_classified_by_jj_s_own_vocabulary() {
        // jj wraps git for transport, and a local-path remote fails in jj's own
        // words — so the patterns are jj's, not git's.
        for detail in [
            "External git program failed",
            "fatal: unable to access 'https://example.invalid/r.git/'",
            "Could not resolve host: example.invalid",
            "Could not find repository at 'file:///nope'",
        ] {
            assert!(is_origin_access_error(detail), "{detail:?} must classify as unreachable");
        }
        for detail in ["operation failed", "bookmark main not found", ""] {
            assert!(!is_origin_access_error(detail), "{detail:?} must not classify as unreachable");
        }
    }

    // ── the guarded-reset pair ───────────────────────────────────────────────

    #[test]
    fn a_check_returns_the_target_and_the_head_together() {
        // update-stale, the two bookmark probes, the pinned target, `@`, and the
        // ancestry check — and the guard carries BOTH ids, because they are
        // only meaningful together.
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[
            Ok(""),           // workspace update-stale
            Ok(SHA),          // present(main)
            Ok(""),           // present(main@origin) — local only
            Ok(SHA),          // log -r main -> the pinned reset target
            Ok(SHA_B),        // log -r @       -> the head the check observed
            Ok(""),           // @- & ~::(main) -> every parent is an ancestor
        ]));
        let guard = backend(fake).is_worktree_safe_to_reset(&root, "main").unwrap();

        assert!(guard.safe);
        assert_eq!(guard.reset_ref, SHA);
        assert_eq!(guard.head, SHA_B);
    }

    #[test]
    fn a_check_on_a_worktree_ahead_of_its_base_is_not_safe() {
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[
            Ok(""),
            Ok(SHA),
            Ok(""),
            Ok(SHA),           // reset target
            Ok(SHA_B),         // head
            Ok("unmerged-parent"), // @- is not an ancestor of main
        ]));
        let guard = backend(fake).is_worktree_safe_to_reset(&root, "main").unwrap();
        assert!(!guard.safe, "unlanded work must never read as safe");
        // The pair is still returned: a caller may weigh other evidence.
        assert_eq!(guard.reset_ref, SHA);
        assert_eq!(guard.head, SHA_B);
    }

    #[test]
    fn a_check_fails_closed_when_the_bookmark_cannot_be_resolved() {
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[Ok(""), Ok(""), Ok("")]));
        let err = backend(fake)
            .is_worktree_safe_to_reset(&root, "main")
            .expect_err("an unresolvable bookmark is not safe, it is unknown");
        assert!(err.message.contains("cannot resolve"), "got: {}", err.message);
    }

    #[test]
    fn a_reset_refuses_an_argument_that_is_not_a_jj_commit_id() {
        // Both ids are interpolated into revsets; a truncated one is a silent
        // no-match, and a name is revset injection.
        let (_d, root) = jj_dir();
        let b = backend(Arc::new(FakeJj::new().then_default(Ok("clean"))));
        for bad in ["main", "HEAD", "", "NOTAHEXSHA", &SHA[..39], "AAAA"] {
            assert!(
                b.reset_worktree_to_ref(&root, SHA, bad, false).is_err(),
                "expected_head {bad:?} must be refused"
            );
            assert!(
                b.reset_worktree_to_ref(&root, bad, SHA, false).is_err(),
                "reset_ref {bad:?} must be refused"
            );
        }
    }

    // ── reset: clean workspace ───────────────────────────────────────────────

    #[test]
    fn a_clean_workspace_is_rebased_and_the_postcondition_is_verified() {
        // jj always commits, so the pinned revset is the lock substitute: a
        // rebase that did NOT move @ must be caught, not reported as success.
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[
            Ok(""),       // workspace update-stale
            Ok("clean"),  // is_dirty
            Ok(""),       // jj rebase -d <ref> -r @ & commit_id(head)
            Ok("clean"),  // parked_on_ref -> is_dirty
            Ok(SHA),      // parked_on_ref -> log -r @-
        ]));
        backend(fake.clone())
            .reset_worktree_to_ref(&root, SHA, SHA_B, true)
            .unwrap();

        assert!(
            fake.called(&["rebase", "-d", SHA, "-r", &format!("@ & commit_id(\"{SHA_B}\")")]),
            "the rebase must pin @ by the commit the check observed: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn a_rebase_that_left_the_workspace_where_it_was_is_refused() {
        // The single most important outcome in this file: jj exits 0 for a
        // no-op rebase, so reporting success here would discard nothing and
        // reset nothing while every caller believes the slot is clean.
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[
            Ok(""),
            Ok("clean"),
            Ok(""),       // rebase "succeeded"
            Ok("clean"),  // parked_on_ref -> is_dirty
            Ok(SHA_B),    // @- is still the OLD commit, not the reset target
        ]));
        let err = backend(fake)
            .reset_worktree_to_ref(&root, SHA, SHA_B, true)
            .expect_err("a workspace that did not move must be refused");
        assert!(err.message.contains("HEAD changed since safety check"), "got: {}", err.message);
    }

    #[test]
    fn a_rebase_error_on_an_already_parked_workspace_is_a_success() {
        // `jj rebase` onto the commit @ already sits on can fail as a no-op.
        // That is only a success when @ really is parked on the target AND was
        // never moved by anything else.
        let (_d, root) = jj_dir();
        let parked = Arc::new(FakeJj::new().then(&[
            Ok(""),
            Ok("clean"),
            Err("This commit is already rebased"),
            Ok("clean"),
            Ok(SHA),
            Ok(SHA_B),
        ]));
        backend(parked.clone())
            .reset_worktree_to_ref(&root, SHA, SHA_B, true)
            .unwrap();
        assert!(parked.called(&["log", "-r", "@", "--no-graph"]));

        // Same jj error, but @ moved in the meantime: a real failure.
        let moved = Arc::new(FakeJj::new().then(&[
            Ok(""),
            Ok("clean"),
            Err("This commit is already rebased"),
            Ok("clean"),
            Ok(SHA),
            Ok("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ]));
        assert!(
            backend(moved).reset_worktree_to_ref(&root, SHA, SHA_B, true).is_err(),
            "a rebase error is only survivable while @ is provably unchanged"
        );
    }

    // ── reset: dirty workspace ───────────────────────────────────────────────

    #[test]
    fn a_dirty_workspace_is_refused_when_the_caller_required_clean() {
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[Ok(""), Ok("dirty")]));
        let err = backend(fake.clone())
            .reset_worktree_to_ref(&root, SHA, SHA_B, true)
            .expect_err("uncommitted work must not be discarded");
        assert!(err.message.contains("became dirty"), "got: {}", err.message);
        assert!(!fake.called(&["abandon"]), "the refusal must precede the abandon");
        assert!(!fake.called(&["rebase"]));
    }

    #[test]
    fn a_dirty_workspace_is_abandoned_and_reparked_only_after_every_check() {
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[
            Ok(""),                          // update-stale
            Ok("dirty"),                     // is_dirty
            Ok(""),                          // jj abandon -r @ & commit_id(head)
            Ok(""),                          // the pinned revset is now empty
            Ok("clean"),                     // still clean
            Ok(""),                          // merged into the reset target
            Ok(""),                          // jj new <ref>
        ]));
        let b = backend(fake.clone());
        b.reset_worktree_to_ref(&root, SHA, SHA_B, false).unwrap();

        let calls = fake.calls();
        assert_eq!(calls[2], vec!["abandon", "-r", &format!("@ & commit_id(\"{SHA_B}\")")]);
        assert_eq!(calls.last().unwrap(), &vec!["new".to_string(), SHA.to_string()]);
    }

    #[test]
    fn an_abandon_that_did_not_take_is_refused_before_the_workspace_is_rebuilt() {
        // `@` still matching the pinned commit means it moved, or never was what
        // the safety check measured. Rebuilding on top would strand the wrong
        // history as the parent of a fresh commit.
        for (label, script) in [
            (
                "the pinned revset is still non-empty",
                vec![Ok(""), Ok("dirty"), Ok(""), Ok(SHA)],
            ),
            (
                "the workspace is still dirty",
                vec![Ok(""), Ok("dirty"), Ok(""), Ok(""), Ok("dirty")],
            ),
            (
                "the workspace is unmerged into the target",
                vec![Ok(""), Ok("dirty"), Ok(""), Ok(""), Ok("clean"), Ok(SHA)],
            ),
        ] {
            let (_d, root) = jj_dir();
            let fake = Arc::new(FakeJj::new().then(&script));
            let err = backend(fake.clone())
                .reset_worktree_to_ref(&root, SHA, SHA_B, false)
                .unwrap_err();
            assert!(
                err.message.contains("HEAD changed since safety check"),
                "{label}: got {}",
                err.message
            );
            assert!(!fake.called(&["new"]), "{label}: must not rebuild the workspace");
        }
    }

    #[test]
    fn reset_worktree_resolves_the_bookmark_once_and_hands_the_pair_over() {
        let (_d, root) = jj_dir();
        let fake = Arc::new(FakeJj::new().then(&[
            Ok(""),       // resolve_reset_ref: update-stale
            Ok(SHA),      // present(main)
            Ok(""),       // present(main@origin)
            Ok(SHA),      // log -r main -> the pinned target
            Ok(SHA_B),    // log -r @    -> head
            Ok(""),       // reset_worktree_to_ref: update-stale
            Ok("clean"),  // is_dirty
            Ok(""),       // rebase
            Ok("clean"),  // parked check
            Ok(SHA),      // @- == the target
        ]));
        backend(fake.clone()).reset_worktree(&root, "main").unwrap();

        // The bookmark was pinned to a commit id, so the rebase targets an id —
        // never the mutable bookmark name.
        assert!(fake.called(&["rebase", "-d", SHA]));
        assert!(
            !fake.called(&["rebase", "-d", "main"]),
            "a reset must never target the mutable bookmark itself"
        );
    }

    // ── refusal surfaces ─────────────────────────────────────────────────────

    #[test]
    fn removal_refuses_a_directory_that_is_not_a_jj_workspace() {
        // Removing a git worktree here would delete its files while leaving the
        // registration behind.
        let (_d, root) = jj_dir();
        let slot = root.join("gitslot");
        std::fs::create_dir_all(slot.join(".git")).unwrap();
        std::fs::write(slot.join("data.txt"), "keep me").unwrap();

        let fake = Arc::new(FakeJj::new().then_default(Ok("")));
        let err = backend(fake.clone())
            .worktree_remove(&JjRepo::at(&root), &slot)
            .expect_err("a git worktree must not be deleted through the jj backend");
        assert!(err.message.contains("not a jj workspace"), "got: {}", err.message);
        assert!(slot.join("data.txt").exists(), "the refusal must precede any deletion");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn removal_refuses_a_main_workspace_which_holds_the_repository_store() {
        // `.jj/repo` is a DIRECTORY in the main workspace: forgetting the
        // digest-named registration would no-op and the delete would destroy
        // the entire repository.
        let (_d, root) = jj_dir();
        std::fs::create_dir(root.join(".jj").join("repo")).unwrap();
        std::fs::write(root.join("README.md"), "the repository").unwrap();

        let fake = Arc::new(FakeJj::new().then_default(Ok("")));
        let err = backend(fake.clone())
            .worktree_remove(&JjRepo::at(&root), &root)
            .expect_err("the main workspace must be refused");
        assert!(err.message.contains("main jj workspace"), "got: {}", err.message);
        assert!(root.join("README.md").exists());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_secondary_workspace_is_forgotten_by_name_and_removed() {
        let (_d, root) = jj_dir();
        let slot = root.join("wt-1");
        std::fs::create_dir_all(slot.join(".jj")).unwrap();
        std::fs::write(slot.join(".jj").join("repo"), "../../main/.jj/repo").unwrap();

        let fake = Arc::new(FakeJj::new().then_default(Ok("")));
        backend(fake.clone())
            .worktree_remove(&JjRepo::at(&root), &slot)
            .unwrap();

        assert_eq!(
            fake.calls()[0],
            vec!["workspace", "forget", &workspace_name_for(&slot)]
        );
        assert!(!slot.exists());
    }

    #[test]
    fn a_dirty_workspace_is_not_removed_by_the_clean_removal() {
        let (_d, root) = jj_dir();
        let slot = root.join("wt-1");
        std::fs::create_dir_all(slot.join(".jj")).unwrap();

        let fake = Arc::new(FakeJj::new().then(&[Ok("dirty")]).then_default(Ok("")));
        let err = backend(fake.clone())
            .remove_clean_worktree(&JjRepo::at(&root), &slot)
            .expect_err("a dirty workspace must be kept");
        assert!(err.message.contains("has local changes"), "got: {}", err.message);
        assert!(slot.exists());
        assert!(!fake.called(&["workspace", "forget"]));
    }

    #[test]
    fn branch_creation_is_refused_and_says_which_marker_was_found() {
        let (_d, root) = jj_dir();
        let b = backend(Arc::new(FakeJj::new().then_default(Ok(""))));
        let err = b.create_branch(&root, "feature").expect_err("jj bookmarks are not git branches");
        assert!(
            err.message.contains("only supported by") || err.message.contains("requires a git worktree"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("jj"), "the operator must learn the slot's flavor: got: {}", err.message);
    }

    #[test]
    fn the_common_git_dir_is_the_main_roots_git_directory_when_colocated() {
        let (_d, main, ws) = main_and_workspace();
        std::fs::create_dir(main.join(".git")).unwrap();

        // `jj workspace root` answers the workspace's own path; the store
        // pointer then walks up to the MAIN root, which is where
        // repo-local .git/info/exclude lives.
        let root = ws.to_string_lossy().to_string();
        let fake = Arc::new(FakeJj::new().then(&[Ok(root.as_str())]));
        assert_eq!(
            backend(fake).common_git_dir(&ws).unwrap(),
            canonicalize(&main).join(".git"),
            "repo-local .git/info/exclude lives at the MAIN root, not the workspace's"
        );
    }

    #[test]
    fn a_non_colocated_repository_reports_no_usable_git_dir() {
        let (_d, _main, ws) = main_and_workspace();

        let root = ws.to_string_lossy().to_string();
        let fake = Arc::new(FakeJj::new().then(&[Ok(root.as_str())]));
        let err = backend(fake)
            .common_git_dir(&ws)
            .expect_err("a non-colocated jj repo has no .git to point at");
        assert!(err.message.contains("not colocated"), "got: {}", err.message);
    }

    // ── no jj binary ─────────────────────────────────────────────────────────

    #[test]
    fn with_no_jj_installed_every_command_fails_with_the_real_cause() {
        // The backend registers REGARDLESS of whether jj is installed, so a
        // `.jj` marker resolves to jj rather than to the seam's "no backend is
        // registered for jj" — which would send the operator hunting for a
        // configuration problem that does not exist.
        let b = JjBackend::discover();
        if find_jj_binary().is_some() {
            return; // jj IS installed here; the fake-path test below still ran.
        }
        assert_eq!(b.name(), BACKEND_JJ, "identity is what dispatch looks the backend up by");

        let (_d, root) = jj_dir();
        let err = b.is_dirty(&root).expect_err("jj is not installed, so this cannot succeed");
        assert_eq!(err.kind, GitErrorKind::NotFound);
        assert!(err.message.contains("jj binary not found"), "got: {}", err.message);
    }

    #[test]
    fn a_git_marker_is_never_honoured_by_the_jj_backend() {
        // Belt and braces over the markerless test: the constant is referenced
        // here so a rename cannot silently break the refusal above.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert_eq!(crate::vcs::worktree_backend_name(dir.path()).unwrap(), Some(BACKEND_GIT));
        assert!(
            backend(Arc::new(FakeJj::new())).detach_worktree(dir.path()).is_err(),
            "a slot marked git must never be answered by the jj backend"
        );
    }
}
