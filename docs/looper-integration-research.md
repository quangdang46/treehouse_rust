# Looper × Treehouse: Integration Research

> Tích hợp treehouse-core API vào looper-rust để quản lý worktree pool, process tracking, và lease TTL — thay thế hệ thống hiện tại fragment và hay kill nhầm/quên kill.

---

## 1. Hiện trạng: Looper Session Management

### 1.1 Architecture hiện tại

```
looperd (daemon)
  ├── looper-scheduler  — tick loop, claim queue, recovery
  ├── looper-agent      — spawn/kill agent processes (5 vendors)
  ├── looper-git        — create/remove git worktrees
  ├── looper-storage    — SQLite (12 tables)
  └── looper-infra      — daemon lock, agent cleanup, worktree cleanup
```

### 1.2 Worktree lifecycle hiện tại

```
每 loop tạo worktree mới:
  Planner    → .looper/worktrees/planner-{id}/
  Reviewer   → .looper/worktrees/review-{id}/
  Worker     → .looper/worktrees/worker-{id}/
  Fixer      → .looper/worktrees/fixer-{id}/

Cleanup chạy mỗi 1 giờ (background thread)
Stale cleanup tại startup (heuristic: mod time + max_keep)
```

### 1.3 Process tracking hiện tại

```rust
// executor.rs: child process spawned with setpgid(0,0)
unsafe {
    cmd.as_std_mut().pre_exec(|| {
        setpgid(Pid::from_raw(0), Pid::from_raw(0)).ok();
        Ok(())
    });
}
let child_pgid = pid as i64;  // PGID = PID (child is group leader)

// Kill via killpg
killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
// After grace period (5s):
killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
```

### 1.4 5 Pain Points cần giải quyết

| # | Vấn đề | Hiện tại | Hậu quả |
|---|---|---|---|
| 1 | **Kill nhầm** | `stop_loop` chỉ update DB, không kill process | Agent tiếp tục chạy sau khi "stopped" |
| 2 | **Quên kill** | `shell.rs` orphan children khi timeout | Orphan processes tiêu resource |
| 3 | **PID reuse** | Lưu `i32` plain, verify lỏng (substring match) | Kill sai process |
| 4 | **Worktree leak** | Cleanup heuristic mỗi 1h | `.looper/worktrees/` grows unbounded |
| 5 | **Inconsistent stop** | `stop_loop` = DB-only, `daemon stop` = `pkill -f` | 5 cơ chế kill riêng biệt |

---

## 2. Treehouse Core API —那些 gì relevant

### 2.1 TreehouseEnv trait

```rust
pub trait TreehouseEnv: Send + Sync {
    fn pool_root(&self) -> Option<PathBuf>;
    fn update_cache_path(&self) -> Option<PathBuf>;
    fn user_config_path(&self) -> Option<PathBuf>;
    fn read_file(&self, path: &Path) -> io::Result<String>;
    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    fn ensure_dir(&self, path: &Path) -> io::Result<()>;
    fn path_exists(&self, path: &Path) -> bool;
    fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn file_meta(&self, path: &Path) -> io::Result<FileMeta>;
    fn env_var(&self, name: &str) -> Option<String>;
    fn env_var_os(&self, name: &str) -> Option<PathBuf>;
    fn cwd(&self) -> Option<PathBuf>;
}
```

### 2.2 Pool — Worktree Management

```rust
pub struct Pool {
    pub root: PathBuf,           // repo root
    pub dir: PathBuf,            // .looper/worktrees/<repo>-<hash>/
    pub(crate) git: Arc<dyn GitBackend>,
    pub(crate) process: Arc<ProcessTable>,
    pub(crate) config: TreehouseConfig,
    pub(crate) lock_timeout: Duration,
    pub(crate) env: Arc<dyn TreehouseEnv>,
}

// Acquire: short-lock protocol (5 steps)
pool.get(&AcquireOptions { branch, lease }) -> Acquired { name, path, branch, lease }

// Release: 3-phase protocol
pool.release(path) -> Result<(), PoolError>

// Lease with TTL
pool.acquire_lease_with_ttl(holder, ttl) -> LeaseInfo

// Status: heal + scan
pool.status() -> Vec<WorktreeStatus>

// GC: reclaim stale/orphaned/dead-owner
pool.gc(&GcOptions) -> GcResult
```

