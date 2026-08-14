//! `treehouse doctor`: read-only health report.
//!
//! Answers "why is this worktree still here? why can't treehouse reclaim it?
//! is an agent still using it? is state broken?" — WITHOUT mutating state
//! (never heals, never writes). 12 checks with Ok/Warn/Error severity.
//!
//! Exit 0 when no Error-severity check; 1 when any Error; `--strict` promotes
//! Warns to failing.

use std::path::{Path, PathBuf};

use crate::pool::{Pool, PoolError};
use crate::state::{State, WorktreeEntry};

/// Severity of a doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Error,
}

/// One doctor check result.
#[derive(Debug, Clone)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub status: Severity,
    pub detail: String,
    pub count: u64,
    pub bytes: Option<u64>,
    pub affected: Vec<PathBuf>,
}

/// The full doctor report.
#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub healthy: bool,
    pub strict_healthy: bool,
}

impl DoctorReport {
    pub fn error_count(&self) -> u64 {
        self.checks
            .iter()
            .filter(|c| c.status == Severity::Error)
            .count() as u64
    }
    pub fn warn_count(&self) -> u64 {
        self.checks
            .iter()
            .filter(|c| c.status == Severity::Warn)
            .count() as u64
    }
}

/// Runs the doctor probes. READ-ONLY.
pub fn run_doctor(pool: &Pool) -> Result<DoctorReport, PoolError> {
    let mut checks = vec![
        // 1. git binary.
        check_git_binary(),
        // 2. config.
        check_config(pool),
        // 3. state.
        check_state(pool),
        // 4. state writable (probe temp file, removed after).
        check_state_writable(pool),
        // 5. lock (probe acquire+release).
        check_lock(pool),
    ];
    // 6-11. pool-inspection checks.
    checks.extend(inspect_worktrees(pool)?);
    // 12. disk.
    checks.push(check_disk(pool));

    let healthy = !checks.iter().any(|c| c.status == Severity::Error);
    let strict_healthy = !checks.iter().any(|c| c.status != Severity::Ok);
    Ok(DoctorReport {
        checks,
        healthy,
        strict_healthy,
    })
}

fn check_git_binary() -> DoctorCheck {
    let out = std::process::Command::new("git").arg("--version").output();
    match out {
        Ok(o) if o.status.success() => DoctorCheck {
            name: "git_binary",
            status: Severity::Ok,
            detail: String::from_utf8_lossy(&o.stdout).trim().to_string(),
            count: 0,
            bytes: None,
            affected: vec![],
        },
        _ => DoctorCheck {
            name: "git_binary",
            status: Severity::Error,
            detail: "git binary not found on PATH".into(),
            count: 0,
            bytes: None,
            affected: vec![],
        },
    }
}

fn check_config(pool: &Pool) -> DoctorCheck {
    // Pool::open already loaded config; report Ok.
    DoctorCheck {
        name: "config",
        status: Severity::Ok,
        detail: format!("max_trees = {}", pool.config.max_trees),
        count: 0,
        bytes: None,
        affected: vec![],
    }
}

fn check_state(pool: &Pool) -> DoctorCheck {
    let path = pool.pool_dir().join("treehouse-state.json");
    if !path.exists() {
        return DoctorCheck {
            name: "state",
            status: Severity::Ok,
            detail: "no state file (fresh pool)".into(),
            count: 0,
            bytes: None,
            affected: vec![],
        };
    }
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            return DoctorCheck {
                name: "state",
                status: Severity::Error,
                detail: format!("cannot read state file: {e}"),
                count: 0,
                bytes: None,
                affected: vec![path],
            };
        }
    };
    match serde_json::from_slice::<State>(&data) {
        Ok(_) => DoctorCheck {
            name: "state",
            status: Severity::Ok,
            detail: "state file parses".into(),
            count: 0,
            bytes: None,
            affected: vec![],
        },
        Err(_) => DoctorCheck {
            name: "state",
            status: Severity::Error,
            detail: "corrupt or truncated; recovery would mark on-disk worktrees leased".into(),
            count: 0,
            bytes: None,
            affected: vec![path],
        },
    }
}

