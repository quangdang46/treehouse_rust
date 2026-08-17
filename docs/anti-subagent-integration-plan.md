# Anti-Subagent × Treehouse Integration Plan

> Replace subprocess calls to `treehouse` CLI with direct `treehouse-core` library API. Eliminate process overhead, fix PID-reuse risk, and unify process tracking.

---

## Current State

anti_subagent already uses treehouse — but via **subprocess calls** to the `treehouse` binary:

```rust
// anti-workspace/src/lib.rs — current implementation
pub fn acquire(repo_root: &Path, holder: &str) -> Result<Lease> {
    let output = Command::new("treehouse")
        .args(["get", "--lease", "--lease-holder", holder, "--json"])
        .current_dir(repo_root)
        .output()?;
    // parse JSON → Lease { path, lease_id, holder }
}

pub fn release_if_lease(lease: &Lease) -> Result<()> {
    Command::new("treehouse")
        .args(["return", "--force", "--if-lease-id", &lease.lease_id, &lease.path])
        .output()?;
    Ok(())
}
```

### Pain Points

| # | Problem | Impact |
|---|---|---|
| 1 | **Subprocess overhead** — every acquire/release forks `treehouse` binary | ~50ms per call, multiple calls per agent lifecycle |
| 2 | **PID reuse risk** — `spawn_gen` field exists but never incremented on restart | Could mark wrong process as crashed |
| 3 | **No daemon lock** — no PID file or flock prevents two daemons | Stale socket → two daemons possible |
| 4 | **Duplicate recovery** — `reconcile_on_start` (main.rs) vs `recover_on_restart` (recovery.rs) | Conflicting logic, maintenance burden |
| 5 | **Lease sweeper is polling** — runs every 15s, releases leases of terminal agents | Delay between agent exit and worktree availability |
| 6 | **Children HashMap lost on restart** — no in-memory Child handles after daemon restart | Must reconcile from SQLite + `kill -0` |

---

## Goals

1. **Zero subprocess** — use `treehouse-core` as Cargo dependency, no `Command::new("treehouse")`
2. **PID-reuse safe** — leverage treehouse's `owner_started_at` verification
3. **Daemon lock** — use treehouse's `fd-lock` for single-instance guarantee
4. **Unified recovery** — treehouse `heal_state()` + `pool.gc()` replaces ad-hoc reconciliation
5. **Immediate lease release** — treehouse `pool.release()` on agent exit, no 15s sweeper

---

## Non-Goals

- Rewrite anti_subagent's IPC, scheduler, or work verification system
- Change the SLP (Supervisor–Lead–Peer) architecture
- Modify harness adapters (Claude, Codex, OpenCode)
- Change the CLI interface

---

## Architecture

```
BEFORE:                                AFTER:

anti-daemon                            anti-daemon
  │                                      │
  ├── anti-workspace                     ├── anti-workspace
  │     │                                │     │
  │     ├── Command::new("treehouse")    │     ├── TreehouseCore<AntiEnv>
  │     │   .args(["get", "--lease"])    │     │   .acquire()
  │     │   .spawn()?                    │     │   .release()
  │     │                                │     │   .gc()
  │     └── fork + exec + JSON parse     │     └── direct function call
  │                                      │
  ├── SQLite (agents table)              ├── SQLite (agents table)
  │   pid INTEGER                        │   pid INTEGER
  │   spawn_gen INTEGER                  │   owner_started_at INTEGER  ← NEW
  │                                      │
  └── HashMap<String, Child>            └── Pool (treehouse state)
      (lost on restart)                      (persistent, PID-reuse safe)

                                      ┌─────────────────────────┐
                                      │ treehouse-core           │
                                      │ ├── Pool (worktree pool) │
                                      │ ├── State (PID-safe)     │
                                      │ ├── Lock (fd-lock)       │
                                      │ └── heal_state()         │
                                      └─────────────────────────┘
```

---

## Phase 1: Add treehouse-core dependency

**No behavior change.** Just wire the dependency.

### Files modified

| File | Change |
|---|---|
| `Cargo.toml` (workspace) | Add `treehouse-core = { path = "../treehouse_rust/crates/treehouse-core" }` |
| `crates/anti-workspace/Cargo.toml` | Add `treehouse-core = { workspace = true }` |
| `crates/anti-daemon/Cargo.toml` | Add `treehouse-core = { workspace = true }` |

### Verification

- `cargo check -p anti-workspace` passes
- `cargo check -p anti-daemon` passes
- All existing tests pass unchanged

---

## Phase 2: Implement AntiEnv + AntiPool

