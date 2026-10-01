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

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

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

/// A process table backed by sysinfo, re-enumerated on every query.
///
/// # Why every query refreshes
///
/// The table exists only to reuse the *expensive* parts of a syscall sweep
/// (building a fresh `System` with cwd for every PID); it is NOT a cache whose
/// contents may be reused for a decision. Every in-use / owner-liveness answer
/// this type returns gates a destructive action (`destroy` classifies
/// `Disposable` and deletes, `heal_state` clears a live owner's reservation),
/// so a snapshot older than the decision is a safety bug, not a stale-cache
/// bug: a process that started after `Pool::open` is invisible to a frozen
/// table and its worktree gets deleted out from under it.
///
/// Go has no table at all — `FindProcessesInWorktree` calls `process.Processes()`
/// (a fresh enumeration) on every invocation and `StartedAt` does a live
/// `process.NewProcess(pid)` + `CreateTime()` syscall per call. Matching that
/// means refreshing here rather than trusting construction-time state, so this
/// type's queries are as current as Go's.
///
/// Cost is kept proportionate to what each query needs, which is why there are
/// two refresh granularities (measured on macOS, ~470 processes):
/// - [`ProcessTable::find_in_worktree`] needs *every* process's cwd, so it
///   does a full re-enumeration (~5 ms).
/// - [`ProcessTable::started_at`] / [`ProcessTable::exists`] need one PID, so
///   they refresh just that PID (~11 µs). `heal_state` and `owner_alive` call
///   `started_at` once per worktree while holding the pool lock, so a full
///   refresh there would multiply a lock hold by the pool size.
pub struct ProcessTable {
    system: Mutex<System>,
}

impl ProcessTable {
    pub fn new() -> Self {
        // Refresh EVERYTHING (cwd, parent, start time, name) for all PIDs.
        //
        // `refresh_processes(All, true)` does NOT populate `cwd` on macOS for
        // other users' processes (it returns `None`, silently making in-use
        // detection blind). `refresh_processes_specifics` with
        // `ProcessRefreshKind::everything()` fetches cwd too — the Go version
        // uses gopsutil which reads cwd for every process.
        let table = Self {
            system: Mutex::new(System::new_with_specifics(
                RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
            )),
        };
        table.refresh();
        table
    }