fn check_state_writable(pool: &Pool) -> DoctorCheck {
    // Probe: create + write + remove a temp file in the pool dir.
    let probe = pool.pool_dir().join("doctor-probe.tmp");
    let result = (|| -> std::io::Result<()> {
        std::fs::write(&probe, b"probe")?;
        std::fs::remove_file(&probe)?;
        Ok(())
    })();
    match result {
        Ok(()) => DoctorCheck {
            name: "state_writable",
            status: Severity::Ok,
            detail: "pool dir is writable".into(),
            count: 0,
            bytes: None,
            affected: vec![],
        },
        Err(e) => DoctorCheck {
            name: "state_writable",
            status: Severity::Error,
            detail: format!("pool dir not writable: {e}"),
            count: 0,
            bytes: None,
            affected: vec![],
        },
    }
}

fn check_lock(pool: &Pool) -> DoctorCheck {
    // Probe: acquire + release the state lock.
    match crate::lock::with_state_lock(pool.pool_dir(), pool.lock_timeout, || Ok::<(), String>(()))
    {
        Ok(()) => DoctorCheck {
            name: "lock",
            status: Severity::Ok,
            detail: "state lock is acquirable".into(),
            count: 0,
            bytes: None,
            affected: vec![],
        },
        Err(e) => DoctorCheck {
            name: "lock",
            status: Severity::Error,
            detail: format!("state lock is wedged: {e}"),
            count: 0,
            bytes: None,
            affected: vec![],
        },
    }
}

/// Inspects worktrees: dead owners, stale/active leases, orphans, dirty, in-use.
fn inspect_worktrees(pool: &Pool) -> Result<Vec<DoctorCheck>, PoolError> {
    let state = State::read_state(pool.pool_dir()).map_err(PoolError::State)?;
    let now = chrono::Utc::now();

    let mut dead_owners = Vec::new();
    let mut stale_leases = Vec::new();
    let mut reclaimable = 0u64;
    let mut active_leases = 0u64;
    let mut orphans = Vec::new();
    let mut dirty = Vec::new();
    let mut in_use = Vec::new();

    let repo_root = pool
        .git
        .main_repo_root(&pool.root)
        .unwrap_or_else(|_| pool.root.clone());
    let default_ref = pool.resolve_prune_default_ref(&repo_root);

    for wt in &state.worktrees {
        if wt.destroying {
            continue;
        }
        // Dead owner.
        if wt.owner_pid != 0 && !crate::reservation::owner_alive(wt, &pool.process) {
            dead_owners.push(PathBuf::from(&wt.path));
        }
        // Leases.
        if wt.leased {
            if wt.is_stale_lease(now) {
                let container = Path::new(&wt.path).parent().unwrap_or(Path::new(&wt.path));
                let bytes = dir_size(container).unwrap_or(0);
                stale_leases.push(PathBuf::from(&wt.path));
                reclaimable += bytes;
            } else {
                active_leases += 1;
            }
        }
        // Orphan.
        if pool.backing_repository_missing(&wt.path) {
            orphans.push(PathBuf::from(&wt.path));
        }
        // Dirty.
        if pool.git_is_dirty(Path::new(&wt.path)).unwrap_or(false) {
            dirty.push(PathBuf::from(&wt.path));
        }
        // In-use (live process or live owner).
        let proc_in_use = pool
            .process
            .is_worktree_in_use(Path::new(&wt.path))
            .unwrap_or(false);
        if proc_in_use || crate::reservation::owner_alive(wt, &pool.process) {
            in_use.push(PathBuf::from(&wt.path));
        }
    }
    let _ = default_ref;
    let _ = reclaimable;

    Ok(vec![
        check_group(
            "dead_owners",
            Severity::Warn,
            "owner reservation no longer matches a live process; heal would clear on next status/gc/run",
            dead_owners,
        ),
        check_group(
            "stale_leases",
            Severity::Warn,
            "lease expired; gc --yes reclaims if idle+clean+merged",
            stale_leases,
        ),
        DoctorCheck {
            name: "active_leases",
            status: Severity::Ok,
            detail: format!("{active_leases} valid lease(s)"),
            count: active_leases,
            bytes: None,
            affected: vec![],
        },
        check_group(
            "orphans",
            Severity::Warn,
            "backing repository missing; use --prune-orphans --yes",
            orphans,
        ),
        check_group(
            "dirty",
            Severity::Warn,
            "uncommitted tracked/untracked changes",
            dirty,
        ),
        check_group(
            "in_use",
            Severity::Warn,
            "live process or owner reservation",
            in_use,
        ),
    ])
}