**New file: `crates/anti-workspace/src/pool.rs`**

### AntiEnv

```rust
use treehouse_core::env::{FileMeta, TreehouseEnv};
use std::path::{Path, PathBuf};

/// anti_subagent's environment — pool lives inside .anti_subagent/worktrees/.
pub struct AntiEnv {
    /// = ~/.anti_subagent or custom state_dir
    state_dir: PathBuf,
    /// = repo_root (for worktree creation context)
    repo_root: PathBuf,
}

impl TreehouseEnv for AntiEnv {
    fn pool_root(&self) -> Option<PathBuf> {
        Some(self.state_dir.join("worktrees"))
    }

    fn update_cache_path(&self) -> Option<PathBuf> {
        None  // anti_subagent manages its own updates
    }

    fn user_config_path(&self) -> Option<PathBuf> {
        None  // Config from ~/.anti_subagent/config.toml
    }

    // All filesystem methods delegate to std::fs
    fn read_file(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    // ... (same pattern as LooperEnv)
}
```

### AntiPool

```rust
use treehouse_core::{TreehouseCore, TreehouseConfig};
use treehouse_core::pool::{Pool, PoolError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Manages the worktree pool for anti_subagent.
pub struct AntiPool {
    core: TreehouseCore<AntiEnv>,
}

impl AntiPool {
    pub fn new(state_dir: &Path, config: PoolConfig) -> Self {
        let env = AntiEnv {
            state_dir: state_dir.to_path_buf(),
            repo_root: PathBuf::new(), // set per-acquire
        };
        Self {
            core: TreehouseCore::with_env(env, TreehouseConfig {
                max_trees: config.max_trees,
                ..TreehouseConfig::default_config()
            }),
        }
    }

    /// Acquire a worktree for a peer agent.
    pub fn acquire(
        &self,
        repo_root: &Path,
        remote_url: Option<&str>,
        holder: &str,
        ttl: Option<Duration>,
    ) -> Result<Lease, PoolError> {
        let pool = self.core.open_pool(repo_root, remote_url)?;
        let lease = pool.acquire_lease_with_ttl(holder, ttl)?;
        Ok(Lease {
            path: PathBuf::from(&lease.path),
            lease_id: lease.lease_id,
            holder: lease.lease_holder,
        })
    }

    /// Release a worktree.
    pub fn release(&self, repo_root: &Path, remote_url: Option<&str>, path: &Path) -> Result<bool, PoolError> {
        let pool = self.core.open_pool(repo_root, remote_url)?;
        pool.release(path.to_str().unwrap_or_default())
    }

    /// GC: reclaim stale/orphaned/dead-owner worktrees.
    pub fn gc(&self, repo_root: &Path, remote_url: Option<&str>) -> Result<GcResult, PoolError> {
        let pool = self.core.open_pool(repo_root, remote_url)?;
        pool.gc(&treehouse_core::pool::GcOptions { dry_run: false, ..Default::default() })
    }
}
```

### Verification

- Unit tests for `AntiEnv` (pool_root path, read/write roundtrip)
- Unit tests for `AntiPool` (acquire/release cycle)
- `cargo test -p anti-workspace` passes

---

## Phase 3: Replace anti-workspace subprocess calls

**Modified: `crates/anti-workspace/src/lib.rs`**

### Before (subprocess)

```rust
pub fn acquire(repo_root: &Path, holder: &str) -> Result<Lease> {
    let output = Command::new("treehouse")
        .args(["get", "--lease", "--lease-holder", holder, "--json"])
        .current_dir(repo_root)
        .output()?;
    // ... parse JSON
}
```

### After (library call)

```rust
pub fn acquire(pool: &AntiPool, repo_root: &Path, remote_url: Option<&str>, holder: &str) -> Result<Lease> {
    pool.acquire(repo_root, remote_url, holder, None)
        .map_err(|e| WorkspaceError::Treehouse(e.to_string()))
}

pub fn acquire_with_ttl(pool: &AntiPool, repo_root: &Path, remote_url: Option<&str>, holder: &str, ttl: Duration) -> Result<Lease> {
    pool.acquire(repo_root, remote_url, holder, Some(ttl))
        .map_err(|e| WorkspaceError::Treehouse(e.to_string()))
}

pub fn release(pool: &AntiPool, repo_root: &Path, remote_url: Option<&str>, lease: &Lease) -> Result<()> {
    pool.release(repo_root, remote_url, &lease.path)
        .map_err(|e| WorkspaceError::Treehouse(e.to_string()))
}
```

### Verification

