//! Git backend: shell out to the `git` binary (Go parity — go-git has
//! incomplete worktree support).
//!
//! This module defines the object-safe [`GitBackend`] trait so a native `gix`
//! backend can be swapped in later (NOT P0). [`ShellGitBackend`] spawns
//! `git.exe` DIRECTLY (no shell, no quoting — each arg is its own `OsString`;
//! MSVC CRT rules handle spaces).

use std::path::{Path, PathBuf};
use std::sync::Arc;

// The safety check and the reset it authorizes must be named together or not
// at all — see [`ResetGuard`]. Re-exported so an implementor can write the
// signature without also importing the seam module.
pub use crate::vcs::ResetGuard;

/// A git repository, split into its common (main) dir and optional linked
/// worktree dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepo {
    /// The main (common) repository directory. For a linked worktree this is
    /// the owning repo's root; for a normal repo it's the repo root.
    pub common_dir: PathBuf,
    /// The working tree dir, if this is a worktree (linked or primary).
    pub worktree: Option<PathBuf>,
}

/// Error kind tags used to classify git failures for prune/destroy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitErrorKind {
    /// `git` binary not found on PATH.
    NotFound,
    /// Could not determine the default branch.
    DefaultBranchUnresolvable,
    /// `git fetch` / `ls-remote` failed because origin is unreachable.
    OriginUnreachable,
    /// The merge ref could not be resolved / is stale.
    MergeRefUnresolvable,
    /// `merge-base --is-ancestor` returned an unexpected exit code.
    StatusFailed,
    /// Any other git failure.
    Other,
}

/// Errors from invoking git.
#[derive(Debug, thiserror::Error)]
#[error("git {command}: {message}")]
pub struct GitError {
    pub command: String,
    pub message: String,
    pub kind: GitErrorKind,
}

impl GitError {
    pub fn new(command: impl Into<String>, message: impl Into<String>, kind: GitErrorKind) -> Self {
        Self {
            command: command.into(),
            message: message.into(),
            kind,
        }
    }
}

/// The git backend contract, mirroring Go `internal/git/git.go`.
///
/// Every method uses the exact git invocation from the Go baseline and
/// interprets exit codes identically.
///
/// The tail of the trait — [`Self::name`], [`Self::resolve_for_worktree`], the
/// guarded-reset pair, and the seeding hooks — is the SEAM: it is what lets a
/// second backend answer for its own worktrees without any call site changing.
/// All of it defaults, so this trait is complete for the one backend that
/// ships today and open for the one that does not.
pub trait GitBackend: Send + Sync {
    /// Root of the repository containing `start` (Go `FindRepoRootFrom`).
    fn repo_root(&self, start: &Path) -> Result<PathBuf, GitError>;

    /// Main repository root for `start`, resolving linked worktrees back to
    /// the owning repo (Go `FindMainRepoRootFrom`).
    fn main_repo_root(&self, start: &Path) -> Result<PathBuf, GitError>;

    /// The default branch name (Go `GetDefaultBranch`): remote-tracking
    /// `origin/HEAD` first, then local `symbolic-ref HEAD`, then
    /// `init.defaultBranch`.
    fn default_branch(&self, repo: &GitRepo) -> Result<String, GitError>;

    /// Whether the repo has a remote named `name` (Go `HasRemote`).
    fn has_remote(&self, repo: &GitRepo, name: &str) -> bool;

    /// The URL of remote `name` (Go `GetRemoteURL`); None if absent.
    fn remote_url(&self, repo: &GitRepo, name: &str) -> Option<String>;

    /// `git fetch origin`; a no-op without origin. Must fail with
    /// `GitErrorKind::OriginUnreachable` on failure so prune can emit a
    /// category-tagged skip (NOT a deletion).
    fn fetch(&self, repo: &GitRepo) -> Result<(), GitError>;

    /// `git worktree add --detach <path> <ref>` (Go `AddWorktree`).
    fn worktree_add(&self, repo: &GitRepo, path: &Path, branch: &str) -> Result<(), GitError>;

