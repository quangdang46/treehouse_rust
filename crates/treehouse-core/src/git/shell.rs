//! [`ShellGitBackend`]: runs the `git` binary directly.
//!
//! Each argument is passed as its own `OsString` — never through a shell, never
//! shell-quoted. Paths with spaces work because `std::process::Command` uses
//! the platform's exec semantics (MSVC CRT argument handling on Windows).
//!
//! Binary discovery: `GIT_BIN` env override → `PATH` → Windows
//! `Program Files\Git\bin\git.exe`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::{GitBackend, GitError, GitErrorKind, GitRepo};

/// Runs git by spawning the binary directly (no shell).
#[derive(Debug, Clone)]
pub struct ShellGitBackend {
    git_bin: PathBuf,
}

impl ShellGitBackend {
    /// Discovers and constructs the backend.
    pub fn discover() -> Result<Self, GitError> {
        let bin = find_git_binary().ok_or_else(|| {
            GitError::new(
                "git",
                "git binary not found on PATH (set GIT_BIN)",
                GitErrorKind::NotFound,
            )
        })?;
        Ok(Self { git_bin: bin })
    }

    /// Constructs from a known git binary path.
    pub fn with_bin(bin: PathBuf) -> Self {
        Self { git_bin: bin }
    }

    /// The resolved git binary path.
    pub fn git_bin(&self) -> &Path {
        &self.git_bin
    }

