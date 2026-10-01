//! [`ShellGitBackend`]: runs the `git` binary directly.
//!
//! Each argument is passed as its own `OsString` — never through a shell, never
//! shell-quoted. Paths with spaces work because `std::process::Command` uses
//! the platform's exec semantics (MSVC CRT argument handling on Windows).
//!
//! Binary discovery: `GIT_BIN` env override → `PATH` → Windows
//! `Program Files\Git\bin\git.exe`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::{GitBackend, GitError, GitErrorKind, GitRepo};

/// One git argument that may carry a caller-supplied path.
///
/// `Command` accepts an `OsStr` on every platform, so a path can reach git
/// byte-for-byte. Routing it through `Path::to_str()` first does not: that
/// returns `None` for any path that is not valid UTF-8, and the tempting
/// `unwrap_or("")` then hands git an EMPTY argument, which git resolves
/// against its own cwd — a different directory than the caller named, on the
/// exact subcommands (`worktree remove --force`) that delete one.
enum Arg<'a> {
    Lit(&'a str),
    Path(&'a Path),
}

impl Arg<'_> {
    fn to_os_string(&self) -> OsString {
        match self {
            Self::Lit(s) => OsString::from(s),
            Self::Path(p) => p.as_os_str().to_os_string(),
        }
    }
}