- Existing integration tests pass (T1-T6)
- No subprocess calls in `strace` output
- Acquire/release latency < 5ms (vs ~50ms before)

---

## Phase 4: Wire into daemon process tracking

**Modified: `crates/anti-daemon/src/main.rs`**

### Pool initialization

```rust
// In daemon startup, create pool per state_dir
let pool = AntiPool::new(&state_dir, config.pool.clone());
```

### Improved spawn

```rust
async fn spawn(&self, params: SpawnParams) -> Result<AgentRecord> {
    // 1. Validate inputs
    // 2. Reserve identity (SQLite INSERT)
    // 3. Acquire workspace from POOL (not subprocess)
    let lease = self.pool.acquire(
        &params.repo_root,
        params.remote_url.as_deref(),
        &params.id,
        params.timeout.map(Duration::from_secs),
    )?;

    // 4. Spawn agent (existing logic)
    let child = cmd.spawn()?;
    let pid = child.id();

    // 5. Attach PID with owner_started_at (PID-reuse safe)
    self.store.attach_pid_with_timestamp(&params.id, pid)?;

    // 6. Track in HashMap
    self.children.lock().unwrap().insert(params.id.clone(), child);

    Ok(record)
}
```

### Improved reaper

```rust
// In reaper thread, release via pool on exit
fn reapExited(&mut self) {
    for (id, child) in self.children.iter_mut() {
        if let Some(status) = child.try_wait()? {
            // Release worktree via pool
            if let Some(lease) = self.store.get_lease(id) {
                let _ = self.pool.release(&lease.repo_root, lease.remote_url.as_deref(), &lease.path);
            }
            // Update status
            self.store.set_status(id, if status.success() { "Completed" } else { "Crashed" })?;
        }
    }
}
```

### Verification

- Agent spawn → acquire → run → exit → release cycle works
- Kill agent → release worktree immediately
- Daemon restart → `pool.gc()` reclaims orphans

---

## Phase 5: Fix PID-reuse + daemon lock

**Modified: `crates/anti-daemon/src/store.rs`**

### PID-reuse safe tracking

```sql
-- agents table: add owner_started_at column
ALTER TABLE agents ADD COLUMN owner_started_at INTEGER DEFAULT 0;
```

```rust
// When attaching PID, also record process start time
pub fn attach_pid_with_timestamp(&self, id: &str, pid: u32) -> Result<()> {
    let started_at = get_process_start_time(pid)?;  // via /proc or sysctl
    self.conn.execute(
        "UPDATE agents SET pid = ?1, owner_started_at = ?2 WHERE id = ?3",
        params![pid, started_at, id],
    )?;
    Ok(())
}

// When verifying liveness, check BOTH pid alive AND start time matches
pub fn is_agent_alive(&self, id: &str) -> Result<bool> {
    let (pid, expected_start): (i32, i64) = self.conn.query_row(
        "SELECT pid, owner_started_at FROM agents WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if pid == 0 { return Ok(false); }
    let actual_start = get_process_start_time(pid as u32)?;
    Ok(actual_start == expected_start)
}
```

### Daemon lock (using treehouse's fd-lock)

```rust
// In daemon startup, acquire exclusive lock
use std::fs::File;
use fd_lock::RwLock;

fn acquire_daemon_lock(state_dir: &Path) -> Result<RwLock<File>> {
    let lock_path = state_dir.join("daemon.lock");
    let file = File::create(&lock_path)?;
    let mut lock = RwLock::new(file);
    lock.write()?;  // blocks until acquired
    Ok(lock)  // guard held for daemon lifetime
}
```

### Verification

- Two daemons cannot run simultaneously
- PID reuse detected and handled correctly
- Existing tests pass

---

## Phase 6: Unified recovery

**Modified: `crates/anti-daemon/src/recovery.rs`**

### Replace duplicate recovery

```rust
// Before: two separate recovery paths
//   reconcile_on_start (main.rs) — dead agent detection
//   recover_on_restart (recovery.rs) — 3-phase recovery

// After: single recovery pipeline
pub fn recover_on_restart(pool: &AntiPool, store: &Store) -> Result<()> {
    // Phase 1: Treehouse heal_state (automatic on every pool operation)
    // Phase 2: Release dead agent worktrees via pool.gc()
    // Phase 3: Reconcile work items (existing logic)
    // Phase 4: Cancel stale queue items (existing logic)
}
```

### Remove lease sweeper

```rust
// Before: background thread every 15s
//   releases leases of agents in terminal states

// After: immediate release on agent exit
//   reaper thread calls pool.release() directly
//   + pool.gc() handles any remaining orphans
```

