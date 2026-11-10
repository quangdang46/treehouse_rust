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
use std::time::{Duration, Instant};

/// Upper bound on one hook command. Hooks are user-configured and run on the
/// acquire/destroy path, so a wedged hook must not stall the pool forever;
/// on expiry the child is killed and the failure is logged like any other.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(600);

/// How often the wait loop polls the child while draining its pipes.
const WAIT_POLL: Duration = Duration::from_millis(10);

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

    // Drain both pipes CONCURRENTLY while the child runs. Reading them only
    // after `wait` deadlocks the moment a hook writes more than the OS pipe
    // buffer (~64 KiB) to either stream: the child blocks on write, `wait`
    // never returns, and the reader never starts.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = out_pipe.as_mut() {
            use std::io::Read;
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = err_pipe.as_mut() {
            use std::io::Read;
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });

    // Wait with a timeout: a hook that never exits is killed rather than
    // holding the caller (and, worse, a `pre_destroy` sweep) open forever.
    let deadline = Instant::now() + HOOK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(());
                }
                std::thread::sleep(WAIT_POLL);
            }
            Err(_) => break Err(()),
        }
    };

    // The pipes hit EOF once the child is reaped (or killed), so the readers
    // always finish; join them before forwarding so output order per stream
    // is preserved.
    let out_buf = out_reader.join().unwrap_or_default();
    let err_buf = err_reader.join().unwrap_or_default();
    let _ = stdout.write_all(&out_buf);
    let _ = stderr.write_all(&err_buf);

    let status = match status {
        Ok(s) => s,
        Err(()) => {
            let _ = writeln!(
                stderr,
                "🌳 hook command failed: {command:?} (exit -1): timed out after {}s",
                HOOK_TIMEOUT.as_secs()
            );
            return;
        }
    };

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

    /// A hook emitting more than the OS pipe buffer (~64 KiB) must not
    /// deadlock: the pipes are drained while the child runs, not after `wait`.
    #[cfg(unix)]
    #[test]
    fn hook_with_output_larger_than_the_pipe_buffer_completes() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, mut err) = buf();
        // ~256 KiB on stdout AND stderr — far past any pipe buffer.
        let cmd =
            "head -c 262144 /dev/zero | tr '\\0' 'o'; head -c 262144 /dev/zero | tr '\\0' 'e' >&2";
        run_one(cmd, dir.path(), &mut out, &mut err);
        assert_eq!(out.len(), 262144, "stdout must be fully drained");
        assert_eq!(err.len(), 262144, "stderr must be fully drained");
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