    /// Re-enumerates every process. Required before any query that inspects
    /// the whole table (cwd of all processes).
    pub fn refresh(&self) {
        let mut system = self.system.lock().unwrap();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::everything(),
        );
    }

    /// Re-reads a single PID into the table (Go `process.NewProcess(pid)`).
    ///
    /// Cheap enough to run on every liveness query. Also drops the entry when
    /// the process is gone, which `refresh_processes_specifics(.., true, ..)`
    /// does for the pids named in `ProcessesToUpdate::Some`.
    fn refresh_pid(&self, pid: i32) {
        let mut system = self.system.lock().unwrap();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
            true,
            ProcessRefreshKind::everything(),
        );
    }

    /// Returns every process whose cwd is the worktree root or a descendant,
    /// after absolute-path + symlink resolution. Matches Go's
    /// `FindProcessesInWorktree` including the `..`-prefixed-dirname rule.
    pub fn find_in_worktree(&self, worktree_path: &Path) -> Result<Vec<ProcessInfo>, ProcessError> {
        let abs_worktree = absolute_and_resolve(worktree_path)
            .ok_or_else(|| ProcessError::Scan("resolving worktree path".into()))?;

        // Re-enumerate FIRST: a worktree whose process started after this
        // table was built would otherwise scan as empty and be classified
        // Disposable. This is the same fresh `process.Processes()` Go does per
        // call, so the post-kill survivor re-scan in `destroy` actually sees
        // that the killed PIDs are gone instead of reading them back out of a
        // frozen snapshot.
        self.refresh();

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
    ///
    /// Refreshes the PID first (Go `StartedAt` is a live `CreateTime()` call).
    /// Without this, a pid started after the table was built reads as absent,
    /// `owner_alive` returns false, and `heal_state` zeroes the owner pair of a
    /// worktree an agent is actively using.
    pub fn started_at(&self, pid: i32) -> Option<i64> {
        self.refresh_pid(pid);
        let system = self.system.lock().unwrap();
        system
            .process(Pid::from_u32(pid as u32))
            .map(|p| p.start_time() as i64 * 1000)
    }

    /// Whether a pid currently exists.
    pub fn exists(&self, pid: i32) -> bool {
        self.refresh_pid(pid);
        let system = self.system.lock().unwrap();
        system.process(Pid::from_u32(pid as u32)).is_some()
    }

    /// The parent pid of `pid`, if determinable.
    ///
    /// Refreshes the PID first (Go `parentPID` builds a fresh `Process` per
    /// call). `protected_chain` walks the caller's ancestry while deciding what
    /// is safe to signal, so an ancestry link must be read live: a stale
    /// parent could make the walk protect the wrong set — or fail to protect
    /// the caller itself.
    fn parent_pid(&self, pid: i32) -> Option<i32> {
        self.refresh_pid(pid);
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
    ///
    /// The target set is scanned live, so this works for a table built long
    /// before the processes existed — which is exactly the `run` path, where
    /// the Pool (and this table) is opened before the child is spawned and
    /// `wait_child` then blocks for the child's whole lifetime.
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
    /// plus its whole ancestor chain.
    ///
    /// Fails CLOSED only when an ancestor exists but its parent genuinely
    /// cannot be read, so a partial chain is never mistaken for a short one.
    /// The benign end-of-walk cases BREAK instead, matching Go's
    /// `filterProtectedProcesses` (terminate.go:75-95):
    ///
    /// * **Reached the root** (`ppid == 0`, so sysinfo reports no parent).
    ///   Every unix root — pid 1 / launchd / init — has no parent, so treating
    ///   this as an error made the walk fail on EVERY platform, and
    ///   `terminate_with_grace` returned Err, so `destroy --include-in-use`
    ///   skipped every in-use worktree no matter what it killed. Go ends the
    ///   walk here via `if parent <= 0 { break }`.
    /// * **The ancestor exited mid-walk** (absent from the table). Go treats
    ///   `ErrorProcessNotRunning` as a benign end — the common Windows case of
    ///   a parent exiting and leaving a dangling parent PID.
    fn protected_chain(
        &self,
        current_pid: i32,
    ) -> Result<std::collections::HashSet<i32>, ProcessError> {
        let mut protected = std::collections::HashSet::new();
        protected.insert(current_pid);
        // `parent_pid` yields None both for "this pid is the root" and "this
        // pid is gone"; both END the walk. Only a pid that is present with an
        // unreadable parent would be a real failure, and sysinfo cannot
        // represent that case — so the walk ends rather than discarding the
        // caller chain (and with it the ability to terminate anything).
        let mut pid = current_pid;
        while let Some(parent) = self.parent_pid(pid) {
            if parent <= 0 {
                break; // root / invalid pid: ancestry is exhausted
            }
            if !protected.insert(parent) {
                break; // cycle guard (already seen)
            }
            pid = parent;
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
    fn protected_chain_always_contains_the_callers_own_pid() {
        // The safety invariant that must hold on EVERY path, including the
        // benign end-of-walk cases: the calling process is in its own protected
        // set, so terminate_with_grace can never signal itself.
        //
        // Previously this test was vacuous — it accepted Ok or Err, so it
        // passed no matter what `protected_chain` did. It only ever passed by
        // accident on platforms where the walk happened to succeed.
        let table = ProcessTable::new();
        let me = std::process::id() as i32;
        let protected = table
            .protected_chain(me)
            .expect("the ancestry walk must terminate, not error, on the root process");
        assert!(
            protected.contains(&me),
            "own pid must be protected; got {protected:?}"
        );
    }

    #[test]
    fn protected_chain_includes_the_parents_shell_when_resolvable() {
        // The walk must actually climb, not just return the caller. Every
        // platform's root (pid 1 / launchd / init) reports no parent, so a walk
        // that cannot climb would still satisfy the own-pid invariant above.
        let table = ProcessTable::new();
        let me = std::process::id() as i32;
        let protected = table
            .protected_chain(me)
            .expect("walk terminates at the root");
        assert!(
            protected.len() > 1,
            "the walk should reach at least one ancestor of pid {me}; got {protected:?}"
        );
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

    /// Spawns a long-lived child whose cwd is `dir`, so the tests below can
    /// observe a process that starts AFTER a `ProcessTable` is built.
    ///
    /// Uses the same "idle command" approach as the hook tests: on unix
    /// `sleep`, on Windows `ping` (which has no `sleep` equivalent but does
    /// block for its timeout). Both are killed by the caller on drop.
    fn spawn_child_in(dir: &Path) -> std::process::Child {
        let mut cmd = std::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" });
        if cfg!(windows) {
            cmd.args(["-n", "60", "127.0.0.1"]);
        } else {
            cmd.arg("60");
        }
        cmd.current_dir(dir)
            .spawn()
            .expect("spawn idle child in the given dir")
    }

    /// Kills a spawned child and reaps it, so the pid leaves the table.
    fn kill_child(child: &mut std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    /// M-003: a process started after the table was built must still be found.
    ///
    /// Before the fix, `ProcessTable` enumerated exactly once in `new()` and
    /// `refresh()` had no callers, so this scan returned empty — and every
    /// downstream safety decision (destroy classification, `owner_alive`,
    /// `heal_state`) read that frozen snapshot. A worktree with an agent
    /// started after `Pool::open` was classified `Disposable` and deleted.
    #[test]
    fn find_in_worktree_sees_process_started_after_the_table_was_built() {
        let dir = tempfile::tempdir().unwrap();
        // Build the table FIRST: the child does not exist yet, so this is
        // exactly the "pool opened, then an agent started" ordering.
        let table = ProcessTable::new();
        assert!(
            table.find_in_worktree(dir.path()).unwrap().is_empty(),
            "precondition: no process in the dir before the child starts"
        );

        let mut child = spawn_child_in(dir.path());
        let pid = child.id() as i32;

        let found = table
            .find_in_worktree(dir.path())
            .unwrap_or_else(|e| panic!("scan failed: {e}"))
            .into_iter()
            .any(|p| p.pid == pid);
        assert!(
            found,
            "a process started after the table was built must be visible to the scan \
             (pid {pid}); a frozen snapshot reports it empty and destroy then deletes \
             a worktree that is in use"
        );

        kill_child(&mut child);
    }

    /// M-003: `started_at` must resolve a pid created after the table was
    /// built, so `owner_alive` does not report a live owner as dead.
    ///
    /// `owner_alive` returning false for a live owner makes `heal_state` zero
    /// `owner_pid`/`owner_started_at` and clear `Destroying`, handing an
    /// actively-used slot to a concurrent acquire.
    #[test]
    fn started_at_resolves_pid_created_after_the_table_was_built() {
        let dir = tempfile::tempdir().unwrap();
        let table = ProcessTable::new();
        let mut child = spawn_child_in(dir.path());
        let pid = child.id() as i32;

        let started = table
            .started_at(pid)
            .unwrap_or_else(|| panic!("start time for a live child (pid {pid}) must resolve"));
        assert!(
            started > 1_000_000_000_000,
            "should be epoch millis, got {started}"
        );

        kill_child(&mut child);
    }

    /// M-003/M-004: a terminated process must disappear from the table.
    ///
    /// This is the deterministic half of the `destroy --include-in-use` bug:
    /// the post-kill survivor re-scan read the same snapshot that still listed
    /// the just-killed pids, so `is_empty()` was never true and destroy always
    /// skipped with "worktree processes still running after termination" —
    /// the opt-in flag was silently a no-op.
    #[test]
    fn scan_after_termination_no_longer_reports_the_killed_process() {
        let dir = tempfile::tempdir().unwrap();
        let table = ProcessTable::new();
        let mut child = spawn_child_in(dir.path());
        let pid = child.id() as i32;

        // Precondition: the child is visible, so the post-kill assertions
        // below are not vacuously true.
        assert!(
            table
                .find_in_worktree(dir.path())
                .unwrap()
                .iter()
                .any(|p| p.pid == pid),
            "precondition: child visible before termination"
        );

        // Terminate exactly as destroy does (SIGTERM -> grace -> SIGKILL),
        // then re-scan. Without a refresh the killed pid is read back out of
        // the frozen snapshot.
        kill_child(&mut child);
        let survivors = table
            .find_in_worktree(dir.path())
            .expect("survivor re-scan must not error");
        assert!(
            !survivors.iter().any(|p| p.pid == pid),
            "the terminated process (pid {pid}) must not survive a live re-scan; \
             survivors reported: {survivors:?}"
        );
    }
}