    /// `git worktree remove --force <path>` (Go `RemoveWorktree`, used by
    /// destroy).
    fn worktree_remove(&self, repo: &GitRepo, path: &Path) -> Result<(), GitError>;

    /// `git worktree remove <path>` (Go `RemoveCleanWorktree`, non-forced; a
    /// dirty worktree is rejected by git).
    fn remove_clean_worktree(&self, repo: &GitRepo, path: &Path) -> Result<(), GitError>;

    /// Creates `branch` at `worktree`'s current HEAD and checks it out (Go
    /// `vcs.CreateBranch`, the `--branch` / `-b` acquisition flag).
    ///
    /// DESTRUCTIVE and fail-closed under the same marker precondition as
    /// [`Self::reset_worktree`]: `git branch` + `git checkout` against a
    /// markerless path would create and switch a branch in the repository
    /// ENCLOSING the pool. Git refuses to create a branch that already exists,
    /// so this never adopts a caller's ref.
    fn create_branch(&self, worktree: &Path, branch: &str) -> Result<(), GitError>;

    /// Whether a LOCAL branch named `branch` exists (Go `LocalBranchExists`,
    /// `git show-ref --verify --quiet refs/heads/<branch>`). Used to refuse
    /// `--branch` against an existing ref rather than silently adopting it.
    fn local_branch_exists(&self, repo: &GitRepo, branch: &str) -> bool;

    /// Whether the worktree has tracked or untracked changes (Go `IsDirty`):
    /// `git status --porcelain --untracked-files=all` — ANY output is dirty.
    /// The `--untracked-files=all` flag is load-bearing: it forces untracked
    /// inclusion past `status.showUntrackedFiles`.
    fn is_dirty(&self, worktree: &Path) -> Result<bool, GitError>;

    /// Resets a worktree to `branch` (Go `ResetWorktree`): a single semantic
    /// unit of `checkout --detach --force` then `reset --hard` then
    /// `clean -fd`.
    ///
    /// DESTRUCTIVE and fail-closed. Implementations MUST verify that
    /// `worktree` still carries the marker git wrote when it was created
    /// (a `.git` entry — file for a linked worktree, directory for a primary
    /// checkout) and return `Err` before touching anything if it is absent or
    /// unresolvable. Without the marker, git resolves `worktree` by walking
    /// UP to whatever repository encloses it; in an in-project pool that is
    /// the user's own working tree, and `reset --hard` + `clean -fd` there is
    /// irreversible data loss. Go enforces the same precondition in
    /// `destructiveBackendForWorktree` (vcs.go:513-525).
    fn reset_worktree(&self, worktree: &Path, branch: &str) -> Result<(), GitError>;

    /// Detaches the worktree HEAD (Go `DetachWorktree`).
    ///
    /// DESTRUCTIVE and fail-closed under the same marker precondition as
    /// [`Self::reset_worktree`]: `checkout --detach` against a markerless path
    /// detaches the ENCLOSING repository's HEAD, not the slot's.
    fn detach_worktree(&self, worktree: &Path) -> Result<(), GitError>;

    /// Whether HEAD of `worktree` is an ancestor of `reference` (Go
    /// `IsHeadMergedIntoRef`): `git merge-base --is-ancestor HEAD <ref>`.
    /// Exit 0 = merged, exit 1 = NOT merged (NOT an error), other exits ->
    /// `GitErrorKind::StatusFailed` (so destroy marks Unverified).
    fn is_head_merged_into_ref(&self, worktree: &Path, reference: &str) -> Result<bool, GitError>;

    /// The fully-qualified merge-safety ref (Go `DefaultBranchMergeRef`).
    /// With origin: `ls-remote --symref origin HEAD` then require the local
    /// `refs/remotes/origin/<branch>` to exist AND match the remote HEAD SHA,
    /// else fail closed (stale). Local-only: `refs/heads/<default>`, fail
    /// closed if unresolvable.
    fn default_branch_merge_ref(&self, repo: &GitRepo) -> Result<String, GitError>;