### 2.3 State — PID-Reuse Safe Tracking

```rust
pub struct WorktreeEntry {
    pub name: String,
    pub path: String,
    pub created_at: DateTime<Utc>,
    pub destroying: bool,
    pub owner_pid: i32,           // PID-reuse safe via owner_started_at
    pub owner_started_at: i64,    // epoch millis — verify process identity
    pub leased: bool,
    pub lease_id: String,         // 128-bit random hex
    pub lease_holder: String,
    pub leased_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>, // TTL auto-reclaim
}

// heal_state: clears dead owners, drops missing paths
// Never touches lease fields (leases survive heal)
```

### 2.4 Lock — Exclusive fd-lock + Backoff

```rust
pub fn with_state_lock<T, E>(
    pool_dir: &Path,
    lock_timeout: Duration,  // default 10s
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, LockError<E>>
// Exponential backoff: 10ms → 20ms → ... → 500ms cap
// RAII guard — dropped on every exit path
```

---

## 3. Mapping: Treehouse → Looper

### 3.1 Concept Mapping

| Treehouse | Looper hiện tại | Treehouse giải quyết |
|---|---|---|
| `Pool` | `.looper/worktrees/` + `worktrees` table | Pool reusable thay vì create-per-loop |
| `WorktreeEntry.owner_pid` | `agent_executions.pid` | PID-reuse safe via `owner_started_at` |
| `WorktreeEntry.leased` + `expires_at` | `idle_timeout` + `max_runtime` | Lease TTL = auto-reclaim, zero orphan |
| `heal_state()` | `agent_cleanup.rs` + `recovery.rs` | Auto-clear dead owners on every lock |
| `with_state_lock()` | `locks` table (TTL-based) | File-level exclusive lock, simpler |
| `pool.gc()` | `worktree_cleanup/` (hourly) | Immediate reclamation, no timer |
| `pool.release()` | `stop_loop` (DB-only) | Kill process + cleanup worktree |
| `TreehouseEnv` | Hardcoded `$HOME/.treehouse` | Consumer controls一切 |

### 3.2 What Looper Keeps

| Component | Giữ nguyên | Lý do |
|---|---|---|
| `looper-storage` SQLite | ✅ | 12 tables, event log, queue — treehouse không thay thế |
| `looper-scheduler` | ✅ | Tick loop, claim, dispatch — orchestration layer |
| `looper-agent` executor | ✅ | Vendor-specific spawning, stdout/stderr capture |
| `looper-git` gateway | ⚠️ | Giữ cho git operations, nhưng treehouse dùng `GitBackend` trait |
| `looper-api` REST | ✅ | API endpoints — treehouse là library, không thay thế API |
| `looper-cli` | ✅ | CLI client — treehouse là dependency, không thay thế |

### 3.3 What Looper Replaces

| Component | Thay thế bằng | Chi tiết |
|---|---|---|
| `cleanup_stale_worktrees()` | `pool.gc()` | Immediate, không heuristic |
| `worktree_cleanup/` (hourly) | `pool.gc()` + lease TTL | Auto-reclaim expired leases |
| `agent_cleanup.rs` orphan scan | `heal_state()` | PID-reuse safe, chạy mỗi lock acquisition |
| `recovery.rs` 5-phase | `heal_state()` + `pool.gc()` | Đơn giản hóa recovery |
| `ActiveExecutionRegistry` | `Pool.status()` + lease | Lease holder = agent identity |
| PID tracking trong SQLite | `WorktreeEntry.owner_pid` | Persistent, PID-reuse safe |
| `shell.rs` orphan handling | Lease TTL auto-expire | Zero orphan, zero manual kill |

---

## 4. Integration Architecture

### 4.1 LooperEnv — TreehouseEnv Implementation