    /// Runs `git <args>` in `cwd`, returning the `Output` (no exit-code check).
    fn run(&self, cwd: Option<&Path>, args: &[&str]) -> Output {
        let mut cmd = Command::new(&self.git_bin);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.output().unwrap_or_else(|e| {
            // If git is missing at runtime (deleted since discovery), surface
            // the spawn error so callers see a hard failure, not a silent
            // empty success.
            Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: format!("git binary failed to spawn: {e}").into_bytes(),
            }
        })
    }

    /// Runs git and checks the exit code, capturing stderr into a
    /// [`GitError`] on failure.
    fn run_checked(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        kind: GitErrorKind,
    ) -> Result<(), GitError> {
        let output = self.run(cwd, args);
        if output.status.success() {
            return Ok(());
        }
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let command = format!("git {}", args.join(" "));
        Err(GitError::new(command, message, kind))
    }

    /// Runs git and returns trimmed, UTF-8-lossy stdout, failing on nonzero
    /// exit.
    fn run_stdout(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        kind: GitErrorKind,
    ) -> Result<String, GitError> {
        let output = self.run(cwd, args);
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let command = format!("git {}", args.join(" "));
            return Err(GitError::new(command, message, kind));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Runs git and returns stdout WITHOUT trimming (used by `remote` parsing).
    fn run_lines(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        kind: GitErrorKind,
    ) -> Result<String, GitError> {
        let output = self.run(cwd, args);
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let command = format!("git {}", args.join(" "));
            return Err(GitError::new(command, message, kind));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Finds the git binary: `GIT_BIN` env → `PATH` → Windows Program Files.
fn find_git_binary() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("GIT_BIN") {
        let p = PathBuf::from(&v);
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(found) = which_in_path("git") {
        return Some(found);
    }
    #[cfg(windows)]
    {
        for base in [
            "C:\\Program Files\\Git\\bin\\git.exe",
            "C:\\Program Files (x86)\\Git\\bin\\git.exe",
        ] {
            let p = PathBuf::from(base);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Minimal `which`: search PATH for an executable named `name`.
fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(&exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The main repo root for a worktree: `rev-parse --git-common-dir` with
/// `--path-format=absolute`, resolving `<main>/.git` → `<main>`.
fn main_repo_root_from(start: &Path, git_bin: &Path) -> Result<PathBuf, GitError> {
    let repo_root = repo_root_from(start, git_bin)?;
    // Try --git-common-dir absolute first.
    let common = run_one(
        git_bin,
        Some(&repo_root),
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    if let Ok(dir) = common {
        let dir = PathBuf::from(dir.trim());
        if dir.file_name() == Some(OsStr::new(".git"))
            && let Some(parent) = dir.parent()
        {
            return Ok(parent.to_path_buf());
        }
    }
    Ok(repo_root)
}

fn repo_root_from(start: &Path, git_bin: &Path) -> Result<PathBuf, GitError> {
    let out = run_one(git_bin, Some(start), &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(out.trim()))
}

/// One-shot git run helper for the free functions above.
fn run_one(git_bin: &Path, cwd: Option<&Path>, args: &[&str]) -> Result<String, GitError> {
    let output = Command::new(git_bin)
        .args(args)
        .current_dir(cwd.unwrap_or(Path::new(".")))
        .output()
        .map_err(|e| {
            GitError::new(
                format!("git {}", args.join(" ")),
                e.to_string(),
                GitErrorKind::Other,
            )
        })?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(GitError::new(
            format!("git {}", args.join(" ")),
            message,
            GitErrorKind::Other,
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl GitBackend for ShellGitBackend {
    fn repo_root(&self, start: &Path) -> Result<PathBuf, GitError> {
        repo_root_from(start, &self.git_bin)
    }

    fn main_repo_root(&self, start: &Path) -> Result<PathBuf, GitError> {
        main_repo_root_from(start, &self.git_bin)
    }

    fn default_branch(&self, repo: &GitRepo) -> Result<String, GitError> {
        // Remote HEAD first (most reliable when origin exists).
        if self.has_remote(repo, "origin")
            && let Ok(out) = self.run_stdout(
                Some(&repo.common_dir),
                &["symbolic-ref", "refs/remotes/origin/HEAD"],
                GitErrorKind::DefaultBranchUnresolvable,
            )
            && let Some(branch) = out.strip_prefix("refs/remotes/origin/")
            && !branch.is_empty()
        {
            return Ok(branch.to_string());
        }
        // Local symbolic-ref HEAD.
        if let Ok(out) = self.run_stdout(
            Some(&repo.common_dir),
            &["symbolic-ref", "HEAD"],
            GitErrorKind::DefaultBranchUnresolvable,
        ) && let Some(branch) = out.strip_prefix("refs/heads/")
            && !branch.is_empty()
        {
            return Ok(branch.to_string());
        }
        // init.defaultBranch.
        if let Ok(out) = self.run_stdout(
            Some(&repo.common_dir),
            &["config", "init.defaultBranch"],
            GitErrorKind::DefaultBranchUnresolvable,
        ) && !out.is_empty()
        {
            return Ok(out);
        }
        Err(GitError::new(
            "git symbolic-ref HEAD",
            "cannot determine default branch: try running 'git fetch' or ensure you are on a branch",
            GitErrorKind::DefaultBranchUnresolvable,
        ))
    }

    fn has_remote(&self, repo: &GitRepo, name: &str) -> bool {
        let Ok(out) = self.run_lines(Some(&repo.common_dir), &["remote"], GitErrorKind::Other)
        else {
            return false;
        };
        out.lines().any(|l| l.trim() == name)
    }

    fn fetch(&self, repo: &GitRepo) -> Result<(), GitError> {
        if !self.has_remote(repo, "origin") {
            return Ok(());
        }
        let out = self.run(Some(&repo.common_dir), &["fetch", "origin"]);
        if out.status.success() {
            return Ok(());
        }
        let message = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(GitError::new(
            "git fetch origin",
            message,
            GitErrorKind::OriginUnreachable,
        ))
    }

    fn worktree_add(&self, repo: &GitRepo, path: &Path, branch: &str) -> Result<(), GitError> {
        let ref_ = self.branch_ref(repo, branch);
        self.run_checked(
            Some(&repo.common_dir),
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap_or(""),
                &ref_,
            ],
            GitErrorKind::Other,
        )
    }

    fn worktree_remove(&self, repo: &GitRepo, path: &Path) -> Result<(), GitError> {
        self.run_checked(
            Some(&repo.common_dir),
            &["worktree", "remove", "--force", path.to_str().unwrap_or("")],
            GitErrorKind::Other,
        )
    }

    fn remove_clean_worktree(&self, repo: &GitRepo, path: &Path) -> Result<(), GitError> {
        self.run_checked(
            Some(&repo.common_dir),
            &["worktree", "remove", path.to_str().unwrap_or("")],
            GitErrorKind::Other,
        )
    }

    fn is_dirty(&self, worktree: &Path) -> Result<bool, GitError> {
        let out = self.run_stdout(
            Some(worktree),
            &["status", "--porcelain", "--untracked-files=all"],
            GitErrorKind::Other,
        )?;
        Ok(!out.is_empty())
    }

    fn reset_worktree(&self, worktree: &Path, branch: &str) -> Result<(), GitError> {
        // Resolve the repo root; fall back to the worktree path.
        let repo_root = self
            .repo_root(worktree)
            .unwrap_or_else(|_| worktree.to_path_buf());
        let repo = GitRepo {
            common_dir: repo_root,
            worktree: Some(worktree.to_path_buf()),
        };
        let ref_ = self.branch_ref(&repo, branch);
        self.run_checked(
            Some(worktree),
            &["checkout", "--detach", "--force", &ref_],
            GitErrorKind::Other,
        )?;
        self.run_checked(
            Some(worktree),
            &["reset", "--hard", &ref_],
            GitErrorKind::Other,
        )?;
        self.run_checked(Some(worktree), &["clean", "-fd"], GitErrorKind::Other)?;
        Ok(())
    }

    fn detach_worktree(&self, worktree: &Path) -> Result<(), GitError> {
        self.run_checked(
            Some(worktree),
            &["checkout", "--detach"],
            GitErrorKind::Other,
        )
    }

    fn is_head_merged_into_ref(&self, worktree: &Path, reference: &str) -> Result<bool, GitError> {
        let out = self.run(
            Some(worktree),
            &["merge-base", "--is-ancestor", "HEAD", reference],
        );
        if out.status.success() {
            return Ok(true);
        }
        if let Some(code) = out.status.code()
            && code == 1
        {
            return Ok(false); // NOT merged — not an error.
        }
        let message = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(GitError::new(
            format!("git merge-base --is-ancestor HEAD {reference}"),
            message,
            GitErrorKind::StatusFailed,
        ))
    }

    fn default_branch_merge_ref(&self, repo: &GitRepo) -> Result<String, GitError> {
        if self.has_remote(repo, "origin") {
            // ls-remote --symref origin HEAD -> (branch, sha).
            let out = self.run(
                Some(&repo.common_dir),
                &["ls-remote", "--symref", "origin", "HEAD"],
            );
            if !out.status.success() {
                let message = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err(GitError::new(
                    "git ls-remote --symref origin HEAD",
                    message,
                    GitErrorKind::OriginUnreachable,
                ));
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let mut branch: Option<String> = None;
            let mut sha: Option<String> = None;
            for line in text.lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() == 3 && fields[0] == "ref:" && fields[2] == "HEAD" {
                    branch = fields[1].strip_prefix("refs/heads/").map(|s| s.to_string());
                } else if fields.len() == 2 && fields[1] == "HEAD" {
                    sha = Some(fields[0].to_string());
                }
            }
            let branch = branch.ok_or_else(|| {
                GitError::new(
                    "git ls-remote --symref",
                    "cannot determine origin default branch",
                    GitErrorKind::MergeRefUnresolvable,
                )
            })?;
            let sha = sha.ok_or_else(|| {
                GitError::new(
                    "git ls-remote --symref",
                    "cannot determine origin default branch commit",
                    GitErrorKind::MergeRefUnresolvable,
                )
            })?;

            let ref_ = format!("refs/remotes/origin/{branch}");
            let local_sha = self.ref_commit(&repo.common_dir, &ref_)?;
            if local_sha != sha {
                return Err(GitError::new(
                    "git rev-parse",
                    format!("{ref_} is stale: expected {sha}, got {local_sha}"),
                    GitErrorKind::MergeRefUnresolvable,
                ));
            }
            return Ok(ref_);
        }

        let branch = self.default_branch(repo)?;
        let ref_ = format!("refs/heads/{branch}");
        self.ref_commit(&repo.common_dir, &ref_)?;
        Ok(ref_)
    }

    fn branch_ref(&self, repo: &GitRepo, branch: &str) -> String {
        let local = format!("refs/heads/{branch}");
        let remote = format!("refs/remotes/origin/{branch}");
        let has_local = self.ref_exists(&repo.common_dir, &local);
        let has_remote = self.ref_exists(&repo.common_dir, &remote);

        match (has_local, has_remote) {
            (true, true) => {
                // Local ancestor of remote => remote ahead (or equal).
                if self.is_ancestor(&repo.common_dir, &local, &remote) {
                    remote
                } else if self.is_ancestor(&repo.common_dir, &remote, &local) {
                    // Remote ancestor of local => local strictly ahead.
                    local
                } else {
                    // Diverged: prefer origin.
                    remote
                }
            }
            (true, false) => local,
            _ => remote,
        }
    }
}

impl ShellGitBackend {
    /// `git rev-parse --verify <ref>^{commit}`.
    fn ref_commit(&self, dir: &Path, ref_: &str) -> Result<String, GitError> {
        self.run_stdout(
            Some(dir),
            &["rev-parse", "--verify", &format!("{ref_}^{{commit}}")],
            GitErrorKind::MergeRefUnresolvable,
        )
    }

    /// Whether a ref exists (`rev-parse --verify` succeeds).
    fn ref_exists(&self, dir: &Path, ref_: &str) -> bool {
        self.run(Some(dir), &["rev-parse", "--verify", ref_])
            .status
            .success()
    }

    /// Whether ref `a` is an ancestor of ref `b` (merge-base --is-ancestor).
    fn is_ancestor(&self, dir: &Path, a: &str, b: &str) -> bool {
        self.run(Some(dir), &["merge-base", "--is-ancestor", a, b])
            .status
            .success()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_git() {
        let backend = ShellGitBackend::discover().expect("git must be installed");
        assert!(backend.git_bin.exists(), "git binary path must exist");
    }

    // ---- integration tests against a real git repo (port of git_test.go) ----

    /// Runs git in `dir`, failing the test on error.
    fn must_git(dir: Option<&Path>, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir.unwrap_or(Path::new(".")))
            .output()
            .expect("git must be installed");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Creates a temp repo with one commit on `main`. Returns a handle holding
    /// the `TempDir` alive (dropped only when the test finishes) plus the repo
    /// path.
    fn temp_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        must_git(
            None,
            &["init", "--initial-branch=main", repo.to_str().unwrap()],
        );
        must_git(Some(&repo), &["config", "user.email", "test@test.com"]);
        must_git(Some(&repo), &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), b"hello\n").unwrap();
        must_git(Some(&repo), &["add", "."]);
        must_git(Some(&repo), &["commit", "-m", "initial"]);
        (dir, repo)
    }

    #[test]
    fn integration_is_dirty_detects_untracked_with_hidden_config() {
        let (_dir, repo) = temp_repo();
        let wt = repo.join("wt");
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
        // Hide untracked files from normal status; --untracked-files=all must
        // override this.
        must_git(Some(&wt), &["config", "status.showUntrackedFiles", "no"]);
        let backend = ShellGitBackend::discover().unwrap();

        assert!(
            !backend.is_dirty(&wt).unwrap(),
            "clean worktree is not dirty"
        );
        std::fs::write(wt.join("untracked.txt"), b"new").unwrap();
        assert!(
            backend.is_dirty(&wt).unwrap(),
            "--untracked-files=all must force untracked detection"
        );
    }

    #[test]
    fn integration_default_branch_from_linked_worktree() {
        let (_dir, repo) = temp_repo();
        let wt = repo.join("wt");
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
        let backend = ShellGitBackend::discover().unwrap();
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: Some(wt.clone()),
        };
        let branch = backend.default_branch(&repo_ref).unwrap();
        assert_eq!(branch, "main");
    }

    #[test]
    fn integration_main_repo_root_from_linked_worktree() {
        let (_dir, repo) = temp_repo();
        let wt = repo.join("wt");
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
        let backend = ShellGitBackend::discover().unwrap();
        let root = backend.main_repo_root(&wt).unwrap();
        // The linked worktree resolves back to the owning repo. Normalize both
        // sides (canonicalize returns the \\?\ verbatim form on Windows).
        let expected = std::fs::canonicalize(&repo).unwrap();
        let norm = |p: &Path| {
            p.to_string_lossy()
                .replace("\\\\?\\", "")
                .replace('\\', "/")
        };
        assert_eq!(norm(&root), norm(&expected));
    }

    #[test]
    fn integration_reset_worktree_cleans_dirty_changes() {
        let (_dir, repo) = temp_repo();
        let wt = repo.join("wt");
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
        let backend = ShellGitBackend::discover().unwrap();

        // Dirty the worktree with an untracked + tracked change.
        std::fs::write(wt.join("scratch.txt"), b"dirty").unwrap();
        std::fs::write(wt.join("README.md"), b"modified").unwrap();

        backend.reset_worktree(&wt, "main").unwrap();
        assert!(
            !backend.is_dirty(&wt).unwrap(),
            "reset must clean the worktree"
        );
        // On Windows core.autocrlf may convert LF -> CRLF on checkout.
        let readme = std::fs::read_to_string(wt.join("README.md")).unwrap();
        assert!(
            readme == "hello\n" || readme == "hello\r\n",
            "README should be reset to committed content, got {readme:?}"
        );
        assert!(
            !wt.join("scratch.txt").exists(),
            "clean -fd must remove untracked files"
        );
    }

    #[test]
    fn integration_is_head_merged_into_ref() {
        let (_dir, repo) = temp_repo();
        let wt = repo.join("wt");
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
        let backend = ShellGitBackend::discover().unwrap();

        // HEAD (== main) is merged into main.
        assert!(backend.is_head_merged_into_ref(&wt, "main").unwrap());

        // An unborn/unknown ref: merge-base errors, treated as NOT merged.
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };
        let merge_ref = backend.default_branch_merge_ref(&repo_ref).unwrap();
        assert_eq!(merge_ref, "refs/heads/main");
        assert!(backend.is_head_merged_into_ref(&wt, &merge_ref).unwrap());
    }
}