    /// The ref to check out for `branch` (Go `branchRef`): strictly-ahead
    /// ref wins, origin on divergence, whichever exists otherwise.
    fn branch_ref(&self, repo: &GitRepo, branch: &str) -> String;

    // ─── Backend identity ───────────────────────────────────────────────────
    //
    // Everything below is declared here with a default that either delegates
    // to the seam or refuses, so adding a backend never requires editing this
    // trait — and therefore never risks colliding with whoever else is editing
    // a dispatch site. Defaults are NOT "and that is fine": each one names the
    // backend that MUST override it.

    /// This backend's name (Go `Backend.Name`), matching the name it is
    /// registered under in [`crate::vcs::BackendRegistry`].
    ///
    /// Defaults to `"git"`, which is correct for the only backend this port
    /// ships today and WRONG for any other. A backend that forgets to override
    /// it cannot be found under its own marker, so its worktrees are refused
    /// loudly rather than answered by git — a failure that is annoying rather
    /// than silent, which is the safe direction for a wrong name.
    fn name(&self) -> &'static str {
        "git"
    }

    /// The backend that owns `path` for operations that REWRITE its checkout
    /// (Go `destructiveBackendForWorktree`).
    ///
    /// Dispatches on the path's OWN marker, never on which backend was asked:
    /// the configured backend must not answer for a slot of the other flavor.
    /// A path with no marker is REFUSED, never answered by the enclosing
    /// repository — in the supported in-project pool layout that repository is
    /// the user's working tree, and rewriting it is irreversible.
    ///
    /// Returns a shared handle rather than an owned backend: dispatch is a
    /// registry lookup, so handing out a clone per call would both cost more
    /// and risk the caller acting on a different instance than everyone else.
    ///
    /// The default routes through
    /// [`crate::vcs::destructive_backend_for_worktree`], so every backend gets
    /// registry dispatch for free. Override only to restrict the answer; do
    /// not re-implement the marker rule.
    fn resolve_for_worktree(&self, path: &Path) -> Result<Arc<dyn GitBackend>, GitError> {
        crate::vcs::destructive_backend_for_worktree(path)
    }

    // ─── The guarded-reset pair ─────────────────────────────────────────────

    /// Reports whether `worktree` can be reset to `branch` without discarding
    /// committed work, and captures BOTH the immutable reset target and the
    /// HEAD the ancestry check ran against (Go `IsWorktreeSafeToReset`).
    ///
    /// The two travel together in a [`ResetGuard`] because they are only
    /// meaningful together: the caller must hand both to
    /// [`Self::reset_worktree_to_ref`] so the check and the reset share one
    /// target and a HEAD that moved in between is refused.
    ///
    /// Fails CLOSED — an unresolvable ref, an unreadable HEAD, or a git
    /// failure is an `Err`, and callers treat that as "not safe". `guard.safe
    /// == false` is a real answer the caller may weigh; `Err` means the
    /// question could not be answered at all.
    ///
    /// The logic lives in [`crate::vcs::is_worktree_safe_to_reset`] because it
    /// needs the git binary, which only a concrete backend holds. The git
    /// implementation forwards to it with `self.git_bin()`.
    fn is_worktree_safe_to_reset(
        &self,
        worktree: &Path,
        branch: &str,
    ) -> Result<ResetGuard, GitError> {
        let _ = (worktree, branch); // Refuses by design; see the default's contract.
        Err(crate::vcs::unsupported(
            self.name(),
            "is_worktree_safe_to_reset",
        ))
    }

    /// Resets `worktree` to an ALREADY RESOLVED commit, re-verifying first (Go
    /// `ResetWorktreeToRef`).
    ///
    /// `expected_head` is the HEAD [`Self::is_worktree_safe_to_reset`] recorded.
    /// The re-read and the destructive update both run while holding git's own
    /// `HEAD.lock` (created `O_CREAT|O_EXCL`), so a concurrent commit,
    /// checkout, merge, or rebase cannot slip a new commit in after the
    /// comparison. When `require_clean` is set, dirtiness is re-checked under
    /// that same lock before the tree is touched.
    ///
    /// REFUSES — before touching anything — if the marker is gone, if either
    /// argument is not a commit id, if HEAD moved, or if `require_clean` and
    /// the tree is dirty. A plain reset without that re-read is WORSE than no
    /// reset: callers believe the guards and the reset shared one target, so a
    /// commit landing in the gap is discarded while every check reports success.
    ///
    /// The logic lives in [`crate::vcs::reset_worktree_to_ref`]; the git
    /// implementation forwards to it with `self.git_bin()`.
    fn reset_worktree_to_ref(
        &self,
        worktree: &Path,
        reset_ref: &str,
        expected_head: &str,
        require_clean: bool,
    ) -> Result<(), GitError> {
        let _ = (worktree, reset_ref, expected_head, require_clean);
        Err(crate::vcs::unsupported(
            self.name(),
            "reset_worktree_to_ref",
        ))
    }

    // ─── Capabilities the later agents implement ────────────────────────────
    //
    // Declared NOW, defaulting to a refusal, so the jj and seeding agents never
    // have to edit this trait. Each default says which backend must override it.

    /// The shared git metadata directory for `start` (Go `CommonGitDir`): where
    /// the repo-local, untracked `.git/info/exclude` lives.
    ///
    /// Backends with no usable git dir return an error and callers degrade
    /// gracefully — an absent exclude file is normal, not a failure.
    fn common_git_dir(&self, start: &Path) -> Result<PathBuf, GitError> {
        let _ = start;
        Err(crate::vcs::unsupported(self.name(), "common_git_dir"))
    }

    /// Copies backend-specific ignored files into a new or recycled worktree
    /// and returns THEIR PATHS (Go `SeedWorktree`).
    ///
    /// The returned inventory is what cleanup trusts later, rather than
    /// re-reading mutable worktree metadata — so it is only as trustworthy as
    /// this function's honesty about what it wrote.
    ///
    /// `manifest` is `None` to use the committed `.worktreeinclude` at the
    /// DESTINATION HEAD, and `Some` to supply the manifest bytes directly; an
    /// empty `Some(&[])` selects nothing.
    fn seed_worktree(
        &self,
        repo: &GitRepo,
        worktree: &Path,
        manifest: Option<&[u8]>,
    ) -> Result<Vec<String>, GitError> {
        let _ = (repo, worktree, manifest);
        Err(crate::vcs::unsupported(self.name(), "seed_worktree"))
    }

    /// Resets a worktree after removing its trusted seed inventory (Go
    /// `ResetWorktreeWithSeededPaths`).
    ///
    /// `seeded` is validated by [`crate::vcs::validate_seed_inventory`] before
    /// anything is deleted on its word. **A `None` or empty inventory must NOT
    /// authorize ignored-file deletion**: it means the caller lost its
    /// bookkeeping, and honoring it converts a lost record into silent data
    /// loss. Every implementation must call the validator first.
    fn reset_worktree_with_seeded_paths(
        &self,
        worktree: &Path,
        branch: &str,
        seeded: &[String],
    ) -> Result<(), GitError> {
        let _ = (worktree, branch);
        // Validate BEFORE the capability refusal. An empty inventory is
        // refused on its own terms — not as "unsupported" — so the log says the
        // caller lost its bookkeeping, which is a different bug from the
        // backend lacking the capability.
        crate::vcs::validate_seed_inventory(seeded)?;
        Err(crate::vcs::unsupported(
            self.name(),
            "reset_worktree_with_seeded_paths",
        ))
    }
}

/// 6-hex sha256 of a string (Go `ShortHash`), used for pool dir naming.
pub fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..6].to_string()
}

// Re-export the shell backend so `treehouse_core::git::ShellGitBackend` works.
pub use self::shell::ShellGitBackend;

mod shell;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hash_is_6_hex() {
        let h = short_hash("https://github.com/foo/bar.git");
        assert_eq!(h.len(), 6);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Stable across calls.
        assert_eq!(h, short_hash("https://github.com/foo/bar.git"));
    }
}
