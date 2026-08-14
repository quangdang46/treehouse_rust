# Treehouse Rust — Port & Agent-Oriented Upgrade Plan

> **One-line goal:** *Treehouse Rust is a full, behavior-compatible Rust port of Treehouse (Go), with stronger reliability and agent-oriented output/cleanup — JSON + TOON machine-readable formats, robust stale-worktree recovery, and diagnostics — while deliberately keeping Treehouse a small reusable Git worktree pool rather than turning it into an agent orchestration system.*

**Baseline:** Go treehouse v2.1.1 (`kunchenguid/treehouse`), read from `.tmp/treehouse/`.
**Target:** Rust, Cargo workspace, `crates/*/src` layout.
**Date:** 2026-08-14.

> **Do not redesign Treehouse during the port.** First establish behavioral parity with the Go implementation, then harden, then add agent-oriented features. Every new behavior must be explicitly marked as an upgrade and must not silently alter existing Go semantics.

---

## 1. Goal & non-goals

### Goal

Port Treehouse **completely** from Go to Rust — every command, every safety invariant, every platform behavior — and then upgrade the parts that directly serve AI coding agents:

1. **`--format human|json|toon`** on every agent-facing command (plus `--json` kept as a Go-compatible alias). `toon_rust` is the TOON encoder.
2. **`treehouse gc`** — automatic, safe reclaim of stale/orphaned/dead-owner worktrees (the #1 pain: agents acquire worktrees, then crash or forget to return them, leaking disk).
3. **Stale-lease detection** — `get --lease --ttl <dur>`; expired leases are surfaced and safely reclaimable.
4. **`treehouse doctor`** — read-only health report, `--format human|json|toon`.
5. **`treehouse run -- <cmd...>`** — acquire → run agent → cleanup *always* (any exit path), never a permanent leak.
6. **Harden the audit findings** while porting: long-lock stalls, the reset-vs-new-process TOCTOU, abrupt Windows termination.

The Go implementation is the behavioral **reference**: preserve exact behavior where it is verified by the Go source and its tests (commands, flags, exit codes, stdout/stderr routing, JSON schemas, state-file wire format, safety invariants). Do not invent compatibility requirements that cannot be established from the Go baseline — anything not pinned by the Go source or tests is an implementation choice, not a contract. This is a port with hardening and agent features, **not** a Go→Rust 1:1 translation and **not** a redesign.

### Non-goals (explicitly out of scope)

MCP, scheduler, task queue, daemon, database, heartbeat server, dashboard, metrics platform, event bus, distributed mode, agent orchestration, custom checkpoint system, cache manager, multi-machine coordination. If these are ever needed, they become separate projects.

---

## 2. The contract we must preserve (P0)

These are the load-bearing invariants from the Go audit. The Rust port must reproduce each **exactly**. Full extraction with file:line citations lives in the appendix; the essentials:

### 2.1 Three distinct ownership facts (never inferred from each other)

| Fact | Persisted as | Self-heals? | Cleared by |
|---|---|---|---|
| Live process | — (scanned: cwd inside worktree) | n/a | process exit |
| Owner reservation | `owner_pid` + `owner_started_at` | Yes — `heal_state` clears when owner dead (PID-reuse safe via start-time token) | `heal_state`, `release` |
| Durable lease | `leased` + `lease_id` + `lease_holder` + `leased_at` | **Never** | only `return` / single-target `destroy --include-leased --yes` |

- `owner_alive(wt)` requires BOTH `owner_pid != 0` AND `owner_started_at != 0` AND `process_started_at(pid) == owner_started_at`.
- A lease is process-independent: survives zero running processes.
- `heal_state` clears dead owner reservations and drops vanished paths, but **never** touches lease fields.
- Leases are skipped by `get`, `prune`, and bulk `destroy --all`.

### 2.2 Safe destructive operations

- **`destroy` and `prune` share one classification** so they agree on "disposable": `disposable` / `leased` / `in-use` / `dirty` / `unmerged` / `unverified`. A target can accumulate **multiple** classes (e.g. `leased+dirty`), each requiring its own opt-in flag.
- Gating: `leased` → single named target + `--include-leased` (never bulk `--all`); `in-use` → `--include-in-use`; `{dirty, unmerged, unverified}` → `--include-unlanded`.
- **Dry-run by default.** No cross-pool/global destroy exists; `destroy --all` without a pool path is an error.
- Two-phase deletion: **phase 1** stamps `Destroying=true` + fresh owner reservation under the state lock (recording the original owner pair); **hooks** run outside all locks; **phase 3** re-locates, re-verifies `sameDestroyReservation` (Path + Destroying + OwnerPID + OwnerStartedAt) under a fresh lock, re-classifies with live state, then deletes. Any skip path **restores the original owner reservation**. A worktree re-acquired mid-hook is never deleted.

### 2.3 Atomic + recoverable state

- `treehouse-state.json` per pool; written atomically: same-dir temp file (`O_EXCL` random suffix) → fsync → `rename`+dir-sync (POSIX) / `ReplaceFileW`/`MoveFileEx` `MOVEFILE_WRITE_THROUGH` (Windows). Existing file mode preserved.
- A **missing** state file = fresh empty pool. A file that exists but fails to parse = `recover_corrupt_state`: scan on-disk dirs for `.git`, mark **every** recovered entry `leased=true`, holder `recovered: state file was corrupt or truncated; verify before reuse`, fail closed if the scan can't complete. (A 0-byte file is *corrupt*, not missing — a classic porting bug.)
- Lock: `<poolDir>/treehouse-state.lock`, `flock LOCK_EX` (unix) / `LockFileEx` (windows), file created with `MkdirAll` 0755. Every read-modify-write runs inside one `WithStateLock`.

### 2.4 Lease identity & conditional release

- `lease_id` = 128-bit random hex (32 lowercase chars), fresh **per acquisition** even for the same path/holder.
- Conditional release (`return --if-lease-id` / `--if-lease-holder`) validates + resets + clears under **one** lock → exactly-once, ABA-safe. Precondition failure → `ErrLeasePreconditionFailed`, no mutation.

### 2.5 Git edge cases

- `IsDirty`: `git status --porcelain --untracked-files=all` — untracked files count even when `status.showUntrackedFiles` is `no`.
- `ResetWorktree`: `checkout --detach --force <ref>` → `reset --hard <ref>` → `clean -fd`.
- Merge safety: `resolvePruneDefaultRef` **fetches origin first**, then `DefaultBranchMergeRef` (remote-tracking refs, not a local branch shadowing origin, not a stale `origin/HEAD`), fail-closed on unreachable origin → category-tagged *skip*, never a deletion.

---

## 3. Workspace layout

```
treehouse_rust/
├── Cargo.toml                          # workspace root
├── LICENSE                             # MIT
├── crates/
│   ├── treehouse/                      # CLI binary crate (clap; thin adapter)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs                 # fn main -> anyhow::Result<()> (CLI error boundary)
│   │       ├── cli/                    # clap subcommands: get/enter/return/status/prune/destroy/gc/init/update/doctor/run
│   │       ├── format.rs               # OutputFormat + Formatter trait (human/json/toon)
│   │       └── ui.rs                   # owo-colors branding to stderr, machine-clean stdout
│   └── treehouse-core/                 # library crate = the behavior-compatible port
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs                  # pub mods + re-export Pool, State, etc.
│           ├── error.rs                # Error (aggregate) + State/Lease/Pool/Git/Process/Hook/Config/Doctor error enums
│           ├── result.rs               # CommandResult model (one structured result per command)
│           ├── pool.rs                 # Pool orchestrator: get/enter/release/status/destroy/prune/gc/doctor/run
│           ├── state.rs                # State, WorktreeEntry, load/heal/save, rfc3339_nano serde shim
│           ├── state_file.rs           # atomic_write_file (tempfile persist + dir sync + mode preservation)
│           ├── lock.rs                 # with_state_lock (exclusive fd-lock + timeout)
│           ├── lease.rs                # Lease, LeaseId, LeaseAcquireOptions{holder, ttl}, is_expired, write_to, clear_lease
│           ├── reservation.rs          # Reservation token, reserve_owner, owner_alive, same_destroy_reservation, restore_original
│           ├── worktree.rs             # WorktreeRef, ClassSet (classification), status computation
│           ├── git/
│           │   ├── mod.rs              # GitBackend trait, GitError, GitErrorKind
│           │   └── shell.rs            # ShellGitBackend (spawns git.exe directly, no shell)
│           ├── process.rs              # ProcessTable via sysinfo (pid/ppid/cwd/start_time/kill)
│           ├── hooks.rs                # Hooks config + Runner (sh -c / cmd /d /s /c, non-fatal)
│           ├── config.rs               # treehouse.toml (repo) + ~/.config/treehouse/config.toml (user)
│           ├── recovery.rs             # recover_corrupt_state (fail-closed dir scan)
│           ├── doctor.rs               # doctor diagnostics (read-only health report)
│           └── output.rs               # status serialization → serde_json / toon (feature-gated)
├── tests/                              # integration tests with real git worktrees
└── docs/
```

Root `Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = ["crates/treehouse", "crates/treehouse-core"]

[workspace.package]
version = "3.0.0"
edition = "2024"
rust-version = "1.88"          # MSRV floor (toon requires >= 1.88, edition 2024)
license = "MIT"
authors = ["qdang46"]

[workspace.dependencies]
serde        = { version = "1.0", features = ["derive"] }
serde_json   = { version = "1.0.151" }
thiserror    = "2.0.20"
anyhow       = "1.0.104"
chrono       = { version = "0.4", features = ["serde"] }
fd-lock      = "4.0.4"
tempfile     = "3.27.0"
sysinfo      = "0.39.6"
toml         = { version = "1.1.4", features = ["serde", "parse"] }
owo-colors   = { version = "4.3.0", features = ["supports-colors"] }
rand         = { version = "0.9", features = ["getrandom"] }
humantime    = "2.1.0"
toon         = { git = "https://github.com/Dicklesworthstone/toon_rust", rev = "<pin verified at implementation time — see §3 / §5.4>" }

[target.'cfg(unix)'.dependencies]
nix = { version = "0.31.3", features = ["signal", "process"] }

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61.2", features = ["Win32_Storage_FileSystem", "Win32_System_Threading"] }
```

**Dependency rationale (researched & verified, all current as of 2026-08-14):**

| Concern | Choice | Why not the alternatives |
|---|---|---|
| CLI | **clap 4.6.6** (derive) | Standard |
| File locking | **fd-lock 4.0.4** | `fs2` unmaintained since 2018; `rustix` flock is Unix-only |
| Process table | **sysinfo 0.39.6** | `pid`/`parent`/`cwd()`/`start_time()`/`kill()` cross-platform |
| Atomic writes | **tempfile 3.27.0** `persist()` | rename on unix, `MoveFileExW`+`REPLACE` on Windows — `std::fs::rename` fails on Windows when dest exists. Add your own `sync_all()` (tempfile doesn't fsync) |
| TOML | **toml 1.1.4** | serde + parse |
| Colors | **owo-colors 4.3.0** | `colored` is MPL-2.0 |
| Errors | **thiserror 2.0.20** + **anyhow 1.0.104** (CLI only) | |
| JSON | **serde_json 1.0.151** | |
| Time | **chrono 0.4** (serde) | RFC3339Nano for `created_at`/`leased_at` |
| Lease IDs | **rand 0.9** (getrandom) | 16 random bytes → 32 hex |
| Durations | **humantime 2.1.0** | `--ttl 30m` |
| Unix signals | **nix 0.31.3** (`cfg(unix)`) | SIGTERM→SIGKILL escalation, setpgid |
| Windows API | **windows-sys 0.61.2** (`cfg(windows)`) | TerminateProcess, LockFileEx, MoveFileExW/ReplaceFileW, GenerateConsoleCtrlEvent |
| TOON | **`toon` git-only** (Dicklesworthstone/toon_rust — verify name/API/license/MSRV at impl time, §3/§5.4) | **NOT** crates.io `toon-rust` (different project, spec v1.4) or `toon-format` |

> ⚠️ **Toon dependency — verify before pinning (at implementation time, not plan time).** TOON has several separate Rust projects and package names have shifted over time. Facts verified **as of 2026-08-14** for `Dicklesworthstone/toon_rust`: git-only (not on crates.io); `[package] name = "toon"` (no `[lib] name`), so Cargo.toml is `toon = { git = ... }` and imports are `use toon::{encode, try_decode};` (the `INTEGRATION_GUIDE.md`'s `toon_rust = { git = ... }` does **not** compile); MSRV ≥ 1.88, edition 2024; license **MIT + OpenAI/Anthropic Rider** (not plain MIT). **Before coding:** re-confirm the current package name, lib name, API, license, MSRV, and a specific commit; pin an immutable `rev` (do not float on `main`); smoke-test the alias with a 10-line program. Feature-gate it in `treehouse-core` behind an optional `toon` feature so a checkout without network still compiles. Do **not** substitute crates.io `toon-rust` or `toon-format` without explicit justification — they are different projects with different spec versions.

---

## 4. Architecture

### 4.1 Core principles (from the audit)

1. **treehouse-core is a pure library.** No CLI concerns (clap, anyhow, owo-colors live only in `crates/treehouse`).
2. **One structured result per command.** Core returns `CommandResult`; the CLI formats it. Business logic never prints.
3. **Short-lock protocol.** State mutation happens under a short lock; all git/hooks/process work happens **outside** the lock. The Go audit found that holding the lock during `git reset`, process termination, and deletion stalls every other pool command; the Rust port fixes that.
4. **Reservation tokens are persisted.** A reservation isn't an in-memory mutex — it's written into the state file (`owner_pid`+`owner_started_at`, the `{Destroying, OwnerPID, OwnerStartedAt}` triple, or `lease_id`), so it survives lock releases and can be re-validated under a fresh lock. This is the Rust analog of Go's `sameDestroyReservation`.
5. **Destroy/prune/gc share one classifier.** `worktree.rs` is pure classification over `(&WorktreeEntry, Option<ProcessInfo>, Option<ClassCheckResults>)` — the same inputs, so "disposable" means the same thing everywhere.
6. **Output format is presentation-only.** Core produces one `CommandResult` and serializes it to `serde_json::Value`; the CLI turns that Value into JSON or TOON. Core never knows what TOON is — so adding a format never touches business logic: `CommandResult → serde_json::Value → { JSON, TOON }`.

### 4.2 Lifecycle state machine

```
                    ┌────────────────────────────────────────────────────────────┐
                    │   Lifecycle (all transitions protected by a Reservation)   │
                    └────────────────────────────────────────────────────────────┘

 AVAILABLE
    │ (A1) reserve — INSIDE state lock: ReadState + heal_state; pick candidate
    │      (skip Destroying/Leased/owner_alive); optionally IsDirty + merged check;
    │      reserve_owner() OR Lease::write_to(); WriteState. Reservation is now
    │      PERSISTED in the file — survives the lock release.
    ▼
 RESERVED
    │ (A2) git reset + clean -fd — OUTSIDE the lock. Entry still carries the
    │      reservation ⇒ any concurrent treehouse consumer sees owner_alive==true
    │      and skips. This is the anti-TOCTOU wall (no lock held).
    ▼
 RESETTING
    │ (A3) commit — INSIDE re-acquired lock: re-read; validate token intact;
    │      WriteState only if heal changed something.
    ▼
 READY
    │ (A4) handoff — OUTSIDE lock: post-create hooks (lease mode routes hook
    │      stdout to stderr so machine output stays clean).
    ▼
 OWNED (owner reservation) / LEASED (durable lease)      ◄── "in use" plateau.
    │                                                     heal_state clears OWNED
    │ (R1) return phase A — INSIDE lock: releasable_worktree + precondition;
    │      run before_reset callback (UNDER the lock, per Go) — caller's
    │      termination/detachment cannot race a later acquisition; WriteState.
    ▼
 RETURNING
    │ (R2) git reset + clean -fd — OUTSIDE the lock. Reservation still held ⇒
    │      no destroy deletes it, no acquire reassigns it.
    ▼
 (R3) VERIFYING — INSIDE re-acquired lock: re-read; validate token intact
      (catches a destroy --include-in-use that superseded mid-reset → abort, entry
      stays); clear_reservation + clear_lease in ONE atomic WriteState.
      │
      ▼
 AVAILABLE

  Destroy/prune/gc run in parallel and are gated by the SAME facts:
 AVAILABLE/OWNED/LEASED/RESERVED
    │ (D1) phase 1 reserve — INSIDE lock: ReadState+heal; classify; stamp
    │      Destroying=true + reserve_owner (fresh pair); save ORIGINAL owner pair
    │      into Reservation.kind.Destroy; WriteState. Concurrent destroyer sees
    │      Destroying+live owner → skip "reserved by another".
    ▼
 hooks — OUTSIDE all locks (pre_destroy; failures logged, non-fatal).
    ▼
 (D2) phase 2 verify+delete — INSIDE fresh lock: re-locate; Reservation::matches()
      (Path + Destroying + OwnerPID + OwnerStartedAt); re-classify. If entry missing /
      token broken / re-classified un-gated → SKIP and restore_original() (never delete
      a re-acquired worktree). Else: (optionally TerminateProcesses with 2s grace for
      --include-in-use) → git worktree remove --force → RemoveAll container (guarded by
      removable_worktree_container) → clear reservation → WriteState.
      │
      ▼
 AVAILABLE
```

**Locked vs external summary:**

- **INSIDE state lock (short):** ReadState, heal_state, classify, reserve stamp, same-destroy validate, clear reservation/lease, WriteState, before_reset callback, owner_alive PID check.
- **OUTSIDE any lock:** default-branch resolve, `git fetch`, `git reset`+`clean -fd`, `git worktree add/remove`, IsDirty / merged checks, hooks, process cwd-scan + kill.

**TOCTOU closure (the Go gap):** Go held the lock *during* `git reset` — correct but stalls every other command. This port instead (1) stamps the reservation **before** the external reset, so any treehouse consumer entering mid-reset observes `owner_alive==true` and skips — identical exclusion, no lock held; (2) keeps the reservation across return so destroy can't delete a mid-reset worktree; (3) re-validates the persisted token under a re-acquired lock. Residual gap (identical to Go, documented): a **bare non-treehouse** process cd'ing into the worktree during reset is unprotected — the lock and reservation only coordinate cooperative (treehouse) consumers.

**Per-operation lock budget:**

```
get:        [L] reserve (ms) … [L] commit (ms)      — reset OUTSIDE
release:    [L] precond+callback (ms) … [L] clear   — reset OUTSIDE
release_conditional: [L] check+clear (ONE lock)      — ABA-safe
destroy:    [L] reserve … hooks OUTSIDE … [L] verify+delete
prune/gc:   [L] heal+snapshot … [L] reserve … hooks OUTSIDE … [L] verify+delete
status:     [L] heal+scan+IsDirty+WriteState (ONE exclusive state lock — it mutates via heal + WriteState, so there is no read-lock; Go parity)
doctor:     no lock (probes only) or a single short exclusive state lock for state health
```

### 4.3 Key Rust types

```rust
// ---- state.rs (wire-identical to Go state.go) ----
fn is_false(v: &bool) -> bool { !*v }
fn is_zero_i32(v: &i32) -> bool { *v == 0 }
fn is_zero_i64(v: &i64) -> bool { *v == 0 }
fn is_empty_str(s: &str) -> bool { s.is_empty() }
fn is_zero_utc(t: &DateTime<Utc>) -> bool { *t == DateTime::<Utc>::UNIX_EPOCH } // omitzero analog

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct State {
    #[serde(default)]
    pub worktrees: Vec<WorktreeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeEntry {
    pub name: String,
    pub path: String,
    #[serde(with = "rfc3339_nano")]
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "is_false")]  pub destroying: bool,
    #[serde(default, skip_serializing_if = "is_zero_i32")] pub owner_pid: i32,
    #[serde(default, skip_serializing_if = "is_zero_i64")] pub owner_started_at: i64,
    #[serde(default, skip_serializing_if = "is_false")]  pub leased: bool,
    #[serde(default, skip_serializing_if = "is_empty_str")] pub lease_id: String,
    #[serde(default, skip_serializing_if = "is_empty_str")] pub lease_holder: String,
    #[serde(default, skip_serializing_if = "is_zero_utc")] pub leased_at: DateTime<Utc>,
    // P1 (additive; zero = permanent lease, so pre-P1 files load identically):
    #[serde(default, skip_serializing_if = "is_zero_utc")] pub expires_at: DateTime<Utc>,
}
// NOTE: the Go wire names use snake_case (owner_pid, lease_id...). If we serialize the
// state file, use #[serde(rename_all = "snake_case")] — NOT camelCase. The Go file is
// {"owner_pid":...}, so match it. (camelCase shown above is illustrative only.)

impl WorktreeEntry {
    pub fn is_stale_lease(&self, now: DateTime<Utc>) -> bool { self.leased && !is_zero_utc(&self.expires_at) && now >= self.expires_at }
    pub fn is_valid_lease(&self, now: DateTime<Utc>) -> bool { self.leased && (is_zero_utc(&self.expires_at) || now < self.expires_at) }
}

// ---- lease.rs ----
pub struct Lease { pub id: String, pub holder: String, pub acquired_at: DateTime<Utc>, pub expires_at: Option<DateTime<Utc>> }
impl Lease { pub fn write_to(&self, e: &mut WorktreeEntry); /* stamps the flattened Go fields */ }

// ---- reservation.rs ----
pub struct Reservation { pub worktree: String, pub kind: ReservationKind }
pub enum ReservationKind {
    Owner { owner_pid: i32, owner_started_at: i64 },
    Destroy { owner_pid: i32, owner_started_at: i64, original_owner_pid: i32, original_owner_started_at: i64 },
    Lease(Lease),
}
impl Reservation {
    pub fn matches(&self, entry: &WorktreeEntry) -> bool;      // sameDestroyReservation
    pub fn restore_original(&self, entry: &mut WorktreeEntry); // restoreOriginalOwnerReservation
    pub fn reserve_owner(&self, process: &ProcessTable) -> Result<(), ProcessError>;
}
pub fn owner_alive(entry: &WorktreeEntry, process: &ProcessTable) -> bool;

// ---- worktree.rs (pure classification) ----
pub struct ClassSet(u32); // DISPOSABLE|LEASED|IN_USE|DIRTY|UNMERGED|UNVERIFIED bitmask
impl ClassSet { pub fn missing_flags(&self, opts: &DestroyOptions) -> Vec<&'static str>; }

// ---- error.rs (thiserror) ----
pub enum Error { State(#[from] StateError), Lease(#[from] LeaseError), Pool(#[from] PoolError),
                 Git(#[from] GitError), Process(#[from] ProcessError), Hook(#[from] HookError),
                 Config(#[from] ConfigError), Doctor(#[from] DoctorError) }
// ...StateError, LeaseError (PreconditionFailed{..}), PoolError (LeasedBulk, InUse, Unlanded,
// ReAcquiredDuringHook, BeingDestroyed, NotFound, Leased), GitError (Exit{code,stderr,kind},
// NotFound, Spawn, OriginUnreachable, Unverified), ProcessError, HookError, ConfigError,
// DoctorError — each with Go-matching Display strings.

// ---- git/mod.rs ----
pub trait GitBackend: Send + Sync {
    fn repo_root(&self, start: &Path) -> Result<PathBuf, GitError>;
    fn default_branch(&self, repo: &GitRepo) -> Result<String, GitError>;        // origin/HEAD → symbolic-ref HEAD → init.defaultBranch
    fn fetch(&self, repo: &GitRepo) -> Result<(), GitError>;                     // fail with kind OriginUnreachable
    fn worktree_add(&self, repo: &GitRepo, path: &Path, base: &str) -> Result<(), GitError>;
    fn worktree_remove(&self, repo: &GitRepo, path: &Path, force: bool) -> Result<(), GitError>;
    fn is_dirty(&self, wt: &GitRepo) -> Result<bool, GitError>;                  // status --porcelain --untracked-files=all
    fn reset_worktree(&self, wt: &GitRepo, reference: &str) -> Result<(), GitError>; // checkout --detach --force → reset --hard → clean -fd
    fn is_head_merged_into_ref(&self, wt: &GitRepo, reference: &str) -> Result<bool, GitError>; // merge-base --is-ancestor; exit 1 = not merged
    fn default_branch_merge_ref(&self, repo: &GitRepo, origin: &str) -> Result<String, GitError>; // ls-remote --symref + fail-closed stale check
    fn branch_ref(&self, wt: &GitRepo) -> Result<Option<String>, GitError>;
}
pub struct GitRepo { pub common_dir: PathBuf, pub worktree: Option<PathBuf> }
// ShellGitBackend spawns git.exe DIRECTLY (no shell, no quoting — each arg its own OsString,
// MSVC CRT rules handle spaces). Discover: GIT_BIN env → PATH → Windows Program Files\Git\bin.
// A native gix backend can be swapped in later behind the same trait (NOT P0).

// ---- pool.rs (public entrypoint) ----
pub struct Pool { /* root, dir, state_path, lock_path, git: Arc<dyn GitBackend>, process: Arc<ProcessTable>, config, lock_timeout */ }
pub struct Acquired { pub name: String, pub path: PathBuf, pub branch: String, pub lease: Option<Lease> }
impl Pool {
    pub fn open(repo_root: &Path, opts: &OpenOptions) -> Result<Self, Error>;
    pub fn get(&self, opts: &AcquireOptions) -> Result<Acquired, Error>;
    pub fn release(&self, target: &str, opts: &ReleaseOptions) -> Result<(), Error>;
    pub fn release_conditional(&self, target: &str, expected: &ReleasePreconditions) -> Result<(), Error>;
    pub fn status(&self, opts: &StatusOptions) -> Result<Vec<WorktreeStatus>, Error>;
    pub fn destroy(&self, target: &DestroyTarget, opts: &DestroyOptions) -> Result<DestroyReport, Error>;
    pub fn prune(&self, opts: &PruneOptions) -> Result<PruneReport, Error>;
    pub fn gc(&self, opts: &GcOptions) -> Result<GcReport, Error>;
    pub fn doctor(&self) -> Result<DoctorReport, Error>;
}
pub struct ReleasePreconditions { pub expected_lease_id: Option<String>, pub expected_lease_holder: Option<String> } // None = omitted; Some("") = expected-empty
```

> ⚠️ **State-file snake_case vs camelCase:** the Go state file is `{"owner_pid":..., "lease_id":...}` — **snake_case**. The design agent's sketch shows camelCase; that's wrong for the *persisted state file*. The **JSON output** schemas (Section 5) use snake_case correctly. Both the state file and CLI JSON output must be snake_case, matching the Go baseline (field names and presence; byte-level encoding beyond that is not a contract unless verified by a Go test).

### 4.4 Cross-platform notes

- **Paths:** always `std::path::Path`/`PathBuf`; never hardcode separators. Worktree names differing only in case collide on Windows.
- **No shell for git:** spawn `git.exe` directly via `std::process::Command`; pass each arg as its own arg. COMSPEC only for interactive subshells and hooks.
- **Signals:** absent on Windows. Ctrl-C → `CTRL_C_EVENT`; kill → `TerminateProcess` (`sysinfo` `kill()`). Graceful-then-abrupt only on unix (SIGTERM→SIGKILL); Windows is abrupt by necessity.
- **Atomic replace:** `std::fs::rename` fails on Windows when dest exists → use `tempfile::persist()` (`MoveFileExW` + `MOVEFILE_REPLACE_EXISTING`).
- **Locks:** advisory only (cooperative processes); Windows locks not inherited by children; explicitly drop/close.
- **fd-lock vs windows-sys versions:** fd-lock 4.0.4 pins `windows-sys >=0.52,<0.60`, so if we also use `windows-sys 0.61.2` directly, cargo builds **two copies** (0.59 + 0.61) — harmless, document it so no one "fixes" it.

---

## 5. Output layer (`--format human|json|toon`)

### 5.1 Design

- **treehouse-core returns `CommandResult`** (one structured result per command, serde-derived, snake_case, Go wire names). The CLI never formats errors — `main` prints the Display string once to stderr and exits 1.
- **Formatter trait** with `Human`/`Json`/`Toon` impls; writers injected (stdout + stderr) so golden tests capture both streams.
- **stdout/stderr discipline (preserved from Go):** machine data (bare path, JSON, TOON) → **stdout only**; 🌳 banners, warnings, prompts → **stderr in every format**. Errors → stderr once, exit 1.
- **Abstraction boundary:** core produces `CommandResult` → serializes to `serde_json::Value`; the CLI formats that Value as JSON or TOON. Core never needs to know what TOON is.

```rust
pub trait Formatter: Send + Sync {
    fn render(&self, result: &CommandResult, out: &mut dyn Write, err: &mut dyn Write) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat { Human, Json, Toon }   // FromStr: "human"|"json"|"toon"; unknown → clap error, exit 1

pub fn formatter(f: OutputFormat) -> Box<dyn Formatter>;
```

### 5.2 `--format` flag semantics

- `--format human|json|toon` is a **global** clap arg (`global = true`, default `Human`). Works before or after the subcommand.
- `--json` remains a per-command bool alias **exactly where Go has it**: `get` and `status` only. Resolution: `--json` → Json; else `--format` value; else Human.
- Conflict: both `--json` and `--format <v≠json>` → error `conflicting output formats: --json and --format <v>` (exit 1). `--json --format json` is fine.
- **Commands that accept `--format`:** `get`, `return`, `status`, `prune`, `destroy`, `gc`, `doctor`, `run` (return/prune/destroy/gc/doctor/run gain it as a NEW Rust capability — Go has no JSON for them).
- **Reject `--format`:** `init`, `update`, `enter`, `--version`.
- `get --json` without `--lease` → `--json requires --lease` (exit 1, byte-exact). `get --format json` without `--lease` → `--format json requires --lease` (distinct string so Go-substring-matching scripts still pass).

### 5.3 Result model (stable JSON schemas)

| Command | Schema (stdout, compact, trailing newline) |
|---|---|
| `get --lease --json` | `{"path":"...","lease_id":"<32hex>","lease_holder":"","leased_at":"<RFC3339Nano>"}` — all 4 keys always present; `leased_at` non-null. P1 adds 5th key `expires_at` (null when permanent). |
| `status --json` | Array; **all keys always present**: `lease_id`/`lease_holder` = `""` when not leased; `leased_at` = `null` when not leased; `processes` = `[]` when none. Empty pool → exactly `[]\n`. P1 adds per-element `expires_at` (null) + `stale` (bool). |
| `return --json` (new) | `{"path","returned","aborted","terminated":[{"pid","name"}],"warnings":[]}` |
| `prune --json` (new) | `{"dry_run","orphans_included","global","pool_count","candidates","pruned","skipped","reclaimable_bytes","freed_bytes","pool_root"?,"pools"?}` |
| `destroy --json` (new) | `{"dry_run","all","scope","planned","destroyed","skipped","planned_bytes","freed_bytes"}`; single-target exit-1 `did not destroy ...` is a `CommandError`, not a result |
| `gc --json` (new) | `{"dry_run","global","pool_count","candidates","reclaimed","skipped","reclaimable_bytes","freed_bytes",...}` |
| `doctor --json` (new) | `{"healthy","strict_healthy","checks":[{"name","status","detail","count","bytes","affected"}]}` |

`EnterResult` and interactive `get` have **no payload** (`payload() -> None`) — a stray stdout write would corrupt the subshell/terminal.

### 5.4 TOON dependency & integration (`Dicklesworthstone/toon_rust`)

> **Verify before pinning — see the dependency note in §3.** The API surface below was confirmed as of 2026-08-14; re-check the package name, API, license, MSRV, and a specific commit before wiring it in, and pin an immutable `rev`.

```rust
use toon::encode;                       // pub fn encode(input: impl Into<JsonValue>, Option<EncodeOptions>) -> String
use serde_json::Value;                  // impl Into<JsonValue> for serde_json::Value exists

// CommandResult::payload() returns serde_json::Value; encode directly:
let s = encode(r.payload().unwrap(), None);   // None → defaults (indent 2, delimiter ',', key_folding Off)
writeln!(out, "{s}")?;                  // toon::encode emits NO trailing newline — append it
```

- **encode-only, one-shot.** Do not use `decode`/`decode_from_lines` (they **panic** on error); `try_decode` only in tests to assert json/toon parity.
- **Quirks that shape output:**
  1. `encode` has no trailing newline → always `writeln!`.
  2. Top-level array of uniform all-primitive objects → tabular form `[N]{k1,k2,...}:` + indented rows.
  3. **`status --toon` is NEVER tabular**: the always-present `processes` array (non-primitive) forces the dashed-list form. Expect the ~40–60% token savings only for flat payloads like `get --lease`.
  4. Empty top-level array → `[0]:` (TOON has no `[]` literal). Empty-pool status TOON = `[0]:`.
  5. Quoting is rule-based (`isSafeUnquoted`): values with `,`/`"`/newline and RFC3339Nano timestamps (contain `:`) get quoted; empty → `""`; null → `null`. Always round-trip through `encode`; never hand-format rows.

### 5.5 Example: `status` in all three formats

**Human** (stdout table; colors auto-off off-TTY):
```
1     available    /home/user/proj/.treehouse/acme-3f2a1b/1/acme
2     in-use       /home/user/proj/.treehouse/acme-3f2a1b/2/acme
                   82144 (opencode)
3     dirty        /home/user/proj/.treehouse/acme-3f2a1b/3/acme
4     leased       /home/user/proj/.treehouse/acme-3f2a1b/4/acme  (held by automation-A)
```
Format `%-4s  %-11s  %s`; leased rows append `  (held by <holder>)`; process rows indented 19 spaces. Empty pool → `🌳 No worktrees in pool.` on **stderr**, exit 0.

**JSON** (stdout, compact, trailing newline; all keys always present):
```json
[{"name":"1","path":"/home/user/proj/.treehouse/acme-3f2a1b/1/acme","status":"available","lease_id":"","lease_holder":"","leased_at":null,"processes":[]},{"name":"2","path":"/home/user/proj/.treehouse/acme-3f2a1b/2/acme","status":"in-use","lease_id":"","lease_holder":"","leased_at":null,"processes":[{"pid":82144,"name":"opencode"}]},{"name":"3","path":"/home/user/proj/.treehouse/acme-3f2a1b/3/acme","status":"dirty","lease_id":"","lease_holder":"","leased_at":null,"processes":[]},{"name":"4","path":"/home/user/proj/.treehouse/acme-3f2a1b/4/acme","status":"leased","lease_id":"9f2c1e04a7b3d5c8e6f10293a4b5c6d7","lease_holder":"automation-A","leased_at":"2026-08-14T12:34:56.123456789-07:00","processes":[]}]
```

**TOON** (stdout + trailing newline; dashed list because `processes` is non-primitive):
```
[4]:
  - name: 1
    path: /home/user/proj/.treehouse/acme-3f2a1b/1/acme
    status: available
    lease_id: ""
    lease_holder: ""
    leased_at: null
    processes: []
  - name: 2
    path: /home/user/proj/.treehouse/acme-3f2a1b/2/acme
    status: in-use
    lease_id: ""
    lease_holder: ""
    leased_at: null
    processes[1]:
      - pid: 82144
        name: opencode
  - name: 4
    path: /home/user/proj/.treehouse/acme-3f2a1b/4/acme
    status: leased
    lease_id: 9f2c1e04a7b3d5c8e6f10293a4b5c6d7
    lease_holder: automation-A
    leased_at: "2026-08-14T12:34:56.123456789-07:00"
    processes: []
```
Empty pool → `[0]:`.

---

## 6. P1 features (the cleanup & agent upgrades)

### 6.1 Stale leases (`expires_at` + `--ttl`)

- Add `expires_at: DateTime<Utc>` to `WorktreeEntry` (omitempty/omitzero; **zero = permanent lease**, so pre-P1 state files load identically). `is_stale_lease(now)` = leased ∧ expired.
- `get --lease --ttl <duration>` (humantime syntax). `--ttl requires --lease`; `--ttl` must be > 0. `TREEHOUSE_LEASE_TTL` env fallback for the holder.
- **A valid lease is NEVER auto-reclaimed** — not by `get`, `prune`, `gc`, or bulk `destroy --all`. Only `return <path>`, single-target `destroy <path> --include-leased --yes`, or `gc`'s safe-reclaim of an **expired** lease that also passes the full disposable bar.
- `heal_state` never clears any lease (valid or stale) — stale leases are *surfaced*, not silently un-leased.
- `status` shows `leased (expired)`; `status --json` adds `expires_at` + `stale` (additive).
- **Second safety layer:** even a live agent running past its TTL is protected — its live process makes the worktree `in use`, so `gc` skips it. TTL only unblocks *idle* worktrees.

### 6.2 `treehouse gc`

Flags: `--yes`, `--all`/`--global`, `--prune-orphans`, `--verbose`/`-v`. No positional args. **Dry-run by default.** Output → stdout (🌳 summaries mirror prune's exact wording); warnings → stderr. Exit 0 on every successful run (incl. 0 candidates / all skipped).

What it cleans (single lock + two-phase re-verify per phase):
1. **Heal** dead owner reservations (`heal_state`) and drop vanished paths.
2. **Candidate selection:** (a) stale-lease worktrees that pass the disposable bar (expired + idle + clean + merged into fail-closed default ref + backing repo present) → `[stale lease]`; (b) ordinary idle spares → `[stale]` (exactly prune's analysis); (c) backing-repo-missing orphans → skipped unless `--prune-orphans` → `[stale/orphaned]`; (d) **valid leases → NEVER candidates** (distinct skip category `valid lease`).
3. **Dry run:** classify + measure + sort + report.
4. **Execute (`--yes`):** reuse the shared two-phase destroy (reserve → hooks outside lock → re-verify `sameDestroyReservation` → remove). Any skip restores the original owner reservation; a worktree re-acquired mid-hook is never deleted.

`gc` has **no `--include-*` flags** — it is a strictly-safe reclaim; unsafe targets are skips, never deletions.

Message strings (mirroring prune so scripts match): `🌳 No stale worktrees to reclaim.`, `🌳 Dry run: would reclaim <n> worktree(s) and free <bytes>.`, `🌳 Re-run with --yes to reclaim these worktrees.`, `🌳 Reclaimed <n> worktree(s) and freed <bytes>.`, `🌳 Skipped <n> worktree(s):`, orphan hint `🌳 Re-run with --prune-orphans ...`.

### 6.3 Crash recovery (status / doctor detect; gc reclaims)

| Post-crash state | Detection | Action |
|---|---|---|
| Dead owner reservation | `owner_alive(wt)` false (pid gone OR PID recycled — start-time token) | `heal_state` clears it + drops vanished paths (self-healing, same as Go); worktree dir untouched |
| Expired lease | `leased && expired` | **Never** auto-cleared; surfaced as `leased (expired)` / `stale_leases`; reclaimed only by `gc` (full disposable bar) or explicit `return --force` / `destroy --include-leased` |
| Corrupt/truncated state | state file exists but unparsable | `recover_corrupt_state`: scan on-disk dirs for `.git`, mark **every** recovered entry `leased=true`, holder `recovered: state file was corrupt or truncated; verify before reuse`, **no expires_at** (so gc can never auto-reclaim); fail closed if scan fails; loud stderr warning. Missing file = fresh pool |

### 6.4 `treehouse doctor`

Read-only health report (never mutates state). Flags: `--format human|json|toon`, `--strict` (any warn also fails). **12 checks:** `git_binary`, `config`, `state`, `state_writable`, `lock`, `dead_owners`, `stale_leases` (with a reclaimable/bytes breakdown), `active_leases`, `orphans`, `dirty`, `in_use`, `disk` (< 10% free → warn).

- Output model (json): `{"healthy":true,"strict_healthy":false,"checks":[{"name":"git_binary","status":"ok","detail":"git 2.47.0","count":0,"bytes":null,"affected":[]},...]}` on **stdout**.
- **Exit code:** 0 when no Error-severity check; 1 when any Error (git missing, config unparsable, state corrupt, state not writable, lock not acquirable). Warns (dead owners, stale leases, orphans, dirty, in-use, disk-low) do **not** fail by default — an agent leaving a dirty worktree shouldn't fail a health gate — but `--strict` promotes them.
- Human: `🌳 treehouse doctor` + one line per check (`  ✓ git binary: git 2.47.0` / `  ⚠ stale leases: 2 (1 reclaimable, 12.5 MiB)` / `  ✗ state: corrupt or truncated ...`) + summary `Doctor: <e> error(s), <w> warning(s)`.

### 6.5 `treehouse run -- <cmd...>`

Acquire → spawn agent → **cleanup ALWAYS** → exit = child's code.

- **Acquire:** internal lease (`get --lease` semantics), holder `run:<pid>` (or `--lease-holder`/env), TTL = `--ttl` > config `lease_ttl` > 24h default. **The lease is load-bearing:** process-independent + TTL-bounded, so even a SIGKILLed treehouse leaves a self-expiring reservation rather than an eternal one. Child env gets `TREEHOUSE_DIR` + `TREEHOUSE_LEASE_ID` (so the child can `return --if-lease-id`); cwd = worktree; spawned directly (no shell) in a **new process group** (setpgid unix / `CREATE_NEW_PROCESS_GROUP` Windows).
- **Cleanup-always (RAII `Drop`):** on EVERY exit path — normal exit, nonzero exit, signal, treehouse-side error, even unwinding panic — detach HEAD, reset to default ref, terminate lingering processes (2s grace), release the lease. **A nonzero child exit is not a reason to leak.**
- **Signal forwarding:** unix — forward SIGINT/SIGTERM/SIGHUP to the child's process group, wait, cleanup, exit `128+signum`. Windows — `GenerateConsoleCtrlEvent` / graceful `TerminateProcess` fallback.
- **Exit code** = child's exit code (0-255; unix signal → 128+signum). `treehouse run -- false` → 1; `-- true` → 0.
- **If treehouse itself is SIGKILLed** (uncatchable, no Drop): the TTL lease is still valid in state → `status` shows `leased (held by run:<pid>)` → after expiry it's **stale** and a later `gc --all --yes` (or cron) reclaims it *iff* idle+clean+merged; if the orphaned child is still running, gc skips it as `in use` and status/doctor surface it until the child exits or a human runs `destroy --include-in-use --yes`. **The guarantee: a lease can never block a pool slot longer than (TTL + next gc sweep), and a live agent is never evicted.**

---

## 7. Phased roadmap — strict order, each phase gates the next

Implement **in this order**; a phase is done when its Definition-of-Done tier passes before starting the next. `run` comes last because it leans on the primitives stabilized by P0→P1-C.

| Phase | Goal | Deliverables |
|---|---|---|
| **P0** | Go→Rust **behavior parity**, no intentional regression | Workspace + crates; full CLI (init, get, enter, return, status, prune, destroy, update, `--version`, `--update-check`); pool/state/lease/owner/process/git/hooks/config; atomic+recoverable state; human output parity; Windows/macOS/Linux; Go tests → Rust tests. **Gate:** parity tests green |
| **P1-A** | Hardening (fix audit findings) | Short-lock protocol (no git/hooks/process under lock); persisted reservation-token revalidation; lock timeout; Windows termination design; go-rust mixed-pool owner-liveness note; recovery + concurrency/race tests |
| **P1-B** | Agent output | `--format json` + `--format toon` (via the verified `toon` dep, §3/§5.4); new JSON/TOON schemas for return/prune/destroy/doctor/gc/run; golden tests for JSON byte-stability + json↔toon parity |
| **P1-C** | Cleanup & diagnostics | `expires_at` + `--ttl`; `treehouse gc`; `treehouse doctor`; stale/orphan/dead-owner crash-recovery surfacing |
| **P1-D** | Process supervision | `treehouse run -- <cmd>` — built on the now-stable acquire/lease/release + gc primitives; cleanup-always + signal forwarding + TTL backstop |
| **P2** (deferred) | Optional | native `gix` GitBackend; MCP; scheduler/capacity; anything from non-goals |

## 8. Definition of done (3 tiers — each gates the next)

### P0 — PARITY (do this first; TOON / GC / `run` are NOT conditions for proving the port correct)
- [ ] Full Treehouse Go functionality ported (init, get, enter, return, status, prune, destroy, update, `--version`, `--update-check`)
- [ ] Existing safety invariants preserved (three ownership facts, destroy/prune classification agreement, two-phase reservation, atomic+recoverable state, lease ABA-safety, `--untracked-files=all`, fetch-first merge safety, stdout/stderr discipline, `--include-*` gating)
- [ ] State compatibility (snake_case wire format, omitempty/omitzero semantics, corrupt-file recovery, empty-file-is-corrupt)
- [ ] CLI behavior parity (exit codes incl. single-target destroy skip → 1, stdout/stderr routing, human message strings verified by the Go source/tests)
- [ ] Git / process / hooks / config / cross-platform (Linux/macOS/Windows)
- [ ] Integration tests with real git worktrees; Go tests converted to Rust tests

### P1 — HARDENING
- [ ] Short-lock protocol (no git/hooks/process under the lock)
- [ ] Persisted reservation-token revalidation
- [ ] Process cleanup + Windows termination improvement
- [ ] Recovery + concurrency/race tests

### P1 — AGENT UX
- [ ] `--format human | json | toon` on get/status/return/prune/destroy/doctor/gc/run; `--json` alias on get/status
- [ ] JSON and TOON outputs tested for equivalent data (round-trip via `toon::try_decode`)
- [ ] TTL leases (`expires_at` + `--ttl`)
- [ ] `treehouse gc` (dry-run default)
- [ ] `treehouse doctor` (human/json/toon)
- [ ] `treehouse run -- <cmd>` (cleanup-always)
- [ ] Stale/orphan/dead-owner recovery surfacing
- [ ] No unnecessary orchestration: no daemon/database/distributed scheduler/MCP

---

## Appendix A. Risks & mitigations

| # | Risk | Mitigation |
|---|---|---|
| 1 | **Toon git-only fragility:** pre-1.0, git-only, package/lib naming inconsistent, force-push can break the dep | Feature-gate (`toon` feature, `optional = true`); **verify package name/API/license/MSRV at implementation time** and pin an immutable `rev`; smoke-test the alias (`use toon::encode`) first. Do not substitute another TOON crate without explicit justification. License MIT+Rider — confirm acceptance before shipping |
| 2 | **Go↔Rust owner-liveness precision:** Go stores `OwnerStartedAt` as gopsutil CreateTime in epoch **millis**; sysinfo `start_time()` is epoch **seconds** | Store millis (accept truncation); document that mixed Go+Rust concurrency on one pool is unsupported; true fix later = ms-precision per-platform |
| 3 | **Inherent reset race with non-cooperative processes** (same as Go) | Reservation + lock only coordinate treehouse consumers; surface the residual risk in `doctor` + docs |
| 4 | **1-second start-time resolution PID-reuse false-positive** (single Rust deployment) | Mitigation: ms storage; PID reuse rare in practice |
| 5 | **status holds the exclusive state lock while running git IsDirty** → a hung git stalls other pool commands | Accepted for Go parity (Go holds the same lock); a later snapshotting refactor could drop the lock entirely |
| 6 | **Two windows-sys copies** (fd-lock pins 0.59, we use 0.61) | Harmless; document so nobody "fixes" it |
| 7 | **TerminateProcess is abrupt** and can't kill higher-privilege processes | Grace path on unix; on Windows prefer graceful/git-level shutdown before TerminateProcess; survivors → restore + skip, never delete |
| 8 | **State-file schema drift** (adding `expires_at` etc.) | Additive-only, behind new optional keys; never change existing field types/precision |
| 9 | **Crash between destroy phase 1 and phase 2** (Destroying=true, owner dead) | `heal_state` clears Destroying when the owner is dead → self-heals on next command; verify in recovery tests |
| 10 | **Lock timeout (new)** — Go blocks forever | `try_lock` + retry/backoff + `lock_timeout`; document the default so parity tests don't assume infinite blocking |
| 11 | **Empty vs missing state file** | A 0-byte file is *corrupt* → recover (lease-everything); only a missing file is fresh. Common porting bug — test it |
| 12 | **TTL vs live-but-long agent** | In-use process check in gc prevents eviction of a live agent; TTL only unblocks idle worktrees |
| 13 | **Windows RemoveAll on in-use files can fail** after `git worktree remove --force` | `remove failed` skip; stale lease persists until process exits or human force-recovers — no data loss |
| 14 | **Contract extension of `get --lease --json` / `status --json`** (new fields) | Additive; re-validate exact-field-set consumers |
| 15 | **`run` cleanup best-effort on panic=abort/SIGKILL** | TTL lease is the backstop; orphaned child keeps disk until child exits or `destroy --include-in-use` — a process lifetime, not a state leak |

## Appendix B. CLI contract quick-reference (from the Go extraction)

- **Exit codes:** 0 = success; 1 = any error. Special cases: `return`/`get` dirty-prompt declined → exit 0; `destroy <path>` single-target with 0 destroyed + a skip → exit **1** with `did not destroy <name> (<class>); re-run with <flags>` (LeasedBulk skips excepted); bulk `destroy --all` skips → exit 0; prune 0 candidates/0 pruned → exit 0.
- **stdout vs stderr:** machine data → stdout; 🌳 banners → stderr. `get --lease` stdout = bare path (one line); `enter --print-path` stdout = bare path; `status` human table → stdout; prune/destroy summaries → stdout; `init` → stderr; `update` → stdout; errors → stderr.
- **JSON contracts:** `get --lease --json` = one object, 4 keys always present, `leased_at` non-null. `status --json` = array, empty exactly `[]\n`, all keys always present (`lease_id`/`lease_holder` = `""`, `leased_at` = `null`, `processes` = `[]`).
- **Env vars:** `TREEHOUSE_NO_UPDATE_CHECK` (== "1"), `TREEHOUSE_DIR` (subshell + return default), `TREEHOUSE_LEASE_HOLDER`, `SHELL`/`COMSPEC`, `HOME`. **Hidden arg:** `--update-check <version>` intercepted in main before clap; silent, writes `~/.treehouse/update-check.json` (24h TTL).
- **Key message strings scripts match** (byte-exact): `--json requires --lease`, `🌳 Leased worktree at <p>. Run 'treehouse return <p>' to release it.`, `🌳 Entered worktree at <p>. Type 'exit' to return.`, `🌳 Worktree returned to pool.`, `🌳 Aborted.`, `🌳 No worktrees in pool.`, `🌳 Dry run: would prune <n> stale worktree(s) and reclaim <b>.`, `orphaned (backing repository missing)`, `origin unreachable (cannot verify)`, `content could not be verified`, `🌳 Dry run: would destroy <n> worktree(s) in <s> and reclaim <b>.`, `--include-leased cannot be combined with --all`, `lease precondition failed: ...`, `worktree <p> is not managed by treehouse`, `treehouse.toml already exists`, `no worktree named "<n>": the pool is empty. Run 'treehouse get' to create one`.
- **formatBytes:** `N B` / `N.N KiB|MiB|GiB|TiB`, one decimal, trailing zeros trimmed.
- **Pool dir layout:** `<root>/.treehouse/<repoName>-<6-hex-sha256-of-remoteURL-or-abs-path>/<N>/<repoName>/`; state `treehouse-state.json`; lock `treehouse-state.lock`.
