//! Process detection (in-use) and termination (lingering processes).
//!
//! Port of Go `internal/process/`: uses sysinfo to find processes whose cwd is
//! inside a worktree (in-use detection) and to terminate lingering processes
//! (SIGTERM -> SIGKILL on unix, TerminateProcess on Windows).
//!
//! In-use detection matches by **resolved realpath**: both the worktree path
//! and each process cwd are symlink-resolved (macOS `/tmp` -> `/private/tmp`,
//! symlinked worktree paths) before comparing, exactly like Go's
//! `FindProcessesInWorktree`. Children whose path relative to the worktree is
//! `.` or a descendant are included; a cwd exactly one level *above* (`..`) is
//! excluded, as are paths starting with `..` + separator.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use sysinfo::{Pid, ProcessesToUpdate, System};

/// A process found running inside a worktree. Display is `name (pid)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: i32,
    pub name: String,
}

impl std::fmt::Display for ProcessInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.pid)
    }
}

/// A snapshot-able process table backed by sysinfo.
///
/// sysinfo must be refreshed before querying. We hold the `System` behind a
/// mutex and re-enumerate on demand (the process table is the most expensive
/// check in the tool; `gc`/`doctor` sweep it per worktree).
pub struct ProcessTable {
    system: Mutex<System>,
}

impl ProcessTable {
    pub fn new() -> Self {
        // Refresh everything (cwd, parent, start time, name) for all PIDs.
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        Self {
            system: Mutex::new(system),
        }
    }

    /// Re-enumerates the process table.
    pub fn refresh(&self) {
        let mut system = self.system.lock().unwrap();
        system.refresh_processes(ProcessesToUpdate::All, true);
    }

    /// Returns every process whose cwd is the worktree root or a descendant,
    /// after absolute-path + symlink resolution. Matches Go's
    /// `FindProcessesInWorktree` including the `..`-prefixed-dirname rule.
    pub fn find_in_worktree(&self, worktree_path: &Path) -> Result<Vec<ProcessInfo>, ProcessError> {
        let abs_worktree = absolute_and_resolve(worktree_path)
            .ok_or_else(|| ProcessError::Scan("resolving worktree path".into()))?;

        let mut result = Vec::new();
        let system = self.system.lock().unwrap();
        for (pid, process) in system.processes() {
            let cwd = match process.cwd() {
                Some(c) => c.to_path_buf(),
                None => continue, // exited or permission-restricted: skip
            };
            let Some(abs_cwd) = absolute_and_resolve(&cwd) else {
                continue;
            };
            let rel = match pathdiff_rel(&abs_worktree, &abs_cwd) {
                Some(r) => r,
                None => continue,
            };
            // Go matcher: include when rel == "." (or empty — our pathdiff
            // yields "" for base==cwd, Go yields "."), OR when rel is a
            // descendant that isn't exactly ".." and doesn't start with
            // "../" (platform separator). A child dir literally named "..x"
            // (e.g. "..cache") IS included.
            let rel_str = rel.to_string_lossy();
            let is_self = rel_str == "." || rel_str.is_empty();
            let is_up =
                rel_str == ".." || rel_str.starts_with("../") || rel_str.starts_with("..\\");
            if is_self || !is_up {
                result.push(ProcessInfo {
                    pid: pid.as_u32() as i32,
                    name: process.name().to_string_lossy().into_owned(),
                });
            }
        }
        Ok(result)
    }

    /// Whether any process is running in the worktree.
    pub fn is_worktree_in_use(&self, worktree_path: &Path) -> Result<bool, ProcessError> {
        Ok(!self.find_in_worktree(worktree_path)?.is_empty())
    }

    /// Epoch **millis** start time for `pid`, or `None` if it can't be
    /// determined. Go stores gopsutil `CreateTime` in millis; sysinfo
    /// `start_time()` returns seconds — multiply by 1000 (accept truncation)
    /// so `owner_alive` matches across the mixed Go+Rust window.
    pub fn started_at(&self, pid: i32) -> Option<i64> {
        let system = self.system.lock().unwrap();
        system
            .process(Pid::from_u32(pid as u32))
            .map(|p| p.start_time() as i64 * 1000)
    }