```rust
use treehouse_core::env::{FileMeta, TreehouseEnv};
use std::path::{Path, PathBuf};

/// Looper's environment — zero .treehouse, zero config files.
/// Pool lives inside .looper/worktrees/.
pub struct LooperEnv {
    /// = repo_path/.looper
    base: PathBuf,
}

impl LooperEnv {
    pub fn new(repo_path: &Path) -> Self {
        Self { base: repo_path.join(".looper") }
    }
}

impl TreehouseEnv for LooperEnv {
    fn pool_root(&self) -> Option<PathBuf> {
        Some(self.base.join("worktrees"))
    }

    fn update_cache_path(&self) -> Option<PathBuf> {
        None  // Looper tự quản update checks
    }

    fn user_config_path(&self) -> Option<PathBuf> {
        None  // Config từ looper.toml
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, data)
    }

    fn ensure_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)
            .map(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).collect())
    }

    fn file_meta(&self, path: &Path) -> io::Result<FileMeta> {
        let meta = std::fs::metadata(path)?;
        Ok(FileMeta { size: meta.len(), modified: meta.modified().ok() })
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn env_var_os(&self, name: &str) -> Option<PathBuf> {
        std::env::var_os(name).map(PathBuf::from)
    }

    fn cwd(&self) -> Option<PathBuf> {
        std::env::current_dir().ok()
    }
}
```

### 4.2 LooperPool — Wrapper cho Integration

```rust
use treehouse_core::{TreehouseCore, TreehouseConfig};
use treehouse_core::pool::{AcquireOptions, Pool, PoolError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Manages the worktree pool for a single project.
pub struct LooperPool {
    core: TreehouseCore<LooperEnv>,
    repo_root: PathBuf,
    remote_url: Option<String>,
}

impl LooperPool {
    pub fn new(repo_root: &Path, remote_url: Option<String>) -> Self {
        let env = LooperEnv::new(repo_root);
        let config = TreehouseConfig {
            max_trees: 16,  // configurable per project
            ..TreehouseConfig::default_config()
        };
        Self {
            core: TreehouseCore::with_env(env, config),
            repo_root: repo_root.to_path_buf(),
            remote_url,
        }
    }

    /// Acquire a worktree for an agent run.
    /// Returns the worktree path + lease info.
    pub fn acquire_for_agent(
        &self,
        agent_vendor: &str,
        loop_id: &str,
        timeout: Duration,
    ) -> Result<AcquiredWorktree, PoolError> {
        let pool = self.core.open_pool(&self.repo_root, self.remote_url.as_deref())?;
        let holder = format!("{}-{}", agent_vendor, loop_id);
        let lease = pool.acquire_lease_with_ttl(
            &holder,
            Some(timeout),  // max_runtime as lease TTL
        )?;
        Ok(AcquiredWorktree {
            path: PathBuf::from(&lease.path),
            lease_id: lease.lease_id,
            holder: lease.lease_holder,
        })
    }

    /// Release a worktree (agent done or killed).
    pub fn release(&self, worktree_path: &Path) -> Result<bool, PoolError> {
        let pool = self.core.open_pool(&self.repo_root, self.remote_url.as_deref())?;
        pool.release(worktree_path.to_str().unwrap_or_default())
    }

    /// Get status of all worktrees.
    pub fn status(&self) -> Result<Vec<WorktreeStatus>, PoolError> {
        let pool = self.core.open_pool(&self.repo_root, self.remote_url.as_deref())?;
        pool.status()
    }

    /// GC: reclaim stale/orphaned/dead-owner worktrees.
    pub fn gc(&self) -> Result<GcResult, PoolError> {
        let pool = self.core.open_pool(&self.repo_root, self.remote_url.as_deref())?;
        pool.gc(&GcOptions { dry_run: false, ..Default::default() })
    }
}

pub struct AcquiredWorktree {
    pub path: PathBuf,
    pub lease_id: String,
    pub holder: String,
}
```

### 4.3 Integration với Agent Executor

