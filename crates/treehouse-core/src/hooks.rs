//! User-configured lifecycle hooks (`post_create` / `pre_destroy`).
//!
//! Port of Go `internal/hooks/`: each command runs sequentially in the target
//! worktree directory via the OS shell (`/bin/sh -c` on unix, `%COMSPEC%
//! /d /s /c` on Windows). A failing hook is logged but never stops later hooks
//! or fails the caller.
//!
//! Critical timing invariant (short-lock protocol): hooks run OUTSIDE the
//! state lock. `post_create` runs after the lock is released; `pre_destroy`
//! runs outside all locks. A probe that acquires the state lock from inside a
//! hook must succeed immediately.

use std::path::Path;
use std::process::{Command, Stdio};

/// Runs each command sequentially in `work_dir`. Failures are logged to
/// `stderr` (🌳 hook command failed: "<cmd>" (exit N): <err>) and do not stop
/// subsequent commands. An empty list is a no-op.
pub fn run(
    commands: &[String],
    work_dir: &Path,
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
) {
    for command in commands {
        run_one(command, work_dir, stdout, stderr);
    }
}

/// Runs a single hook command, logging (not propagating) failures.
pub fn run_one(
    command: &str,
    work_dir: &Path,
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
) {
    let mut cmd = new_hook_command(command);
    cmd.current_dir(work_dir);
    // Capture the child's stdout/stderr and forward to the given writers
    // (Go sets cmd.Stdout/Stderr directly and streams them).
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(stderr, "🌳 hook command failed: {command:?} (exit -1): {e}");
            return;
        }
    };

    // Reap the child; forward any captured output.
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(stderr, "🌳 hook command failed: {command:?} (exit -1): {e}");
            return;
        }
    };

    if let Some(mut pipe) = child.stdout.take() {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = stdout.write_all(&buf);
    }
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = stderr.write_all(&buf);
    }

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let _ = writeln!(
            stderr,
            "🌳 hook command failed: {command:?} (exit {code}): command exited with status {status}"
        );
    }
}

/// Builds the shell command for a hook (Go `newHookCommand`).
#[cfg(unix)]
fn new_hook_command(command: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command);
    cmd
}

/// Builds the shell command for a hook: `%COMSPEC% /d /s /c <cmd>` (fallback
/// `cmd.exe`). Matches Go's `newHookCommand` + `windowsShellCommandLine`.
#[cfg(windows)]
fn new_hook_command(command: &str) -> Command {
    let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    let mut cmd = Command::new(&shell);
    cmd.args(["/d", "/s", "/c", command]);
    cmd
}

/// The exact `cmd.exe` command line Go pins in its test:
/// `"<shell>" /d /s /c "<command>"`. Kept for parity documentation and the
/// pinned single-string test.
#[cfg(windows)]
pub fn windows_shell_command_line(shell: &str, command: &str) -> String {
    format!("\"{shell}\" /d /s /c \"{command}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf() -> (Vec<u8>, Vec<u8>) {
        (Vec::new(), Vec::new())
    }

    #[test]
    fn empty_list_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, mut err) = buf();
        run(&[], dir.path(), &mut out, &mut err);
        assert!(out.is_empty() && err.is_empty());
    }

    /// Writes a sentinel file using a shell redirection command that works on
    /// the current platform. The temp dir path has no spaces, so unquoted
    /// redirection targets are safe on cmd.exe (which mangles quoted ones).
    fn sentinel_cmd(sentinel: &Path) -> String {
        format!("echo hi > {}", sentinel.display())
    }

    #[test]
    fn hook_runs_in_given_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("ran.txt");
        let (mut out, mut err) = buf();
        let cmd = sentinel_cmd(&sentinel);
        run(&[cmd], dir.path(), &mut out, &mut err);
        assert!(
            sentinel.exists(),
            "sentinel must exist; stderr: {}",
            String::from_utf8_lossy(&err)
        );
    }

    #[test]
    fn failing_hook_does_not_stop_remaining() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("after.txt");
        let fail = "this-command-definitely-does-not-exist-xyzzy".to_string();
        let ok = sentinel_cmd(&sentinel);
        let (mut out, mut err) = buf();
        run(&[fail, ok], dir.path(), &mut out, &mut err);
        // Second command must run despite the first failing.
        assert!(sentinel.exists(), "second command must run after a failure");
        let errs = String::from_utf8_lossy(&err);
        assert!(
            errs.contains("hook command failed"),
            "failure must be logged: {errs}"
        );
    }

    #[test]
    fn hook_failure_logs_and_returns() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, mut err) = buf();
        run(&["exit 3".to_string()], dir.path(), &mut out, &mut err);
        let errs = String::from_utf8_lossy(&err);
        assert!(
            errs.contains("hook command failed"),
            "expected failure log: {errs}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_shell_command_line_wraps_quoted_command() {
        // Mirrors Go's pinned single-string test exactly.
        let shell = r"C:\Windows\System32\cmd.exe";
        let command = r#"echo hi > "C:\Temp\ran.txt""#;
        let got = windows_shell_command_line(shell, command);
        let want = format!("\"{shell}\" /d /s /c \"{command}\"");
        assert_eq!(got, want);
    }
}