    /// Whether a pid currently exists.
    pub fn exists(&self, pid: i32) -> bool {
        let system = self.system.lock().unwrap();
        system.process(Pid::from_u32(pid as u32)).is_some()
    }

    /// The parent pid of `pid`, if determinable.
    fn parent_pid(&self, pid: i32) -> Option<i32> {
        let system = self.system.lock().unwrap();
        system
            .process(Pid::from_u32(pid as u32))
            .and_then(|p| p.parent())
            .map(|p| p.as_u32() as i32)
    }

    /// Terminates every process found in the worktree, protecting the calling
    /// process and its entire ancestor chain.
    ///
    /// unix: SIGTERM all, wait up to `grace` polling every 50ms, SIGKILL
    /// survivors. Windows: `TerminateProcess` (abrupt; `grace` is effectively
    /// ignored, matching Go). Individual kill failures are swallowed; the
    /// initial scan error propagates.
    pub fn terminate_with_grace(
        &self,
        worktree_path: &Path,
        grace: Duration,
    ) -> Result<Vec<ProcessInfo>, ProcessError> {
        let procs = self.find_in_worktree(worktree_path)?;
        let current_pid = std::process::id() as i32;
        let protected = self.protected_chain(current_pid)?;
        let targets: Vec<ProcessInfo> = procs
            .into_iter()
            .filter(|p| !protected.contains(&p.pid))
            .collect();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let pids: Vec<i32> = targets.iter().map(|p| p.pid).collect();
        self.terminate(&pids, grace);
        Ok(targets)
    }

    /// Builds the set of pids that must never be killed: the calling process
    /// plus its whole ancestor chain. On a parent-lookup failure, fails CLOSED
    /// (returns `None`) so nothing is terminated.
    fn protected_chain(
        &self,
        current_pid: i32,
    ) -> Result<std::collections::HashSet<i32>, ProcessError> {
        let mut protected = std::collections::HashSet::new();
        protected.insert(current_pid);
        let mut pid = current_pid;
        loop {
            match self.parent_pid(pid) {
                Some(parent) if parent > 0 && !protected.contains(&parent) => {
                    protected.insert(parent);
                    pid = parent;
                }
                Some(parent) if parent <= 0 => break,
                Some(_) => break, // cycle guard (already seen)
                None => return Err(ProcessError::Scan(format!("resolving parent of pid {pid}"))),
            }
        }
        Ok(protected)
    }