```rust
// executor.rs — thay đổi chính

impl ConfiguredExecutor {
    pub async fn start_with_pool(
        &self,
        pool: &LooperPool,
        input: RunInput,
    ) -> Result<Execution, AgentError> {
        // 1. Acquire worktree từ pool (với lease TTL)
        let acquired = pool.acquire_for_agent(
            &self.config.vendor.to_string(),
            &input.loop_id,
            Duration::from_secs(input.timeout),
        ).map_err(|e| AgentError::SpawnFailed(e.to_string()))?;

        // 2. Spawn agent trong worktree (giữ nguyên logic hiện tại)
        let mut cmd = self.build_command(&input, &acquired.path);
        // ... setpgid, pre_exec, spawn ...

        // 3. Agent chạy, lease auto-expires nếu timeout
        // 4. Khi agent done → pool.release()
        // 5. Khi timeout → lease expires → pool.gc() auto-reclaim
    }
}
```

### 4.4 Integration với stop_loop API

```rust
// main.rs — stop_loop giờ KILL process + release worktree

async fn stop_loop(&self, project_name: &str, loop_seq: i64) -> Result<(), ApiError> {
    let rec = self.repos.loops.get_by_seq(loop_seq)...;

    // 1. Kill running agent process (thêm mới!)
    if let Some(execution) = self.active_executions.get_by_loop(&rec.id) {
        execution.kill("stopped by user").await?;
    }

    // 2. Release worktree từ pool (thêm mới!)
    if let Some(worktree_path) = self.get_worktree_for_loop(&rec.id) {
        self.looper_pool.release(&worktree_path)?;
    }

    // 3. Update DB status (giữ nguyên)
    self.repos.loops.update_status(&rec.id, "stopped", &finished)?;
    self.repos.queue.cancel_by_loop(&rec.id, &finished, Some("stopped by user"))?;

    Ok(())
}
```

---

## 5. File-by-File Integration Points

### 5.1 New Files

| File | Purpose |
|---|---|
| `looper-agent/src/pool.rs` | `LooperEnv` + `LooperPool` wrapper |
| `looper-agent/src/lease.rs` | Lease lifecycle management |

### 5.2 Modified Files

| File | Changes |
|---|---|
| `looperd/Cargo.toml` | Add `treehouse-core` dependency |
| `looperd/src/main.rs` | Init `LooperPool` per project, replace `cleanup_stale_worktrees` with `pool.gc()`, update `stop_loop` to kill + release |
| `looper-agent/src/executor.rs` | `start_with_pool()` method, release on completion |
| `looper-scheduler/src/recovery.rs` | Replace orphan cleanup with `heal_state()` |
| `looper-config/src/types.rs` | Add `pool: PoolConfig` section |
| `looper-infra/src/worktree_cleanup/` | Delegate to `pool.gc()` |

### 5.3 Config Changes

```toml
# looper.toml — thêm section mới
[pool]
max_trees = 16          # worktrees per project
lock_timeout_secs = 10  # fd-lock timeout
gc_interval_secs = 300  # background GC interval (5 min)
```

```rust
// types.rs — thêm PoolConfig
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct PoolConfig {
    pub max_trees: u32,
    pub lock_timeout_secs: u64,
    pub gc_interval_secs: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_trees: 16,
            lock_timeout_secs: 10,
            gc_interval_secs: 300,
        }
    }
}
```

---

## 6. Migration Path

### Phase 1: Add treehouse-core dependency (no behavior change)

```
looperd/Cargo.toml  ← add treehouse-core = { path = "../treehouse_rust/crates/treehouse-core" }
looper-agent/Cargo.toml ← add treehouse-core
```

### Phase 2: Implement LooperEnv + LooperPool

```
looper-agent/src/pool.rs ← NEW: LooperEnv, LooperPool, AcquiredWorktree
```

### Phase 3: Wire into executor

```
executor.rs ← add start_with_pool(), release on completion
```

### Phase 4: Wire into stop_loop + recovery

```
main.rs ← stop_loop: kill + release
recovery.rs ← heal_state() replaces orphan scan
```

### Phase 5: Replace cleanup subsystem

```
worktree_cleanup/ ← delegate to pool.gc()
cleanup_stale_worktrees() ← remove (replaced by GC)
```

### Phase 6: Config + docs

```
types.rs ← PoolConfig
README.md ← update architecture docs
```

---

## 7. Before/After Comparison

### 7.1 Session Kill Flow