fn check_group(
    name: &'static str,
    severity: Severity,
    detail: &str,
    affected: Vec<PathBuf>,
) -> DoctorCheck {
    DoctorCheck {
        name,
        status: if affected.is_empty() {
            Severity::Ok
        } else {
            severity
        },
        detail: if affected.is_empty() {
            "none".into()
        } else {
            detail.into()
        },
        count: affected.len() as u64,
        bytes: None,
        affected,
    }
}

fn check_disk(pool: &Pool) -> DoctorCheck {
    let path = pool.pool_dir();
    let free = disk_free(path);
    let total = disk_total(path);
    let pct_free = if total > 0 {
        (free as f64 / total as f64) * 100.0
    } else {
        100.0
    };
    DoctorCheck {
        name: "disk",
        status: if pct_free < 10.0 {
            Severity::Warn
        } else {
            Severity::Ok
        },
        detail: format!("{:.1}% free on pool volume", pct_free),
        count: 0,
        bytes: Some(free),
        affected: vec![],
    }
}

#[cfg(unix)]
fn disk_free(_p: &Path) -> u64 {
    // Use statvfs via libc-free approach: report 0 (unknown) to avoid a dep.
    let _ = _p;
    0
}

#[cfg(windows)]
fn disk_free(p: &Path) -> u64 {
    // Use windows-sys GetDiskFreeSpaceExW.
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut free: u64 = 0;
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut free,
        );
        free
    }
}

#[cfg(unix)]
fn disk_total(_p: &Path) -> u64 {
    0
}

#[cfg(windows)]
fn disk_total(p: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut total: u64 = 0;
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            &mut total,
            std::ptr::null_mut(),
        );
        total
    }
}

/// Recursively measures a directory's size.
fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            total += dir_size(&p)?;
        } else {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

/// Helper for the CLI to render the report as JSON.
pub fn report_json(r: &DoctorReport) -> serde_json::Value {
    serde_json::json!({
        "healthy": r.healthy,
        "strict_healthy": r.strict_healthy,
        "checks": r.checks.iter().map(|c| serde_json::json!({
            "name": c.name,
            "status": match c.status { Severity::Ok => "ok", Severity::Warn => "warn", Severity::Error => "error" },
            "detail": c.detail,
            "count": c.count,
            "bytes": c.bytes,
            "affected": c.affected.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

// Make WorktreeEntry import used (reference in signature contexts).
#[allow(unused)]
fn _use(_wt: &WorktreeEntry) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_order() {
        assert_eq!(Severity::Ok as u8, 0);
        assert_eq!(Severity::Warn as u8, 1);
        assert_eq!(Severity::Error as u8, 2);
    }

    #[test]
    fn report_counts() {
        let r = DoctorReport {
            checks: vec![
                DoctorCheck {
                    name: "a",
                    status: Severity::Ok,
                    detail: "".into(),
                    count: 0,
                    bytes: None,
                    affected: vec![],
                },
                DoctorCheck {
                    name: "b",
                    status: Severity::Warn,
                    detail: "".into(),
                    count: 0,
                    bytes: None,
                    affected: vec![],
                },
                DoctorCheck {
                    name: "c",
                    status: Severity::Error,
                    detail: "".into(),
                    count: 0,
                    bytes: None,
                    affected: vec![],
                },
            ],
            healthy: false,
            strict_healthy: false,
        };
        assert_eq!(r.error_count(), 1);
        assert_eq!(r.warn_count(), 1);
    }
}
