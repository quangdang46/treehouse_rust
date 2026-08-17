# Looper × Treehouse Integration Plan

> Replace looper's fragmented session management with treehouse-core's unified worktree pool, PID-reuse-safe process tracking, and lease-based TTL auto-reclamation.

---

## Goals

1. **Zero orphan processes** — lease TTL + `heal_state()` replaces ad-hoc timeout/kill
2. **Zero kill-wrong** — `stop_loop` kills the actual agent process, not just DB status
3. **Zero worktree leaks** — bounded pool (`max_trees`) + immediate GC replaces hourly heuristic cleanup
4. **PID-reuse safe** — `owner_started_at` verification replaces loose `ps | contains` matching
5. **Unified stop semantics** — one mechanism instead of five different kill paths

---

## Non-Goals

- Rewrite looper's scheduler, API, or CLI (keep existing interfaces)
- Replace SQLite storage (treehouse state is additive, not replacement)
- Change agent vendor spawning logic (Claude, Codex, etc.)
- Multi-node coordination (loopernet stays unchanged)

---

## Architecture

```
                    BEFORE                              AFTER
                    ──────                              ─────

looperd                                  looperd
  ├── looper-agent                         ├── looper-agent
  │   └── executor.rs                      │   ├── executor.rs (acquire pool)
  │       └── setpgid + killpg             │   └── pool.rs (NEW: LooperEnv + LooperPool)
  ├── looper-git                           ├── looper-git
  │   └── create/cleanup worktrees         │   └── create worktrees (kept for git ops)
  ├── looper-infra                         ├── looper-infra
  │   ├── agent_cleanup.rs                 │   ├── agent_cleanup.rs (simplified)
  │   ├── worktree_cleanup/                │   └── worktree_cleanup/ (delegates to pool.gc)
  │   └── recovery.rs                      │
  └── looper-storage                       └── looper-storage
      └── 12 SQLite tables                     └── 12 SQLite tables (unchanged)

                                           ┌──────────────────────────────┐
                                           │  treehouse-core (dependency) │
                                           │  ├── Pool (worktree pool)    │
                                           │  ├── State (PID-reuse safe)  │
                                           │  ├── Lock (fd-lock + backoff)│
                                           │  └── heal_state() (auto)     │
                                           └──────────────────────────────┘
```

---

## Phase 1: Add treehouse-core dependency

**No behavior change.** Just wire the dependency.

### Files modified

| File | Change |
|---|---|
| `Cargo.toml` (workspace) | Add `treehouse-core = { path = "../treehouse_rust/crates/treehouse-core" }` |
| `crates/looper-agent/Cargo.toml` | Add `treehouse-core = { workspace = true }` |
| `crates/looperd/Cargo.toml` | Add `treehouse-core = { workspace = true }` |

### Verification

- `cargo check -p looper-agent` passes
- `cargo check -p looperd` passes
- All existing tests pass unchanged

---

## Phase 2: Implement LooperEnv + LooperPool

**New file: `crates/looper-agent/src/pool.rs`**

### LooperEnv

Implements `TreehouseEnv` trait. Pool root = `.looper/worktrees/`. No config files, no update cache.

```rust
pub struct LooperEnv { base: PathBuf }

impl TreehouseEnv for LooperEnv {
    fn pool_root(&self) -> Option<PathBuf> {
        Some(self.base.join("worktrees"))
    }
    fn update_cache_path(&self) -> Option<PathBuf> { None }
    fn user_config_path(&self) -> Option<PathBuf> { None }
    // All filesystem methods delegate to std::fs
}
```

### LooperPool

Wraps `TreehouseCore<LooperEnv>` with looper-specific methods.

```rust
pub struct LooperPool {
    core: TreehouseCore<LooperEnv>,
    repo_root: PathBuf,
    remote_url: Option<String>,
}

impl LooperPool {
    pub fn new(repo_root: &Path, remote_url: Option<String>, config: PoolConfig) -> Self;
    pub fn acquire_for_agent(&self, vendor: &str, loop_id: &str, timeout: Duration) -> Result<AcquiredWorktree>;
    pub fn release(&self, path: &Path) -> Result<bool>;
    pub fn status(&self) -> Result<Vec<WorktreeStatus>>;
    pub fn gc(&self) -> Result<GcResult>;
}
```

