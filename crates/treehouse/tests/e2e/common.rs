//! Shared e2e harness for the `tests/e2e/` scenarios.
//!
//! Each scenario file declares `mod common;` and uses these helpers. The
//! harness lives here (not in `tests/e2e_harness.rs`) so Rust treats it as a
//! module of the test, not a separate test binary.

#![allow(dead_code)] // helpers used by future e2e scenario files

use std::path::{Path, PathBuf};
use std::process::Command;

/// The path to the built treehouse binary (workspace target/debug).
pub fn treehouse_bin() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // manifest is crates/treehouse; workspace root is two levels up.
    let workspace = Path::new(&manifest).parent().unwrap().parent().unwrap();
    let target = workspace.join("target/debug");
    let exe = if cfg!(windows) {
        target.join("treehouse.exe")
    } else {
        target.join("treehouse")
    };
    assert!(
        exe.exists(),
        "treehouse binary not built at {}",
        exe.display()
    );
    exe
}

/// Spawn attempts tolerated while a staged binary reports ETXTBSY.
///
/// 200 x 10 ms = 2 s: far longer than a write teardown, far shorter than a CI
/// job's patience.
const ETXTBSY_RETRIES: u32 = 200;
const ETXTBSY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

/// `cmd.output()`, retrying only while the spawn fails with ETXTBSY.
///
/// On Linux `execve(2)` refuses a file that is still open for writing anywhere
/// on the box, so a suite that stages a binary by copying it and immediately
/// executes that copy can lose the race against the copy's own write teardown.
/// Only the suites that stage are exposed: `update_e2e.rs::install_target`
/// writes into a temp dir and execs the result, whereas `e2e.rs` and
/// `bulk_ops_e2e.rs` exec `target/debug/treehouse` straight off disk and never
/// come near this path. Nothing about the treehouse binary is wrong in that
/// failure, so the retry belongs to the harness and nowhere else -- the product
/// installs by renaming (`updater.rs::atomic_replace`), which has no such
/// window.
///
/// Every other error returns on the first attempt, so a genuinely missing or
/// non-executable binary still fails immediately and still names itself.
/// `ErrorKind::ExecutableFileBusy` is stable since Rust 1.83, under this
/// workspace's 1.88 MSRV, and needs no new dependency.
pub fn output_tolerating_etxtbsy(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    let mut attempt = 0;
    loop {
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                assert!(
                    attempt < ETXTBSY_RETRIES,
                    "the binary at {:?} stayed executable-file-busy across {ETXTBSY_RETRIES} \
                     spawn attempts",
                    cmd.get_program()
                );
                attempt += 1;
                std::thread::sleep(ETXTBSY_BACKOFF);
            }
            other => return other,
        }
    }
}

/// Runs the treehouse binary as a subprocess with isolated HOME.
pub fn run(
    bin: &Path,
    repo: &Path,
    home: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> (String, String, i32) {
    run_from(bin, repo, repo, home, extra_env, args)
}

/// Runs the treehouse binary from a specific working directory.
pub fn run_from(
    bin: &Path,
    _repo: &Path,
    work_dir: &Path,
    home: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> (String, String, i32) {
    let mut env = build_env(home);
    for (k, v) in extra_env {
        env.push((k.to_string(), v.to_string()));
    }
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(work_dir);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap_or_else(|e| {
        panic!(
            "failed to run treehouse {args:?} from {}: {e}",
            work_dir.display()
        )
    });
    let code = out.status.code().unwrap_or(-1);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        code,
    )
}

/// The subprocess env: isolated HOME/USERPROFILE, update checks suppressed.
fn build_env(home: &Path) -> Vec<(String, String)> {
    let skip = [
        "HOME",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "TREEHOUSE_DIR",
    ];
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .collect();
    if cfg!(windows) {
        env.push(("USERPROFILE".to_string(), home.display().to_string()));
    } else {
        env.push(("HOME".to_string(), home.display().to_string()));
    }
    env.push(("TREEHOUSE_NO_UPDATE_CHECK".to_string(), "1".to_string()));
    env
}

/// Creates a temp repo (bare remote + one commit on main) and an isolated HOME.
///
/// The backing `TempDir` is leaked so the repo survives for the whole test
/// (a dropped TempDir would delete the repo on return).
pub fn setup() -> (PathBuf, PathBuf) {
    let base = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let base = base.path().to_path_buf();
    // Resolve symlinks on unix so paths match what git rev-parse returns
    // (macOS /tmp -> /private/tmp). On Windows, canonicalize returns a \\?\
    // verbatim prefix that git can't mkdir, so keep the raw path there.
    #[cfg(not(windows))]
    let base = std::fs::canonicalize(&base).unwrap_or(base);
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let bare = base.join("remote.git");
    let repo = base.join("myrepo");

    git(
        None,
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            bare.to_str().unwrap(),
        ],
    );
    git(
        None,
        &["init", "--initial-branch=main", repo.to_str().unwrap()],
    );
    git(Some(&repo), &["config", "user.email", "test@test.com"]);
    git(Some(&repo), &["config", "user.name", "Test"]);
    git(
        Some(&repo),
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    std::fs::write(repo.join("README.md"), b"hello\n").unwrap();
    git(Some(&repo), &["add", "."]);
    git(Some(&repo), &["commit", "-m", "initial commit"]);
    git(Some(&repo), &["push", "-u", "origin", "main"]);
    (repo, home)
}

/// Runs git in `dir`; panics on failure.
pub fn git(dir: Option<&Path>, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir.unwrap_or(Path::new(".")))
        .output()
        .expect("git must be installed");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Parses the worktree path from `treehouse get` stderr (un-prettifies `~`).
pub fn extract_worktree_path(stderr: &str, home: &Path) -> Option<String> {
    const PREFIX: &str = "Entered worktree at ";
    let idx = stderr.find(PREFIX)?;
    let rest = &stderr[idx + PREFIX.len()..];
    let end = rest.find(". Type")?;
    let path = &rest[..end];
    if let Some(stripped) = path.strip_prefix('~') {
        Some(format!("{}{}", home.display(), stripped))
    } else {
        Some(path.to_string())
    }
}

/// Whether output contains a raw git failure (byte-match Go).
pub fn contains_raw_git_failure(output: &str) -> bool {
    output.contains("fatal:")
        || output.contains("not a git repository")
        || output.contains("Could not read from remote repository")
        || output.contains("does not appear to be a git repository")
}

/// Removes a linked worktree's backing git dir (reads `.git` gitdir pointer).
pub fn remove_worktree_backing_git_dir(wt_path: &Path) {
    let git_file = wt_path.join(".git");
    if let Ok(contents) = std::fs::read_to_string(&git_file)
        && let Some(gitdir) = contents.strip_prefix("gitdir:")
    {
        let gitdir_path = wt_path.join(gitdir.trim());
        let _ = std::fs::remove_dir_all(&gitdir_path);
    }
}
