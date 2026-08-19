# Research: Daemon-Based Auto-Clean for Treehouse

> **Date**: 2026-08-19
> **Author**: quangdang46
> **Status**: Draft — needs design review before implementation

---

## 1. Current State: How Cleanup Works Today

Treehouse is explicitly a **"no-daemon CLI"** — every operation is an inline command, state is an on-disk JSON file guarded by a file lock. Cleanup is manual or semi-automatic.

### 1.1 Existing Cleanup Mechanisms

| Mechanism | Trigger | Scope | Safety |
|-----------|---------|-------|--------|
| `treehouse run -- <cmd>` | Explicit invocation | Single worktree | RAII guard + TTL lease; cleanup on every exit path (exit, signal, panic) |
| `treehouse return` | Explicit invocation | Single worktree | Conditional on lease ID (ABA-safe) |
| `treehouse gc --all --yes` | Manual / cron | All pools globally | Dry-run default; only reclaims idle + clean + merged + expired-lease trees |
| `treehouse prune --yes` | Manual / cron | Single pool | Dry-run default; removes stale idle worktrees (clean + merged only) |
| `heal_state()` | Implicit on every state read | Single pool | Clears dead owner reservations; drops missing-path entries; **never touches leases** |
| TTL leases (`--ttl 30m`) | Self-expiring | Single worktree | Lease expires → becomes stale → eligible for next `gc` sweep |

### 1.2 What "heal_state" Does Automatically

`heal_state()` runs on **every state read** (`get`, `status`, `gc`, `prune`, `return`, `run`). It:

1. **Clears dead owner reservations** — if `owner_pid != 0` but the process no longer matches `owner_started_at`, zero the owner fields and `destroying`.
2. **Drops entries whose path no longer exists on disk** — removes phantom entries.
3. **Never touches lease fields** — valid or stale, leases survive heal_state.

This means owner-based worktrees self-heal passively. But **leases never self-heal** — only `return` or explicit `destroy --include-leased` clears them.

### 1.3 The Gap: What Requires Manual Intervention

| Problem | Current behavior | Pain point |
|---------|-----------------|------------|
| Agent crashes with TTL lease | Lease expires, but worktree stays `leased` until someone runs `gc` | Disk accumulates stale leased trees between sweeps |
| Agent crashes without TTL | Lease is permanent; only `return --force` or `destroy --include-leased` reclaims it | Requires human intervention |
| `treehouse run` itself crashes | TTL lease protects the tree; but cleanup never runs | Worktree left dirty + leased until next `gc` |
| Pool exhaustion | User must run `gc` or increase `max_trees` | No proactive reclamation |
| Orphaned worktrees | Only removed with `gc --prune-orphans --yes` | Must be explicitly opted in |
| Disk pressure | `doctor` warns at <10% free; no action taken | No automatic reclamation under pressure |

### 1.4 README's "No Daemon" Principle

From the README design principles:

> **No daemon** | Inline commands; the state file + lock is the whole coordination story

This is a deliberate architectural choice inherited from the Go original. The rationale:
- Simpler deployment (single binary, no service management)
- No background resource consumption
- File lock is sufficient for cooperative coordination
- TTL leases + manual `gc` cover most cases

---

## 2. The Problem Statement

Even with TTL leases, **stale worktrees accumulate between manual `gc` sweeps**. The user's observation:

> "We need a daemon to track and clean treehouse even when treehouse is not working or errors."

Concretely:
1. **No automatic periodic cleanup** — if no one runs `gc`, leaked worktrees persist indefinitely.
2. **No reactive cleanup** — when a lease expires or a process dies, nothing proactively reclaims the tree.
3. **No cross-session awareness** — each CLI invocation is stateless; there's no background monitor watching all pools.
4. **Error states compound** — if `treehouse run` errors out mid-cleanup, the worktree is left in a half-cleaned state with no retry mechanism.

---

## 3. Proposed Solution: `treehouse watch` (Daemon Mode)

### 3.1 Design Philosophy

Rather than a full daemon, introduce a **lightweight background watcher** that:

- Runs as a foreground process or system service (user's choice)
- Periodically sweeps all managed pools
- Reacts to file-system events on state files (optional, for responsiveness)
- Never conflicts with CLI operations (same file-lock protocol)
- Can be started/stopped independently of any agent session

### 3.2 Proposed Commands

```bash
# Start the watcher (foreground, daemon, or one-shot)
treehouse watch                     # foreground, sweep every 60s
treehouse watch --daemon            # background service (systemd/launchd/Windows Service)
treehouse watch --interval 30s      # custom sweep interval
treehouse watch --once              # one-shot sweep (exit after first pass)

# Status of the watcher
treehouse watch status              # is a watcher running? PID, uptime, last sweep

# Stop the watcher
treehouse watch stop                # sends SIGTERM to the running watcher
```

### 3.3 What the Watcher Does Each Sweep

```
For each pool under ~/.treehouse/ (or --all pools):
  1. Read + heal state (same as every CLI command)
  2. For each worktree:
     a. If stale lease (expired TTL) + idle + clean + merged → reclaim (gc logic)
     b. If dead owner + idle → clear reservation (heal logic)
     c. If orphaned + --prune-orphans → remove
     d. If dirty + in-use for > configured TTL → log warning (never auto-destroy dirty)
  3. Write state back
  4. Emit structured log (JSON/TOON) for observability
```

### 3.4 New Config Options

```toml
# treehouse.toml or ~/.config/treehouse/config.toml

[watch]
# Enable automatic cleanup sweeps
enabled = true

# Sweep interval in seconds (default: 60)
interval_secs = 60

# Automatically reclaim expired-lease worktrees (default: true)
auto_gc = true

# Automatically reclaim orphaned worktrees (default: false)
auto_prune_orphans = false

# Log level: quiet, info, verbose (default: info)
log_level = "info"

# Emit events for external monitoring (default: false)
emit_events = false
```

### 3.5 Safety Invariants (Must Preserve)

| Invariant | How it's preserved |
|-----------|-------------------|
| Valid leases are never touched | Watcher uses same `gc` logic — valid lease → skip |
| In-use worktrees are never evicted | Same `is_worktree_in_use` process scan |
| Dirty worktrees are never auto-destroyed | Watcher only reclaims clean + merged trees |
| File-lock protocol is unchanged | Watcher acquires same lock as CLI commands |
| ABA-safety on return | Same lease ID matching |
| Dry-run by default (for destructive ops) | Watcher auto-commits only for provably-safe reclaims |
| Crash-recovery conservatism | Corrupt state → mark as leased (same recovery) |

---

## 4. Architecture Options

### Option A: Simple Timer Loop (Recommended for v1)

```
treehouse watch
  └─ loop { sleep(interval); sweep_all_pools(); }
```

- **Pros**: Simple, no new dependencies, easy to reason about
- **Cons**: No event-driven responsiveness; worst-case latency = interval
- **Implementation**: New `crates/treehouse-core/src/watch.rs` + CLI subcommand
- **Dependencies**: None new (tokio not needed; `std::thread::sleep` suffices)

### Option B: Filesystem Watcher (notify crate)

```
treehouse watch
  └─ notify::Watcher on ~/.treehouse/**/treehouse-state.json
       └─ on modify → sweep that specific pool
```

- **Pros**: Immediate reaction to state changes
- **Cons**: `notify` crate dependency; platform-specific behavior; may fire too often
- **Best for**: High-frequency agent environments (10+ concurrent agents)

### Option C: System Service (systemd / launchd / Windows Service)

```
treehouse watch --daemon
  └─ registers as OS service → auto-start on boot
  └─ same sweep loop as Option A
```

- **Pros**: Survives reboots; managed by OS
- **Cons**: Complex cross-platform service management; harder to debug
- **Best for**: Server/CI environments where agents run 24/7

### Recommendation

**Start with Option A** (timer loop). It covers 90% of the use case with minimal complexity. Option B can be added later as an optimization. Option C is a separate feature for production deployments.

---

## 5. Implementation Plan

### Phase 1: Core Watch Loop

**Files to create/modify:**

| File | Change |
|------|--------|
| `crates/treehouse-core/src/watch.rs` | **New**: `sweep_all_pools()`, `sweep_pool()`, `WatchOptions`, `WatchEvent` |
| `crates/treehouse-core/src/lib.rs` | Add `pub mod watch;` |
| `crates/treehouse/src/cli.rs` | Add `Watch` subcommand with args |
| `crates/treehouse/src/main.rs` | Handle `Watch` subcommand dispatch |
| `crates/treehouse-core/src/config.rs` | Add `WatchConfig` to `TreehouseConfig` |

### Phase 2: Discovery (Sweep All Pools)

The watcher needs to find all pools without a repo context. Currently, `gc --all` and `prune --all` already do this via `discovery.rs`.

**File**: `crates/treehouse-core/src/discovery.rs` (already exists)

```
discover_all_pools() → Vec<PathBuf>
  - scans ~/.treehouse/ for pool directories
  - each pool has treehouse-state.json
```

The watcher reuses this discovery mechanism.

### Phase 3: Event Emission (Optional)

```rust
pub enum WatchEvent {
    SweepStarted { pool_count: usize },
    PoolSwept { pool: PathBuf, reclaimed: usize, skipped: usize },
    WorktreeReclaimed { pool: PathBuf, name: String, tag: String },
    WorktreeSkipped { pool: PathBuf, name: String, reason: String },
    SweepCompleted { duration: Duration, total_reclaimed: usize },
    Error { pool: Option<PathBuf>, error: String },
}
```

Output as JSON lines on stdout for external tools to consume.

### Phase 4: Graceful Shutdown

- Register SIGTERM/SIGINT handler
- Finish current sweep before exiting
- Write "watcher stopped" event

---

## 6. Risk Assessment

| Risk | Severity | Mitigation |
|------|----------|------------|
| Watcher and CLI race on same pool | Low | Same file-lock protocol; watcher is just another CLI caller |
| Watcher auto-reclaims a tree an agent is about to use | Low | Only reclaims idle + clean + merged + expired-lease; never touches in-use or valid-lease |
| Watcher itself crashes mid-sweep | Low | Two-phase engine (reserve → re-verify → delete); half-deleted trees are safe |
| Resource consumption | Low | Sleep-based loop; process table refresh is the only expensive operation |
| Breaking "no daemon" principle | Medium | This is an opt-in command, not a required component; CLI remains self-contained |
| Config complexity | Low | Sensible defaults; watch section is optional |

---

## 7. Comparison: Current vs. Proposed

| Scenario | Current behavior | With `treehouse watch` |
|----------|-----------------|----------------------|
| Agent crashes (TTL lease) | Stale tree persists until manual `gc` | Reclaimed within `interval_secs` |
| Agent crashes (no TTL) | Permanent lease; manual intervention needed | Still requires manual return (by design — no auto-destroy of permanent leases) |
| `treehouse run` errors mid-cleanup | Worktree left dirty + leased | Watcher detects stale lease + cleans if idle + clean + merged |
| Disk fills up | `doctor` warns; user must act | Watcher can optionally trigger `gc` under disk pressure |
| 10 concurrent agents across repos | User must remember to run `gc` periodically | Watcher sweeps all pools automatically |
| CI/CD pipeline | Must add `treehouse gc --all --yes` to cron | `treehouse watch --daemon` handles it |

---

## 8. Open Questions

1. **Should the watcher be in `treehouse-core` or only in the `treehouse` CLI crate?**
   - Recommendation: Core sweep logic in `treehouse-core`, CLI dispatch in `treehouse`.

2. **Should `treehouse watch` require `--yes` for auto-reclaim?**
   - Recommendation: No — the watcher only reclaims provably-safe trees (idle + clean + merged + expired). This is the same safety bar as `gc --yes`.

3. **Should we add a `post_gc` hook for notification?**
   - Could be useful for CI/CD integration (e.g., Slack notification when trees are reclaimed).

4. **Windows service registration?**
   - Defer to Phase 3+; the timer loop works as a foreground process on Windows too.

5. **Should `treehouse run` start the watcher automatically if not running?**
   - Tempting but violates the "no hidden background processes" principle. Better to document: "Run `treehouse watch --daemon` for automatic cleanup."

---

## 9. References

- `crates/treehouse-core/src/gc.rs` — GC implementation (two-phase reclaim engine)
- `crates/treehouse-core/src/prune.rs` — Prune implementation (stale idle worktree removal)
- `crates/treehouse-core/src/run.rs` — RAII cleanup guard + TTL lease
- `crates/treehouse-core/src/lease.rs` — Durable lease system (TTL support)
- `crates/treehouse-core/src/state.rs` — State file format + heal_state
- `crates/treehouse-core/src/process.rs` — Process detection (in-use check)
- `crates/treehouse-core/src/doctor.rs` — Health diagnostics (12 checks)
- `crates/treehouse-core/src/discovery.rs` — Pool discovery (for --all sweeps)
- `crates/treehouse-core/src/config.rs` — Config loading + pool dir resolution
- `crates/treehouse/src/cli.rs` — CLI surface (subcommand definitions)
- `docs/rust-port-plan.md` — Go → Rust port plan (behavioral reference)