### PoolConfig

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct PoolConfig {
    pub max_trees: u32,           // default: 16
    pub lock_timeout_secs: u64,   // default: 10
    pub gc_interval_secs: u64,    // default: 300 (5 min)
}
```

### Config integration

| File | Change |
|---|---|
| `crates/looper-config/src/types.rs` | Add `pool: Option<PoolConfig>` to `Config` struct |
| `crates/looper-config/src/types.rs` | Add `PoolConfig` struct with defaults |

### Verification

- Unit tests for `LooperEnv` (read/write roundtrip, pool_root path)
- Unit tests for `LooperPool` (acquire/release cycle with `InMemoryEnv`)
- `cargo test -p looper-agent` passes

---

## Phase 3: Wire into agent executor

**Modified: `crates/looper-agent/src/executor.rs`**

### New method: `start_with_pool`

```rust
impl ConfiguredExecutor {
    pub async fn start_with_pool(
        &self,
        pool: &LooperPool,
        input: RunInput,
    ) -> Result<Execution, AgentError> {
        // 1. Acquire worktree from pool (lease TTL = max_runtime)
        let acquired = pool.acquire_for_agent(
            &self.config.vendor.to_string(),
            &input.loop_id,
            Duration::from_secs(input.timeout),
        )?;

        // 2. Spawn agent in worktree (existing logic)
        let mut cmd = self.build_command(&input, &acquired.path);
        // ... setpgid, pre_exec, spawn ...

        // 3. Store lease_id in Execution for release on completion
    }
}
```

### Modified: `Execution::run_loop`

After agent exits, release worktree back to pool:

```rust
// In run_loop(), after child exits:
if let Some(ref pool) = self.pool {
    let _ = pool.release(Path::new(&self.input.working_directory));
}
```

### Modified: `Execution::kill`

After SIGTERM/SIGKILL, release worktree:

```rust
pub async fn kill(&self, reason: &str) -> Result<(), AgentError> {
    self.killed_flag.store(true, Ordering::SeqCst);
    if let Some(pid) = self.state.lock().await.pid {
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    // Release worktree back to pool
    if let Some(ref pool) = self.pool {
        let _ = pool.release(Path::new(&self.input.working_directory));
    }
    Ok(())
}
```

### Verification

- E2E test: acquire → spawn agent → agent exits → worktree released
- E2E test: acquire → spawn agent → kill → worktree released
- Existing executor tests pass

---

## Phase 4: Wire into stop_loop + recovery

**Modified: `crates/looperd/src/main.rs`**

### RuntimeState changes

```rust
pub struct RuntimeState {
    // ... existing fields ...
    pools: HashMap<String, LooperPool>,  // per-project pools
}
```

### Improved stop_loop

```rust
async fn stop_loop(&self, project_name: &str, loop_seq: i64) -> Result<(), ApiError> {
    let rec = self.repos.loops.get_by_seq(loop_seq)?;

    // 1. Kill running agent process (NEW!)
    if let Some(execution) = self.active_executions.get_by_loop(&rec.id) {
        execution.kill("stopped by user").await?;
    }

    // 2. Release worktree from pool (NEW!)
    if let Some(path) = self.get_worktree_for_loop(&rec.id) {
        if let Some(pool) = self.pools.get(project_name) {
            let _ = pool.release(&path);
        }
    }

    // 3. Update DB status (unchanged)
    self.repos.loops.update_status(&rec.id, "stopped", &now)?;
    self.repos.queue.cancel_by_loop(&rec.id, &now, Some("stopped by user"))?;

    Ok(())
}
```

### Improved close_loop

Same pattern as stop_loop — kill + release + DB update.

### Improved recovery

**Modified: `crates/looper-scheduler/src/recovery.rs`**

Replace Phase 1 (orphan cleanup via `ps` scan) with treehouse's `heal_state()`:

```rust
// Before: 5-phase recovery with ps-based orphan detection
// After: heal_state() runs on every lock acquisition (automatic)
//        + pool.gc() reclaims stale leases
```

Keep Phases 2-4 (expired locks, stale runs, stale queue) — these are looper-specific.

### Verification

- `looper stop <project> <seq>` kills agent AND releases worktree
- Daemon startup: `pool.gc()` reclaims orphans from previous crash
- Existing recovery tests pass

---

## Phase 5: Replace cleanup subsystem

**Modified: `crates/looper-infra/src/worktree_cleanup/`**

### Simplified cleanup

Replace the hourly `worktree_cleanup` plan+execute cycle with treehouse's `pool.gc()`:

```rust
// Before: hourly background thread scanning worktrees vs DB
// After: pool.gc() called periodically (every gc_interval_secs)
//        + pool.gc() called on graceful shutdown
```

### Removed code

| File | Action |
|---|---|
| `worktree_cleanup/plan.rs` | Simplify — delegate to `pool.gc()` |
| `worktree_cleanup/run.rs` | Simplify — delegate to `pool.gc()` |
| `cleanup_stale_worktrees()` in `main.rs` | Remove — replaced by `pool.gc()` |

### Verification

- No orphan worktrees after daemon restart
- Graceful shutdown reclaims all worktrees
- Existing cleanup tests pass (or adapted)

---

## Phase 6: Documentation + examples

### Files modified

| File | Change |
|---|---|
| `README.md` | Update architecture section |
| `docs/` | Add integration guide |
| `crates/looper-agent/src/pool.rs` | Add doc comments |

### Documentation

- Architecture diagram showing treehouse-core integration
- Configuration reference for `[pool]` section
- Migration guide for existing users

---

## Data Flow: Acquire → Run → Release

```
1. Scheduler claims queue item
   └── dispatches to role processor (Planner, Worker, etc.)

2. Role processor calls executor.start()
   └── executor calls pool.acquire_for_agent(vendor, loop_id, timeout)
       └── treehouse: Pool::get() with lease TTL
           └── short-lock protocol:
               1. OUTSIDE lock: git fetch
               2. LOCK #1: scan for available worktree, stamp lease
               3. OUTSIDE lock: git reset
               4. LOCK #2: re-validate reservation
               5. OUTSIDE lock: run hooks
           └── returns Acquired { path, lease_id, holder }

3. Agent runs in worktree
   ├── stdout/stderr captured
   ├── idle timeout monitors output
   └── max runtime = lease TTL

4. Agent exits (normal or killed)
   └── executor calls pool.release(path)
       └── treehouse: Pool::release() with 3-phase protocol:
           1. OUTSIDE lock: resolve branch
           2. LOCK #1: find entry, validate lease, run before_reset
           3. OUTSIDE lock: git reset
           4. LOCK #2: clear lease, write state
       └── worktree available for next loop

5. Background GC (every 5 min)
   └── pool.gc() reclaims:
       ├── Expired leases (TTL passed)
       ├── Dead owners (process gone)
       └── Missing paths (worktree deleted externally)
```

---

## Data Flow: Stop/Kill

```
User runs: looper stop <project> <seq>
  │
  ├──→ POST /api/projects/{project}/loops/{seq}/terminate
  │      │
  │      ├──→ active_executions.kill("stopped by user")
  │      │      └── killpg(SIGTERM) to process group
  │      │      └── after grace period: killpg(SIGKILL)
  │      │
  │      ├──→ pool.release(worktree_path)
  │      │      └── worktree back to pool (available for reuse)
  │      │
  │      └──→ repos.loops.update_status("closed")
  │             └── DB status updated
  │
  └──→ Result: agent killed, worktree reused, DB updated
```

---

## Configuration Reference

```toml
# looper.toml

[pool]
# Maximum worktrees per project (default: 16)
max-trees = 16

# File lock timeout for pool state (default: 10s)
lock-timeout-secs = 10

# Background GC interval (default: 300s = 5 min)
gc-interval-secs = 300
```

---

## Risk Assessment

| Risk | Impact | Mitigation |
|---|---|---|
| Treehouse file lock vs SQLite | Low | Different files: `treehouse-state.lock` vs WAL |
| Lease TTL too short | Medium | Configurable per project, default 30min |
| Breaking existing worktree DB records | Medium | Keep `worktrees` table as audit trail |
| Multi-daemon conflict | Low | Treehouse lock serializes per pool |
| Git operations under lock | Low | Short-lock protocol: git OUTSIDE lock |

---

## Timeline

| Phase | Effort | Dependencies |
|---|---|---|
| Phase 1: Add dependency | 1 hour | None |
| Phase 2: LooperEnv + LooperPool | 4 hours | Phase 1 |
| Phase 3: Executor integration | 6 hours | Phase 2 |
| Phase 4: stop_loop + recovery | 4 hours | Phase 3 |
| Phase 5: Cleanup replacement | 3 hours | Phase 4 |
| Phase 6: Documentation | 2 hours | Phase 5 |
| **Total** | **~20 hours** | |

---

*Plan version: 2026-08-17 | Status: Draft*