### Verification

- Daemon restart correctly reclaims all orphaned worktrees
- No duplicate recovery logic
- Existing integration tests pass

---

## Phase 7: Documentation + cleanup

### Files modified

| File | Change |
|---|---|
| `README.md` | Update architecture diagram |
| `crates/anti-workspace/README.md` | Document library usage |
| `crates/anti-daemon/src/main.rs` | Remove `treehouse_bin` config (no longer needed) |

### Config changes

```toml
# ~/.anti_subagent/config.toml

# Before:
treehouse_bin = "/usr/local/bin/treehouse"

# After: removed — treehouse-core is a library dependency
# [pool] section added:
[pool]
max-trees = 16
lock-timeout-secs = 10
gc-interval-secs = 300
```

---

## Data Flow: Spawn → Run → Release

```
1. CLI: anti spawn --id peer-1 --role worker --repo ~/project
   └── IPC to daemon

2. Daemon: spawn()
   ├── Validate inputs
   ├── SQLite INSERT agent (status=Created)
   ├── AntiPool::acquire(repo, remote, "peer-1", Some(30m))
   │   └── treehouse-core: Pool::get() with lease TTL
   │       └── short-lock protocol → worktree path + lease_id
   ├── Build command (harness adapter)
   ├── cmd.spawn() → child PID
   ├── store.attach_pid_with_timestamp(id, pid, started_at)
   ├── children.insert(id, child)
   └── Emit AgentStarted event

3. Agent runs in worktree
   ├── stdout/stderr captured to log file
   ├── Reaper thread polls try_wait() every 5s
   └── Lease TTL = max runtime (auto-expire safety net)

4. Agent exits
   ├── Reaper detects via try_wait()
   ├── AntiPool::release(repo, remote, worktree_path)
   │   └── treehouse-core: Pool::release() → worktree available
   ├── store.set_status(id, "Completed" or "Crashed")
   └── Emit AgentCompleted / AgentCrashed event

5. Background GC (every 5 min)
   └── AntiPool::gc()
       └── treehouse-core: Pool::gc()
           ├── Reclaim expired leases
           ├── Clear dead owners (PID mismatch)
           └── Drop missing paths
```

---

## Data Flow: Kill/Stop

```
User: anti stop peer-1
  │
  ├──→ IPC to daemon
  │
  ├──→ PeerManager::terminate(id)
  │      ├── kill -TERM to PID
  │      └── after grace: kill -9
  │
  ├──→ AntiPool::release(repo, remote, path)
  │      └── worktree back to pool immediately
  │
  └──→ store.set_status("Stopped")
         └── DB updated
```

---

## Before/After Comparison

| Aspect | Before | After |
|---|---|---|
| Acquire speed | ~50ms (subprocess) | <5ms (library call) |
| Release speed | ~50ms (subprocess) | <5ms (library call) |
| PID safety | `kill -0` only | `owner_started_at` verification |
| Daemon lock | None (stale socket risk) | `fd_lock` on `daemon.lock` |
| Orphan cleanup | `kill -0` scan on restart | `heal_state()` + `pool.gc()` |
| Lease release | 15s sweeper thread | Immediate on agent exit |
| Recovery | 2 duplicate paths | 1 unified pipeline |
| Config | `treehouse_bin` path | Zero — library dependency |

---

## Risk Assessment

| Risk | Impact | Mitigation |
|---|---|---|
| treehouse-core lock contention with SQLite | Low | Different files: `treehouse-state.lock` vs WAL |
| Breaking existing integration tests | Medium | Phase 1 is pure additive; tests pass unchanged |
| Thread safety of AntiPool | Low | `TreehouseCore` is `Send + Sync`, pool uses `Arc` |
| Backward compatibility | Low | Keep CLI `treehouse` binary available as fallback |
| Multi-repo support | Medium | Pool is per-repo; anti_subagent already tracks `repo_root` per agent |

---

## Timeline

| Phase | Effort | Dependencies |
|---|---|---|
| Phase 1: Add dependency | 1 hour | None |
| Phase 2: AntiEnv + AntiPool | 3 hours | Phase 1 |
| Phase 3: Replace subprocess calls | 4 hours | Phase 2 |
| Phase 4: Wire into daemon | 4 hours | Phase 3 |
| Phase 5: PID-reuse + daemon lock | 3 hours | Phase 4 |
| Phase 6: Unified recovery | 3 hours | Phase 5 |
| Phase 7: Documentation | 2 hours | Phase 6 |
| **Total** | **~20 hours** | |

---

*Plan version: 2026-08-17 | Status: Draft*