/// Fail-closed precondition for every operation that REWRITES a worktree's
/// checkout (reset, detach).
///
/// Go wraps all five destructive entry points in `destructiveBackendForWorktree`
/// (`internal/vcs/vcs.go:513-525`, added in c88b53e / v2.3.0) for exactly this
/// reason: it refuses a markerless path rather than falling back to the
/// configured backend, because in an in-project pool — a supported layout,
/// since a relative pool `root` nests `.treehouse` under the repo — that
/// fallback resolves to the repository ENCLOSING the pool. `reset --hard`
/// followed by `clean -fd` then discards the user's real uncommitted work in
/// their real working tree.
///
/// The marker is checked with `symlink_metadata` (lstat) rather than
/// `metadata` (stat), mirroring Go's `markerPresent`: a dangling symlink IS an
/// entry that is present on disk, and its unresolvable target is a read
/// failure for the caller to surface — not an absent marker. It is then
/// followed once with `metadata` so a dangling link fails HERE, loudly, instead
/// of surfacing three destructive git commands later. Both file and directory
/// `.git` entries are accepted: a linked worktree has a `gitdir:` file, a
/// primary checkout has a directory, and both are legitimately the thing the
/// caller asked us to reset.
///
/// jj is not a backend in this port, so only the `.git` marker is consulted
/// (Go's `slotMarkerBackend` accepts a `.jj` directory as the alternative).
fn require_worktree_marker(worktree: &Path) -> Result<(), GitError> {
    let marker = worktree.join(".git");
    let present = match std::fs::symlink_metadata(&marker) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(GitError::new(
                "lstat .git marker",
                format!("reading .git marker in {}: {e}", worktree.display()),
                GitErrorKind::Other,
            ));
        }
    };
    if !present {
        return Err(GitError::new(
            "lstat .git marker",
            format!(
                "refusing to modify {}: it holds no .git marker",
                worktree.display()
            ),
            GitErrorKind::Other,
        ));
    }
    std::fs::metadata(&marker).map(|_| ()).map_err(|e| {
        GitError::new(
            "stat .git marker",
            format!("resolving .git marker in {}: {e}", worktree.display()),
            GitErrorKind::Other,
        )
    })
}

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
        self.run_os(cwd, args.iter().map(|s| OsString::from(*s)).collect())
    }

    /// The spawn itself, with arguments already converted to the OS form.
    fn run_os(&self, cwd: Option<&Path>, args: Vec<OsString>) -> Output {
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
        checked(output, format!("git {}", args.join(" ")), kind)
    }

    /// [`Self::run_checked`] for argument lists that carry a caller-supplied
    /// path. Only the `worktree` subcommands take one, and they are exactly
    /// the ones where a degraded path argument reaches a destructive git
    /// command — hence the separate entry point rather than a generic
    /// conversion that would touch every call site.
    fn run_checked_args(
        &self,
        cwd: Option<&Path>,
        args: &[Arg<'_>],
        kind: GitErrorKind,
    ) -> Result<(), GitError> {
        let command = args
            .iter()
            .map(|a| a.to_os_string().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let output = self.run_os(cwd, args.iter().map(Arg::to_os_string).collect());
        checked(output, format!("git {command}"), kind)
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

    /// Runs git and returns RAW stdout bytes (never lossy-converted).
    ///
    /// Needed wherever git's output is a protocol rather than text: the
    /// NUL-separated records of `ls-files -z`, and the blob of a committed
    /// manifest. `String::from_utf8_lossy` would replace a non-UTF-8 byte with
    /// U+FFFD, and a path that survives that substitution is a path the copy
    /// then cannot open — or worse, one that names a DIFFERENT file than the
    /// one git selected.
    fn run_bytes(&self, cwd: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, GitError> {
        self.run_bytes_os(cwd, args.iter().map(|s| OsString::from(*s)).collect())
    }

    /// [`Self::run_bytes`] for an argument list carrying a caller-supplied path
    /// (the temporary `--exclude-from` file is written from a manifest, so its
    /// path is not a compile-time literal).
    fn run_bytes_os(&self, cwd: Option<&Path>, args: Vec<OsString>) -> Result<Vec<u8>, GitError> {
        let command = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let output = self.run_os(cwd, args);
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(GitError::new(format!("git {command}"), message, GitErrorKind::Other));
        }
        Ok(output.stdout)
    }
}

/// Splits git's NUL-separated record output into owned paths.
///
/// A trailing NUL is the record terminator, not an empty final record; a
/// missing one (empty output) yields no records at all rather than one empty
/// path that would later be validated and rejected.
fn split_nul(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let body = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    String::from_utf8_lossy(body)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Shared exit-code check: success is `Ok`, anything else is the stderr text
/// wrapped with the command line that produced it (the `command` half of a
/// [`GitError`], so operators can see what actually ran).
fn checked(output: Output, command: String, kind: GitErrorKind) -> Result<(), GitError> {
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(GitError::new(command, message, kind))
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

/// The committed seed manifest's filename (Go `worktreeIncludeName`).
const WORKTREE_INCLUDE: &str = ".worktreeinclude";

impl ShellGitBackend {
    /// Reads the blob of `.worktreeinclude` at the destination worktree's HEAD.
    ///
    /// Only the COMMITTED blob is read, never a working-tree file: the manifest
    /// decides which of the user's ignored files get copied into a new
    /// checkout, and a repo that is dirty or mid-merge must not be able to widen
    /// that selection with an uncommitted edit. A missing file is a no-op, not
    /// an error — most repos do not seed at all.
    fn committed_worktree_include(&self, worktree: &Path) -> Result<Option<Vec<u8>>, GitError> {
    let listed = self.run_bytes(
        Some(worktree),
        &[
            "ls-tree",
            "-z",
            "--name-only",
            "--full-tree",
            "HEAD",
            "--",
            WORKTREE_INCLUDE,
        ],
    )?;
    if !split_nul(&listed).iter().any(|n| n == WORKTREE_INCLUDE) {
        return Ok(None);
    }
    // A tree entry of the same name (a directory, say) must not be read as a
    // manifest: `cat-file blob` would fail with a confusing message, and
    // treating the failure as "no manifest" would hide a real misconfiguration.
    let spec = format!("HEAD:{WORKTREE_INCLUDE}");
    let kind = self.run_stdout(
        Some(worktree),
        &["cat-file", "-t", &spec],
        GitErrorKind::Other,
    )?;
    if kind.trim() != "blob" {
        return Err(GitError::new(
            format!("git cat-file -t {spec}"),
            format!("committed {WORKTREE_INCLUDE} is not a file"),
            GitErrorKind::Other,
        ));
    }
    Ok(Some(self.run_bytes(
        Some(worktree),
        &["cat-file", "blob", &spec],
    )?))
}

    /// The ignored paths a manifest selects in `repo` (Go `selectedSeedPathsWithManifestEnv`).
    ///
    /// Git itself evaluates the pattern language, by handing the manifest to
    /// `ls-files` as an exclude file. Reimplementing gitignore matching in Rust
    /// would silently diverge on negation, escaping, and `**`, and the divergence
    /// would show up as "the wrong files got copied" — so the patterns are given
    /// to the tool that owns them.
    ///
    /// The second `ls-files` pass is the security-relevant one: it intersects
    /// the manifest's selections with the paths the repository ACTUALLY ignores.
    /// A manifest that names a tracked file selects nothing, so
    /// `.worktreeinclude` can never be used to smuggle a tracked file's content
    /// into a worktree.
    fn selected_seed_paths(
        &self,
        repo_root: &Path,
        manifest: &[u8],
    ) -> Result<Vec<String>, GitError> {
    // The temp file must live and be written before `ls-files` reads it, and
    // must not outlive the call: it holds repository-selection content in a
    // predictable path, so it is removed on every path out.
    let exclude = tempfile::NamedTempFile::new().map_err(|e| {
        GitError::new(
            "git ls-files --exclude-from",
            format!("creating temporary exclude file: {e}"),
            GitErrorKind::Other,
        )
    })?;
    std::fs::write(exclude.path(), manifest).map_err(|e| {
        GitError::new(
            "git ls-files --exclude-from",
            format!("writing temporary exclude file: {e}"),
            GitErrorKind::Other,
        )
    })?;

    let exclude_from = format!("--exclude-from={}", exclude.path().display());
    let selected = self.run_bytes(
        Some(repo_root),
        &["ls-files", "-z", "--others", "--ignored", &exclude_from],
    )?;
    if selected.is_empty() {
        return Ok(Vec::new());
    }

    let ignored = self.run_bytes(
        Some(repo_root),
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
        ],
    )?;
    let ignored: std::collections::HashSet<String> = split_nul(&ignored).into_iter().collect();

    Ok(split_nul(&selected)
        .into_iter()
        .filter(|name| ignored.contains(name))
        .collect())
    }
}

/// Whether the destination worktree tracks `name` or anything at or beneath it.
///
/// Checked in the DESTINATION, not the source: the worktree may be cut from a
/// different commit than the source repo's current checkout, so the source's
/// index says nothing about what this worktree already has. A seed must never
/// overwrite tracked content — that is a working-tree edit dressed up as a copy.
fn destination_tracks(tracked: &[String], name: &str) -> bool {
    if tracked.iter().any(|t| t == name) {
        return true;
    }
    // A tracked DESCENDANT counts: the manifest selected a directory-shaped
    // pattern, and something inside it is tracked at the destination.
    let prefix = format!("{name}/");
    if tracked.iter().any(|t| t.starts_with(&prefix)) {
        return true;
    }
    // So does a tracked ANCESTOR: `config` is not itself listed, but
    // `config/settings.env` is, and seeding a file over it would clobber it.
    let mut ancestor = name;
    while let Some((head, _)) = ancestor.rsplit_once('/') {
        if tracked.iter().any(|t| t == head) {
            return true;
        }
        ancestor = head;
    }
    false
}

/// Creates `worktree/rel`'s parent directories, refusing to replace a file.
///
/// A seed whose ancestor exists as a regular file is refused rather than
/// removed: the alternative is deleting something the user (or git) put there
/// to make room for a copy.
fn ensure_seed_parent_dir(worktree: &Path, rel: &str) -> Result<(), GitError> {
    let Some((parent, _)) = rel.rsplit_once('/') else {
        return Ok(()); // Top-level seed: the worktree root is its parent.
    };
    let mut current = worktree.to_path_buf();
    for part in parent.split('/') {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() => continue,
            Ok(_) => {
                return Err(GitError::new(
                    "seed worktree",
                    format!("refusing to replace existing seed ancestor {}", current.display()),
                    GitErrorKind::Other,
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|e| {
                    GitError::new(
                        "seed worktree",
                        format!("creating {}: {e}", current.display()),
                        GitErrorKind::Other,
                    )
                })?;
            }
            Err(e) => {
                return Err(GitError::new(
                    "seed worktree",
                    format!("reading {}: {e}", current.display()),
                    GitErrorKind::Other,
                ));
            }
        }
    }
    Ok(())
}

/// Refuses a worktree path that is not a plain directory.
///
/// Seeded content is written BELOW this path, so a symlinked destination would
/// redirect every write outside the pool — into the user's real repository, or
/// anywhere else the link points.
fn require_plain_worktree_dir(worktree: &Path) -> Result<(), GitError> {
    let meta = std::fs::symlink_metadata(worktree).map_err(|e| {
        GitError::new(
            "seed worktree",
            format!("reading worktree {}: {e}", worktree.display()),
            GitErrorKind::Other,
        )
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(GitError::new(
            "seed worktree",
            format!("refusing to seed symlinked worktree {}", worktree.display()),
            GitErrorKind::Other,
        ));
    }
    Ok(())
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

    fn remote_url(&self, repo: &GitRepo, name: &str) -> Option<String> {
        self.run_stdout(
            Some(&repo.common_dir),
            &["remote", "get-url", name],
            GitErrorKind::Other,
        )
        .ok()
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
        self.run_checked_args(
            Some(&repo.common_dir),
            &[
                Arg::Lit("worktree"),
                Arg::Lit("add"),
                Arg::Lit("--detach"),
                Arg::Path(path),
                Arg::Lit(&ref_),
            ],
            GitErrorKind::Other,
        )
    }

    fn worktree_remove(&self, repo: &GitRepo, path: &Path) -> Result<(), GitError> {
        self.run_checked_args(
            Some(&repo.common_dir),
            &[
                Arg::Lit("worktree"),
                Arg::Lit("remove"),
                Arg::Lit("--force"),
                Arg::Path(path),
            ],
            GitErrorKind::Other,
        )
    }

    fn remove_clean_worktree(&self, repo: &GitRepo, path: &Path) -> Result<(), GitError> {
        self.run_checked_args(
            Some(&repo.common_dir),
            &[Arg::Lit("worktree"), Arg::Lit("remove"), Arg::Path(path)],
            GitErrorKind::Other,
        )
    }

    fn create_branch(&self, worktree: &Path, branch: &str) -> Result<(), GitError> {
        // Marker FIRST, before any dispatch on this path. `git branch` +
        // `git checkout` are only ever meant for the checkout named by
        // `worktree`; against a markerless path git walks UP to the enclosing
        // repository and would create and switch a branch in the user's own
        // working tree.
        require_worktree_marker(worktree)?;

        // Create at the worktree's CURRENT commit, verified first. Going
        // through `rev-parse` means the branch is pinned to a known commit
        // rather than to whatever HEAD reports between two commands.
        let expected_head = self.run_stdout(
            Some(worktree),
            &["rev-parse", "--verify", "HEAD^{commit}"],
            GitErrorKind::Other,
        )?;

        // `--` ends option parsing: a name beginning with `-` is then a branch,
        // not a flag. Git refuses an existing ref, including one created
        // concurrently, so this never adopts a caller's branch.
        self.run_checked_args(
            Some(worktree),
            &[
                Arg::Lit("branch"),
                Arg::Lit("--"),
                Arg::Lit(branch),
                Arg::Lit(&expected_head),
            ],
            GitErrorKind::Other,
        )?;

        let checkout = self.run(Some(worktree), &["checkout", branch]);
        if !checkout.status.success() {
            return Err(GitError::new(
                format!("git checkout {branch}"),
                String::from_utf8_lossy(&checkout.stderr).trim().to_string(),
                GitErrorKind::Other,
            ));
        }

        // Exit status alone is not authoritative: a post-checkout hook can fail
        // after checkout, or succeed after switching HEAD somewhere else. Both
        // the branch NAME and the COMMIT must still match the acquisition.
        let checked_out = self.run_stdout(
            Some(worktree),
            &["symbolic-ref", "-q", "--short", "HEAD"],
            GitErrorKind::Other,
        )?;
        let head = self.run_stdout(
            Some(worktree),
            &["rev-parse", "--verify", "HEAD^{commit}"],
            GitErrorKind::Other,
        )?;
        if checked_out != branch || head != expected_head {
            return Err(GitError::new(
                format!("git checkout {branch}"),
                format!(
                    "checkout did not leave HEAD on {branch:?} at commit {expected_head} \
                     (on {checked_out:?} at {head})"
                ),
                GitErrorKind::Other,
            ));
        }
        Ok(())
    }

    fn local_branch_exists(&self, repo: &GitRepo, branch: &str) -> bool {
        let out = self.run(
            Some(&repo.common_dir),
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        );
        out.status.success()
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
        // Precondition FIRST, before anything can resolve the path upward.
        // `checkout --force` / `reset --hard` / `clean -fd` are only ever meant
        // for the checkout named by `worktree`; with the marker gone, git
        // walks up to the enclosing repository and these three commands destroy
        // the user's real uncommitted work there.
        require_worktree_marker(worktree)?;
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
        // Same precondition as `reset_worktree`: `checkout --detach` on a
        // markerless path detaches the ENCLOSING repository's HEAD.
        require_worktree_marker(worktree)?;
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

    fn common_git_dir(&self, start: &Path) -> Result<PathBuf, GitError> {
        // --path-format=absolute lands on Git ≥2.31. Older git rejects the
        // flag outright, so the fallback is a second invocation rather than a
        // parse of its stderr: a relative answer is then resolved against
        // `start`, which is where git resolved it from.
        let out = match self.run_stdout(
            Some(start),
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            GitErrorKind::Other,
        ) {
            Ok(out) => out,
            Err(_) => self.run_stdout(
                Some(start),
                &["rev-parse", "--git-common-dir"],
                GitErrorKind::Other,
            )?,
        };
        let path = PathBuf::from(out.trim());
        if path.is_absolute() {
            Ok(path)
        } else {
            Ok(start.join(path))
        }
    }

    fn seed_worktree(
        &self,
        repo: &GitRepo,
        worktree: &Path,
        manifest: Option<&[u8]>,
    ) -> Result<Vec<String>, GitError> {
        // The destination is validated BEFORE anything is read or written. A
        // symlinked worktree would redirect every seeded write outside the pool,
        // and running git inside it first would resolve that link and report
        // whatever repository it points at — an error message that names the
        // wrong problem for the operator who has to fix it.
        require_plain_worktree_dir(worktree)?;

        // `None` selects the committed manifest; `Some` REPLACES it. An empty
        // `Some` is therefore "seed nothing" and must not fall back — a caller
        // that computed an empty selection means it, and reading the committed
        // manifest instead would copy files the caller excluded.
        let manifest: Vec<u8> = match manifest {
            Some(bytes) => bytes.to_vec(),
            None => self.committed_worktree_include(worktree)?.unwrap_or_default(),
        };
        if manifest.is_empty() {
            return Ok(Vec::new());
        }

        let selected = self.selected_seed_paths(&repo.common_dir, &manifest)?;
        if selected.is_empty() {
            return Ok(Vec::new());
        }

        // What the DESTINATION already tracks, which seeding must not touch.
        let tracked = split_nul(&self.run_bytes(Some(worktree), &["ls-files", "-z"])?);
        let selected: Vec<String> = selected
            .into_iter()
            .filter(|name| !destination_tracks(&tracked, name))
            .collect();
        if selected.is_empty() {
            return Ok(Vec::new());
        }

        let mut copied: Vec<String> = Vec::new();
        for name in selected {
            let source = repo.common_dir.join(&name);
            let meta = std::fs::symlink_metadata(&source).map_err(|e| {
                GitError::new(
                    "seed worktree",
                    format!("reading {}: {e}", source.display()),
                    GitErrorKind::Other,
                )
            })?;

            // A symlink is FLATTENED into a regular file holding its target,
            // rather than recreated as a link. Copying the link would put a
            // dangling or escaping reference into the worktree; copying the
            // target text keeps the seed data-only, with nothing to follow.
            let (data, mode) = if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&source).map_err(|e| {
                    GitError::new(
                        "seed worktree",
                        format!("reading link {}: {e}", source.display()),
                        GitErrorKind::Other,
                    )
                })?;
                (target.to_string_lossy().into_owned().into_bytes(), 0o666)
            } else if meta.is_file() {
                (
                    std::fs::read(&source).map_err(|e| {
                        GitError::new(
                            "seed worktree",
                            format!("reading {}: {e}", source.display()),
                            GitErrorKind::Other,
                        )
                    })?,
                    mode_of(&meta),
                )
            } else {
                // Directories and special files are not seeds. `ls-files`
                // lists files, so this is unreachable in practice; skipping is
                // the safe reading of anything unexpected.
                continue;
            };

            ensure_seed_parent_dir(worktree, &name)?;

            let dest = worktree.join(&name);
            // create_new == O_EXCL: never clobber a file that appeared between
            // the selection and this write.
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest)
                .map_err(|e| {
                    GitError::new(
                        "seed worktree",
                        format!("creating {}: {e}", dest.display()),
                        GitErrorKind::Other,
                    )
                })?;
            // Recorded BEFORE the write completes: a path that exists belongs
            // in the cleanup inventory even if writing it then fails, or the
            // next reset would leave an unrecorded file behind forever.
            copied.push(name.clone());
            std::io::Write::write_all(&mut file, &data).map_err(|e| {
                GitError::new(
                    "seed worktree",
                    format!("writing {}: {e}", dest.display()),
                    GitErrorKind::Other,
                )
            })?;
            drop(file);
            set_mode(&dest, mode).map_err(|e| {
                GitError::new(
                    "seed worktree",
                    format!("setting mode on {}: {e}", dest.display()),
                    GitErrorKind::Other,
                )
            })?;
        }
        Ok(copied)
    }

    fn reset_worktree_with_seeded_paths(
        &self,
        worktree: &Path,
        branch: &str,
        seeded: &[String],
    ) -> Result<(), GitError> {
        // The inventory is validated BEFORE anything else happens, including
        // before the reset itself. An empty or malformed inventory must
        // authorize zero deletions — it means the caller lost its bookkeeping,
        // and treating that as "delete nothing silently" would leave the
        // reset looking successful while the pool's own seeds accumulate.
        crate::vcs::validate_seed_inventory(seeded)?;
        require_worktree_marker(worktree)?;
        require_plain_worktree_dir(worktree)?;

        // Seeds are removed FIRST, while the worktree is still known to be the
        // one that was checked, and BEFORE the tracked tree is rewritten: a
        // tracked path can only appear at a seeded path after `read-tree -u`
        // restores it, and that ordering is what makes the inventory the
        // authority on what may be deleted.
        for name in seeded {
            let target = worktree.join(name);
            match std::fs::symlink_metadata(&target) {
                Ok(meta) if meta.is_dir() => {
                    std::fs::remove_dir_all(&target).map_err(|e| {
                        GitError::new(
                            "reset worktree",
                            format!("removing seeded directory {}: {e}", target.display()),
                            GitErrorKind::Other,
                        )
                    })?;
                }
                Ok(_) => {
                    std::fs::remove_file(&target).map_err(|e| {
                        GitError::new(
                            "reset worktree",
                            format!("removing seeded file {}: {e}", target.display()),
                            GitErrorKind::Other,
                        )
                    })?;
                }
                // Already gone: nothing to remove, and NOT a failure. The
                // inventory is a permission to delete, not an obligation.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(GitError::new(
                        "reset worktree",
                        format!("reading seeded path {}: {e}", target.display()),
                        GitErrorKind::Other,
                    ));
                }
            }
        }

        self.reset_worktree(worktree, branch)
    }
}