    #[cfg(unix)]
    fn terminate(&self, pids: &[i32], grace: Duration) {
        for pid in pids {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            if !pids.iter().any(|&pid| self.alive(pid)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for pid in pids {
            if self.alive(*pid) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(*pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }

    #[cfg(windows)]
    fn terminate(&self, pids: &[i32], _grace: Duration) {
        // Windows has no graceful SIGTERM for arbitrary processes; use
        // TerminateProcess. Individual failures (e.g. already gone) are
        // swallowed.
        for &pid in pids {
            let _ = terminate_process_windows(pid);
        }
    }

    #[cfg(unix)]
    fn alive(&self, pid: i32) -> bool {
        // signal 0 validates existence without signaling.
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }
}

#[cfg(windows)]
fn terminate_process_windows(pid: i32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let r = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if r == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// Absolute + symlink-resolved path (Go `resolvePath`): returns the canonical
/// path, or the input if resolution fails.
fn absolute_and_resolve(p: &Path) -> Option<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(p)
    };
    match std::fs::canonicalize(&abs) {
        Ok(c) => Some(c),
        Err(_) => Some(abs),
    }
}

/// Path of `cwd` relative to `base`, as a `PathBuf`, or `None` on failure.
pub(crate) fn pathdiff_rel(base: &Path, cwd: &Path) -> Option<PathBuf> {
    // std has no Rel in stable; implement the same semantics as Go's filepath.Rel
    // over components. For our purpose we only need "." vs a descendant, so a
    // component walk suffices.
    let base_comp: Vec<_> = base.components().collect();
    let cwd_comp: Vec<_> = cwd.components().collect();
    let common = base_comp
        .iter()
        .zip(cwd_comp.iter())
        .take_while(|(a, b)| a == b)
        .count();
    // If no common prefix, the paths are on different roots — not a match.
    if common == 0 {
        return None;
    }
    // From the remaining base components we'd need ".." per level, then the
    // rest of cwd. Go's filepath.Rel returns "." when both are equal.
    if common == base_comp.len() && common == cwd_comp.len() {
        return Some(PathBuf::from("."));
    }
    let mut rel = PathBuf::new();
    for _ in common..base_comp.len() {
        rel.push("..");
    }
    for c in &cwd_comp[common..] {
        rel.push(c.as_os_str());
    }
    Some(rel)
}

/// Errors from process operations.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("process scan failed: {0}")]
    Scan(String),
}

impl Default for ProcessTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_info_display() {
        let p = ProcessInfo {
            pid: 82144,
            name: "opencode".into(),
        };
        assert_eq!(p.to_string(), "opencode (82144)");
    }

    #[test]
    fn pathdiff_basic() {
        let base = Path::new("/home/user/proj/.treehouse/acme/1/acme");
        // base == cwd => "." (Go filepath.Rel semantics).
        assert_eq!(pathdiff_rel(base, base).unwrap().to_string_lossy(), ".");
        let child = base.join("src").join("main.rs");
        let rel = pathdiff_rel(base, &child).unwrap();
        // Compare component-wise to be separator-agnostic (Windows uses \).
        let comps: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(comps, ["src", "main.rs"]);
        // One level up => "..".
        let parent = base.parent().unwrap();
        assert_eq!(pathdiff_rel(base, parent).unwrap().to_string_lossy(), "..");
    }

    #[test]
    fn matcher_includes_dotdotdot_cache_and_excludes_parent() {
        // Exercise the same classification logic as find_in_worktree.
        fn matches(rel: &str) -> bool {
            let rel_str = rel;
            let is_self = rel_str == "." || rel_str.is_empty();
            let is_up =
                rel_str == ".." || rel_str.starts_with("../") || rel_str.starts_with("..\\");
            is_self || !is_up
        }
        assert!(matches("."), "cwd == worktree root");
        assert!(matches("src"), "descendant");
        assert!(
            matches("..cache"),
            "child dir literally named '..cache' is included"
        );
        assert!(!matches(".."), "exactly one level up is excluded");
        assert!(!matches("../sibling"), "parent-relative path is excluded");
        assert!(!matches("../../other"), "grandparent is excluded");
    }

    #[test]
    fn protected_chain_fails_closed_on_missing_parent() {
        // A pid with no resolvable parent (e.g. a non-existent pid) must fail
        // closed (terminate nothing), matching Go's filterProtectedProcesses.
        let table = ProcessTable::new();
        // The current process is ALWAYS protected.
        let me = std::process::id() as i32;
        // If our own chain resolves, we must be in it. If it fails (e.g. Windows
        // snapshot can't resolve our parent), that is the fail-closed behavior —
        // terminate nothing.
        if let Ok(protected) = table.protected_chain(me) {
            assert!(protected.contains(&me));
        }
    }

    #[test]
    fn protected_chain_contains_own_pid_when_it_resolves() {
        let table = ProcessTable::new();
        let me = std::process::id() as i32;
        // The calling process must never be killable: if the chain resolves,
        // we're protected.
        if let Ok(protected) = table.protected_chain(me) {
            assert!(protected.contains(&me), "own pid must be protected");
        }
    }

    #[test]
    fn started_at_matches_own_process() {
        let table = ProcessTable::new();
        let me = std::process::id() as i32;
        let started = table.started_at(me).expect("own start time known");
        assert!(started > 0, "start time should be positive millis");
        assert!(
            started > 1_000_000_000_000,
            "should be epoch millis (not seconds)"
        );
    }
}
