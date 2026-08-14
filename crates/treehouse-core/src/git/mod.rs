//! Git backend: shell out to the `git` binary (Go parity — go-git has
//! incomplete worktree support).
//!
//! This module defines the object-safe [`GitBackend`] trait so a native `gix`
//! backend can be swapped in later (NOT P0). [`ShellGitBackend`] spawns
//! `git.exe` DIRECTLY (no shell, no quoting — each arg is its own `OsString`;
//! MSVC CRT rules handle spaces).

use std::path::{Path, PathBuf};

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

    /// Whether the worktree has tracked or untracked changes (Go `IsDirty`):
    /// `git status --porcelain --untracked-files=all` — ANY output is dirty.
    /// The `--untracked-files=all` flag is load-bearing: it forces untracked
    /// inclusion past `status.showUntrackedFiles`.
    fn is_dirty(&self, worktree: &Path) -> Result<bool, GitError>;

    /// Resets a worktree to `branch` (Go `ResetWorktree`): a single semantic
    /// unit of `checkout --detach --force` then `reset --hard` then
    /// `clean -fd`.
    fn reset_worktree(&self, worktree: &Path, branch: &str) -> Result<(), GitError>;

    /// Detaches the worktree HEAD (Go `DetachWorktree`).
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