```
BEFORE:
  looper stop <project> <seq>
    → POST /api/.../terminate
    → close_loop() — DB status = "closed"
    → cancel queue items
    → ❌ Agent process CONTINUES running

AFTER:
  looper stop <project> <seq>
    → POST /api/.../terminate
    → close_loop() — DB status = "closed"
    → active_executions.kill("stopped by user") — SIGTERM to process group
    → looper_pool.release(worktree_path) — worktree back to pool
    → ✅ Agent process KILLED, worktree REUSED
```

### 7.2 Orphan Process Handling

```
BEFORE:
  Daemon startup → agent_cleanup.rs
    → SELECT active executions FROM DB
    → ps -p <pid> -o command= | contains("claude")  ← LỎNG
    → kill -TERM -<pgid>
    → ⚠️ PID reuse risk, substring match

AFTER:
  Every lock acquisition → heal_state()
    → Check owner_pid + owner_started_at  ← PID-REUSE SAFE
    → Dead owner → auto-clear, worktree available
    → ✅ No startup scan needed, no PID reuse risk
```

### 7.3 Worktree Lifecycle

```
BEFORE:
  Loop #1: create planner-abc123/
  Loop #2: create worker-def456/
  Loop #3: create review-ghi789/
  ... (unbounded)
  Every 1h: worktree_cleanup scans + removes old ones
  ⚠️ Grows unbounded between cleanup cycles

AFTER:
  Pool: max_trees=16
  Loop #1: pool.acquire() → worktree #1 (lease: 30m)
  Loop #2: pool.acquire() → worktree #2 (lease: 30m)
  Loop #3: pool.acquire() → worktree #1 (REUSED after release)
  Lease expires → pool.gc() auto-reclaims
  ✅ Bounded, reusable, immediate cleanup
```

### 7.4 Stop Semantics

```
BEFORE (5 different mechanisms):
  stop_loop     → DB-only (no kill)
  cancel_run    → DB-only (no kill)
  daemon stop   → pkill -f looperd (kills daemon, not agent)
  timeout       → SIGTERM → SIGKILL (only for current execution)
  recovery      → ps scan + kill (startup only)

AFTER (1 unified mechanism):
  stop_loop     → active_executions.kill() + pool.release()
  cancel_run    → active_executions.kill() + pool.release()
  daemon stop   → kill_all() + pool.gc()
  timeout       → lease expires → pool.gc() auto-reclaim
  recovery      → heal_state() on every lock acquisition
  ✅ Consistent, immediate, zero orphan
```

---

## 8. Risk Assessment

| Risk | Impact | Mitigation |
|---|---|---|
| treehouse-core file lock conflicts with SQLite | Low | treehouse uses `fd-lock` on `treehouse-state.lock`, SQLite uses WAL — different files |
| Lease TTL too short → premature reclaim | Medium | Configurable per project, default 30min |
| Lease TTL too long → resource waste | Low | `pool.gc()` reclaims stale leases immediately |
| Breaking existing worktree DB records | Medium | Keep `worktrees` table as read-only audit trail, treehouse manages actual state |
| Multi-daemon conflict | Low | treehouse lock serializes all operations per pool |
| Git operations under lock | Low | treehouse short-lock protocol: git OUTSIDE lock |

---

## 9. Open Questions

1. **Multi-project pools**: Treehouse pool is per-repo. Looper manages multiple projects. Should each project get its own pool, or share one?

2. **Worktree DB sync**: Looper's `worktrees` table tracks branch, status, metadata. Treehouse has its own `treehouse-state.json`. Which is source of truth?

3. **Native resume support**: Claude Code's `--resume` needs `native_session_id`. Treehouse leases don't carry vendor-specific metadata. How to bridge?

4. **Graceful shutdown**: Currently `cleanup_stale_worktrees(max_keep=0)`. With treehouse, should shutdown call `pool.gc(dry_run=false)` or let leases expire naturally?

5. **Concurrent access**: Treehouse lock is file-based (one process). Looper daemon is single-process. Safe, but what about `loopernet` multi-node?

---

*Research version: 2026-08-17 | Author: treehouse × looper integration*