/// The permission bits of a metadata value (Go `info.Mode().Perm()`).
#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

/// Applies permission bits, a no-op where the platform has none.
///
/// Best effort by design: a seed that copied correctly but could not be
/// chmod'ed is still a usable seed, and failing the whole acquisition over it
/// would leave the worktree half-seeded.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
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

    // ---- M-002: fail closed on a pool slot that lost its .git marker ----
    //
    // The scenario these reproduce is an IN-PROJECT pool: a supported layout
    // (config.rs nests `.treehouse` under the repo when `root` is relative),
    // in which a slot whose `.git` marker was deleted sits INSIDE the user's
    // real repository. Git then walks up from the slot, resolves against the
    // enclosing repo, and `reset --hard` + `clean -fd` destroy the user's
    // uncommitted work there. Go refuses this at vcs.go:513-525.

    /// Creates a pool-slot-shaped directory INSIDE `repo`: a plain directory
    /// with no `.git` marker at all, exactly what remains after the marker of
    /// a torn-down worktree is deleted.
    fn markerless_slot(repo: &Path) -> PathBuf {
        let slot = repo.join(".treehouse").join("wt-1");
        std::fs::create_dir_all(&slot).unwrap();
        slot
    }

    #[test]
    fn integration_reset_worktree_refuses_markerless_slot() {
        let (_dir, repo) = temp_repo();
        let slot = markerless_slot(&repo);
        let backend = ShellGitBackend::discover().unwrap();

        // The enclosing repo carries work that MUST survive the refusal.
        std::fs::write(repo.join("README.md"), b"uncommitted edit\n").unwrap();
        std::fs::write(repo.join("scratch.txt"), b"untracked\n").unwrap();

        let err = backend
            .reset_worktree(&slot, "main")
            .expect_err("reset must refuse a slot with no .git marker");
        assert!(
            err.message.contains("refusing to modify"),
            "error must state the refusal, got: {}",
            err.message
        );
        assert!(
            err.message.contains(&slot.display().to_string()),
            "error must name the offending path, got: {}",
            err.message
        );

        // The real proof: nothing in the ENCLOSING repo was touched.
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).unwrap(),
            "uncommitted edit\n",
            "reset --hard must not have run against the enclosing repo"
        );
        assert!(
            repo.join("scratch.txt").exists(),
            "clean -fd must not have run against the enclosing repo"
        );
    }

    #[test]
    fn integration_detach_worktree_refuses_markerless_slot() {
        let (_dir, repo) = temp_repo();
        let slot = markerless_slot(&repo);
        let backend = ShellGitBackend::discover().unwrap();

        let branch_before = backend
            .run_stdout(
                Some(&repo),
                &["symbolic-ref", "--short", "HEAD"],
                GitErrorKind::Other,
            )
            .unwrap();

        let err = backend
            .detach_worktree(&slot)
            .expect_err("detach must refuse a slot with no .git marker");
        assert!(
            err.message.contains("refusing to modify"),
            "error must state the refusal, got: {}",
            err.message
        );

        let branch_after = backend
            .run_stdout(
                Some(&repo),
                &["symbolic-ref", "--short", "HEAD"],
                GitErrorKind::Other,
            )
            .unwrap();
        assert_eq!(
            branch_after, branch_before,
            "checkout --detach must not have run against the enclosing repo"
        );
    }

    /// Go `markerPresent` counts a dangling symlink as PRESENT and surfaces
    /// the unresolvable target as a read failure — never as an absent marker,
    /// which would let the caller fall through to the enclosing repository.
    #[test]
    #[cfg(unix)]
    fn integration_markerless_check_rejects_dangling_git_symlink() {
        let (_dir, repo) = temp_repo();
        let slot = markerless_slot(&repo);
        std::os::unix::fs::symlink(repo.join(".git").join("does-not-exist"), slot.join(".git"))
            .unwrap();

        let err = require_worktree_marker(&slot).expect_err("dangling marker must be refused");
        assert!(
            err.message.starts_with("resolving .git marker in"),
            "a dangling symlink is a read failure, not an absent marker; got: {}",
            err.message
        );
    }

    /// The guard must not be so strict that it breaks the real thing: a linked
    /// worktree's `.git` is a FILE, and that is the normal pool-slot shape.
    /// (`integration_reset_worktree_cleans_dirty_changes` covers the happy path
    /// too; this pins the file-vs-directory contract explicitly.)
    #[test]
    fn integration_marker_check_accepts_dot_git_file_and_directory() {
        let (_dir, repo) = temp_repo();
        let wt = repo.join("wt");
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );

        let marker = wt.join(".git");
        assert!(
            marker.is_file(),
            "a linked worktree's .git is a file, and the guard must accept it"
        );
        require_worktree_marker(&wt).expect("linked worktree must pass the marker check");
        require_worktree_marker(&repo).expect("primary checkout's .git dir must pass too");
    }

    #[test]
    fn integration_marker_check_refuses_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = require_worktree_marker(&dir.path().join("nope"))
            .expect_err("a path that does not exist has no marker");
        assert!(
            err.message.contains("refusing to modify"),
            "got: {}",
            err.message
        );
    }

    // ── .worktreeinclude seeding ─────────────────────────────────────────────

    /// A repo with a committed `.gitignore` (`*`, so everything is ignored) and
    /// a committed `.worktreeinclude` manifest, plus a detached worktree.
    /// Returns `(TempDir, repo, worktree)`; the manifest is `""` for none.
    fn seed_repo(manifest: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let worktree = dir.path().join("worktree");
        must_git(
            None,
            &["init", "--initial-branch=main", repo.to_str().unwrap()],
        );
        must_git(Some(&repo), &["config", "user.email", "test@test.com"]);
        must_git(Some(&repo), &["config", "user.name", "Test"]);
        std::fs::write(repo.join(".gitignore"), b"*\n").unwrap();
        std::fs::write(repo.join(".worktreeinclude"), manifest.as_bytes()).unwrap();
        // -f: the manifest is itself ignored by `.gitignore` containing `*`,
        // and it still has to be committed — it is the one file seeding reads
        // from the OBJECT store, never the working tree.
        must_git(Some(&repo), &["add", "-f", ".gitignore", ".worktreeinclude"]);
        must_git(Some(&repo), &["commit", "-m", "seed manifest"]);
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", worktree.to_str().unwrap()],
        );
        (dir, repo, worktree)
    }

    /// Writes `contents` to `root/name`, creating parent directories.
    fn write_under(root: &Path, name: &str, contents: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// The end-to-end shape: seed a worktree, then reset it with the recorded
    /// inventory, and prove both halves of the contract.
    ///
    /// The last assertion is the one that matters. `clean -fd` does not remove
    /// IGNORED files, so a reset would leave the seed behind whether or not the
    /// inventory did anything — and a naive implementation that simply ran the
    /// reset would pass the first check while never actually cleaning anything.
    /// The user-placed file is the mirror image: it must survive because it is
    /// not in the inventory, and it would also survive a correct implementation.
    /// Together they pin that the deletion is driven by the inventory and by
    /// nothing else.
    #[test]
    fn integration_seed_then_reset_removes_only_the_recorded_seeds() {
        let (_dir, repo, worktree) = seed_repo("*.env\n");
        let backend = ShellGitBackend::discover().unwrap();
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };

        // An ignored file the manifest selects, and one it does not.
        write_under(&repo, ".env", "SECRET=1\n");
        write_under(&repo, "notes.txt", "not selected\n");

        let seeded = backend
            .seed_worktree(&repo_ref, &worktree, None)
            .expect("seeding a committed manifest must succeed");

        assert_eq!(
            seeded,
            vec![".env".to_string()],
            "the inventory must record exactly what was copied"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join(".env")).unwrap(),
            "SECRET=1\n",
            "the selected ignored file must be seeded"
        );
        assert!(
            !worktree.join("notes.txt").exists(),
            "a file the manifest does not select must not be seeded"
        );

        // A file the USER put in the worktree after acquisition. Ignored, and
        // matched by the same `*.env` pattern, but never in the inventory.
        write_under(&worktree, "user.env", "MINE=1\n");

        backend
            .reset_worktree_with_seeded_paths(&worktree, "main", &seeded)
            .expect("reset with a trusted inventory must succeed");

        assert!(
            !worktree.join(".env").exists(),
            "the recorded seed must be removed by the reset"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("user.env")).unwrap(),
            "MINE=1\n",
            "an ignored file the user placed must SURVIVE: the inventory is a \
             permission to delete specific paths, never a licence to sweep \
             ignored files"
        );
    }

    /// An inventory that is absent, or one naming a path outside the worktree,
    /// must authorize ZERO deletions. This is the fail-closed property the
    /// whole design turns on: a lost or tampered state file must never become
    /// "delete the user's ignored files".
    #[test]
    fn integration_reset_refuses_an_untrustworthy_inventory() {
        let (_dir, repo, worktree) = seed_repo("*.env\n");
        let backend = ShellGitBackend::discover().unwrap();
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };
        write_under(&repo, ".env", "SECRET=1\n");
        let seeded = backend.seed_worktree(&repo_ref, &worktree, None).unwrap();
        assert_eq!(seeded, vec![".env".to_string()]);
        // A second ignored file that is NOT in the inventory.
        write_under(&worktree, "user.env", "MINE=1\n");

        for bad in [
            vec![],                              // lost bookkeeping
            vec!["".to_string()],                // empty component
            vec!["../escape.env".to_string()],   // climbs out of the worktree
            vec!["/etc/passwd".to_string()],     // absolute
            vec![".git/config".to_string()],     // the repository's own metadata
            vec!["nested/../../out".to_string()], // traversal mid-path
        ] {
            backend
                .reset_worktree_with_seeded_paths(&worktree, "main", &bad)
                .expect_err("an untrustworthy inventory must be refused");
            assert!(
                worktree.join("user.env").exists() && worktree.join(".env").exists(),
                "a refused reset must delete nothing, got inventory {bad:?}"
            );
        }
    }

    /// Git owns the manifest's pattern language. These are the cases a
    /// hand-rolled matcher gets wrong, so they are pinned against git itself.
    #[test]
    fn integration_seed_honours_git_exclude_semantics() {
        for (manifest, want, unwanted) in [
            // Negation: a later pattern re-includes a file.
            ("*.env\n!important.env\n", vec!["app.env"], vec!["important.env"]),
            // Anchored: `/` binds to the repo root.
            ("/root.env\n", vec!["root.env"], vec!["nested/root.env"]),
            // `**` spanning directories.
            (
                "build/**/cache/*.json\n",
                vec!["build/cache/a.json", "build/one/two/cache/b.json"],
                vec!["build/one/data.json"],
            ),
            // Comments and spaces are part of the pattern language.
            (
                "# a comment\nname with space\n",
                vec!["name with space"],
                vec!["a comment"],
            ),
        ] {
            let (_dir, repo, worktree) = seed_repo(manifest);
            let backend = ShellGitBackend::discover().unwrap();
            let repo_ref = GitRepo {
                common_dir: repo.clone(),
                worktree: None,
            };
            for name in want.iter().chain(unwanted.iter()) {
                write_under(&repo, name, "seeded\n");
            }

            let seeded = backend
                .seed_worktree(&repo_ref, &worktree, None)
                .unwrap_or_else(|e| panic!("seeding {manifest:?} failed: {e}"));

            for name in &want {
                assert!(
                    worktree.join(name).exists(),
                    "{manifest:?} must select {name}"
                );
            }
            for name in &unwanted {
                assert!(
                    !worktree.join(name).exists(),
                    "{manifest:?} must NOT select {name}"
                );
            }
            // The inventory is the record of what was written, so it must match
            // what actually landed on disk.
            for name in &want {
                assert!(
                    seeded.iter().any(|p| p == name),
                    "{name} must appear in the returned inventory for {manifest:?}"
                );
            }
        }
    }

    /// A TRACKED file named by the manifest must not be seeded. Source ignore
    /// rules say nothing about the destination's index, and the destination may
    /// be cut from a different commit — so a manifest that names a tracked path
    /// selects nothing rather than overwriting it.
    #[test]
    fn integration_seed_never_overwrites_tracked_content() {
        let (_dir, repo, worktree) = seed_repo("*.env\n");
        let backend = ShellGitBackend::discover().unwrap();
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };

        // Tracked files the manifest would otherwise select: one named exactly,
        // one beneath a selected path. `-f` because `.gitignore` is `*`.
        write_under(&repo, "config/settings.env", "COMMITTED\n");
        write_under(&repo, "loose.env", "COMMITTED\n");
        must_git(Some(&repo), &["add", "-f", "config/settings.env", "loose.env"]);
        must_git(Some(&repo), &["commit", "-m", "tracked"]);

        // Change the SOURCE copy of the tracked file without committing it. If
        // seeding copies from the working tree, this newer content lands in the
        // worktree and the assertion below catches it.
        write_under(&repo, "loose.env", "SOURCE-DIRTIED\n");

        // Re-cut the worktree at the new HEAD, so the destination index is what
        // decides what seeding may not touch. A DIFFERENT commit is the point:
        // source ignore rules say nothing about the destination's index.
        must_git(
            Some(&repo),
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", worktree.to_str().unwrap()],
        );

        // A genuinely ignored file, so the test distinguishes "seeded nothing
        // at all" from "skipped exactly the tracked ones".
        write_under(&repo, "real.env", "IGNORED\n");

        let seeded = backend
            .seed_worktree(&repo_ref, &worktree, None)
            .expect("seeding must succeed");

        // The tracked files DO exist in the worktree — git checked them out. What
        // must not have happened is a copy over the top of them, so the
        // assertion is on content: seeding writes the SOURCE's bytes, and
        // these two differ between source and committed content below.
        assert_eq!(
            std::fs::read_to_string(worktree.join("loose.env")).unwrap(),
            "COMMITTED\n",
            "a tracked file the manifest names must keep its committed content"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("config/settings.env")).unwrap(),
            "COMMITTED\n",
            "a tracked file beneath a selected path must keep its committed content"
        );
        assert!(
            worktree.join("real.env").exists(),
            "a genuinely ignored file must still be seeded"
        );
        assert!(
            !seeded.iter().any(|p| p == "loose.env" || p == "config/settings.env"),
            "tracked paths must not reach the inventory, got {seeded:?}"
        );
    }

    /// No committed manifest, an empty one, and an explicit empty override must
    /// all seed nothing — and an explicit override must not fall back to the
    /// committed manifest, or a caller that computed "select nothing" would get
    /// the repository's selections instead.
    #[test]
    fn integration_seed_is_a_no_op_without_a_manifest() {
        let (_dir, repo, worktree) = seed_repo("*.env\n");
        let backend = ShellGitBackend::discover().unwrap();
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };
        write_under(&repo, ".env", "SECRET=1\n");

        // An explicit EMPTY override selects nothing even though the committed
        // manifest would select `.env`.
        assert!(
            backend
                .seed_worktree(&repo_ref, &worktree, Some(b""))
                .unwrap()
                .is_empty(),
            "an explicitly empty manifest must select nothing, not fall back"
        );
        assert!(
            !worktree.join(".env").exists(),
            "an empty override must not fall back to the committed manifest"
        );

        // A manifest that is present but empty commits the same no-op.
        assert!(
            backend
                .seed_worktree(&repo_ref, &worktree, Some(b"*.env\n"))
                .unwrap()
                .contains(&".env".to_string())
        );

        // A repo with no `.worktreeinclude` at all seeds nothing and does not
        // error — most repos do not seed.
        let (_d2, repo2, worktree2) = seed_repo("");
        must_git(Some(&repo2), &["rm", "--cached", "-f", ".worktreeinclude"]);
        must_git(Some(&repo2), &["commit", "-m", "drop manifest"]);
        write_under(&repo2, ".env", "SECRET=1\n");
        let repo_ref2 = GitRepo {
            common_dir: repo2.clone(),
            worktree: None,
        };
        let backend2 = ShellGitBackend::discover().unwrap();
        // The destination worktree was cut before the manifest was dropped, so
        // rebuild it to read the current HEAD.
        must_git(Some(&repo2), &["worktree", "remove", "--force", worktree2.to_str().unwrap()]);
        must_git(
            Some(&repo2),
            &["worktree", "add", "--detach", worktree2.to_str().unwrap()],
        );
        assert!(
            backend2
                .seed_worktree(&repo_ref2, &worktree2, None)
                .unwrap()
                .is_empty(),
            "a repo with no committed manifest must seed nothing"
        );
    }

    /// The manifest is read from the committed blob only. An uncommitted edit to
    /// `.worktreeinclude` must not widen what gets copied into a worktree —
    /// that would let a dirty or mid-merge repo select files the commit never
    /// authorized.
    #[test]
    fn integration_seed_ignores_an_uncommitted_manifest() {
        let (_dir, repo, worktree) = seed_repo("");
        let backend = ShellGitBackend::discover().unwrap();
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };
        write_under(&repo, ".env", "SECRET=1\n");
        // Uncommitted: selects `.env`, but was never committed.
        std::fs::write(repo.join(".worktreeinclude"), b"*.env\n").unwrap();

        assert!(
            backend
                .seed_worktree(&repo_ref, &worktree, None)
                .unwrap()
                .is_empty(),
            "an uncommitted .worktreeinclude must not authorize any seed"
        );
        assert!(
            !worktree.join(".env").exists(),
            "the uncommitted manifest must not have been honored"
        );
    }

    /// Seeding writes below `worktree`, so a SYMLINKED destination would
    /// redirect every copy outside the pool — into the user's real repository,
    /// or anywhere else the link points.
    #[test]
    #[cfg(unix)]
    fn integration_seed_refuses_a_symlinked_worktree() {
        let (_dir, repo, worktree) = seed_repo("*.env\n");
        let backend = ShellGitBackend::discover().unwrap();
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };
        write_under(&repo, ".env", "SECRET=1\n");

        // Replace the worktree with a symlink to a directory OUTSIDE the pool.
        // `worktree remove` already deletes the directory, so nothing is left
        // to remove — only the path to re-point at the link.
        let elsewhere = tempfile::tempdir().unwrap();
        must_git(
            Some(&repo),
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );
        std::os::unix::fs::symlink(elsewhere.path(), &worktree).unwrap();

        let err = backend
            .seed_worktree(&repo_ref, &worktree, None)
            .expect_err("a symlinked worktree must be refused");
        assert!(
            err.message.contains("symlinked worktree"),
            "the refusal must say why, got: {}",
            err.message
        );
        assert!(
            !elsewhere.path().join(".env").exists(),
            "no seed may be written through a symlinked worktree"
        );
    }

    /// `--git-common-dir` is where the repo-local, untracked `.git/info/exclude`
    /// lives, and an in-project pool is recorded there.
    #[test]
    fn integration_common_git_dir_resolves_the_info_directory() {
        let (_dir, repo) = temp_repo();
        let backend = ShellGitBackend::discover().unwrap();
        let common = backend.common_git_dir(&repo).unwrap();
        assert!(
            common.join("info").is_absolute(),
            "the git dir must be absolute, got {}",
            common.display()
        );
        // For a linked worktree it is the MAIN repo's git dir, not the
        // worktree's private one — that is what makes one exclude file cover
        // every worktree in the pool.
        let wt = repo.join("wt");
        must_git(
            Some(&repo),
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
        let from_worktree = backend.common_git_dir(&wt).unwrap();
        assert_eq!(
            from_worktree, common,
            "a linked worktree must resolve to the owning repository's git dir"
        );
    }

    /// The inventory [`GitBackend::seed_worktree`] returns must survive a round
    /// trip through the pool state file, because it is the ONLY record of what
    /// a later reset is allowed to delete. A build that drops it on write turns
    /// every subsequent reset into either a refusal (inventory lost) or, worse,
    /// an unrecorded set of files nothing will ever clean up.
    ///
    /// The fields are carried in `WorktreeEntry::extra` today (there are no
    /// typed `seeded_paths` fields yet), so this also pins that the flatten map
    /// keeps working for the one field whose absence would be silent.
    #[test]
    fn integration_seeded_inventory_round_trips_through_the_state_file() {
        use crate::state::{State, WorktreeEntry};

        let (_dir, repo, worktree) = seed_repo("*.env\n");
        let backend = ShellGitBackend::discover().unwrap();
        let repo_ref = GitRepo {
            common_dir: repo.clone(),
            worktree: None,
        };
        write_under(&repo, ".env", "SECRET=1\n");

        let seeded = backend.seed_worktree(&repo_ref, &worktree, None).unwrap();
        assert_eq!(seeded, vec![".env".to_string()]);

        // Record the inventory the way an acquisition would, then write the
        // whole state file out and read it back.
        let mut entry = WorktreeEntry {
            name: "1".to_string(),
            path: worktree.to_string_lossy().into_owned(),
            ..Default::default()
        };
        entry.extra.insert(
            "seeded_paths".to_string(),
            serde_json::to_value(&seeded).unwrap(),
        );
        entry.extra.insert("seed_inventory_known".to_string(), true.into());
        let state = State {
            worktrees: vec![entry],
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            json.contains(r#""seeded_paths":[".env"]"#),
            "the inventory must reach the state file, got: {json}"
        );

        let read_back: State = serde_json::from_str(&json).unwrap();
        let recovered: Vec<String> = serde_json::from_value(
            read_back.worktrees[0].extra["seeded_paths"].clone(),
        )
        .expect("seeded_paths must read back as a path list");
        assert_eq!(
            recovered, seeded,
            "an inventory that changes across a state write would either \
             refuse a legitimate reset or delete the wrong path"
        );

        // And the recovered inventory still authorizes exactly the seeded file:
        // a user-placed ignored file added after the write is untouched.
        write_under(&worktree, "user.env", "MINE=1\n");
        backend
            .reset_worktree_with_seeded_paths(&worktree, "main", &recovered)
            .expect("the recovered inventory must still authorize the reset");
        assert!(!worktree.join(".env").exists(), "the seed must be removed");
        assert!(
            worktree.join("user.env").exists(),
            "a user-placed ignored file must still survive a reset driven by \
             the RECOVERED inventory"
        );
    }
}
