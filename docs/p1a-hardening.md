# P1-A hardening design

**Status:** implemented (p1-hardening bead)
**Commits:** pool.rs release_conditional ABA fix; hardening.rs race tests

## Short-lock protocol (the audit's "long lock holds" fix)

The Go audit found git reset/termination/deletion running **under** the state
lock, stalling every other pool command on a hung git/hook. The Rust port
deliberately moves these outside:

| Operation | Inside lock (ms) | Outside lock |
|---|---|---|
| acquire | read + heal + scan + stamp reservation + write | branch resolve, fetch, `git reset`/`clean`, post_create hooks |
| release | find + validate + before_reset; then clear + write | `git reset`/`clean` |
| release_conditional | precondition check + clear + write (ONE lock, ABA-safe) | `git reset` |
| destroy/prune/gc | reserve `Destroying`+owner; re-verify + delete | pre_destroy hooks |
| status | read + heal + scan + write (ONE exclusive lock) | IsDirty (per-worktree) |

The reservation is **persisted** in the state file (owner pair /
`Destroying`+owner / lease_id), so it survives lock releases and is
re-validated via `Reservation::matches()` under a re-acquired lock. This is
the anti-TOCTOU wall for cooperative consumers: a treehouse process entering
mid-reset sees `owner_alive == true` and skips.

## ABA-safety of conditional release (exactly-once)

`release_conditional` validates the lease precondition and clears it in **one
atomic lock** (after the external reset). Two concurrent callers with the same
`--if-lease-id` both pass the initial check, but only the first clears the
lease; the second's re-validation fails. Verified by
`hardening::tests::concurrent_conditional_release_exactly_once`.

## Lock timeout

Go blocks forever on `flock`. This port uses `try_lock` + retry/backoff up to
`DEFAULT_LOCK_TIMEOUT` (10s), returning `LockError::Timeout` on a wedged
holder. Deliberate deviation, documented in `lock.rs`.

## Windows termination design

- Windows has no graceful `SIGTERM` for arbitrary processes; `TerminateProcess`
  is the cross-platform kill (abrupt). The `grace` period is effectively
  ignored on Windows (Go parity).
- Under `destroy --include-in-use`: processes are terminated with a 2s grace on
  unix; on Windows they are terminated abruptly. **Survivors always restore the
  original owner reservation and skip — never delete.**
- Prefer graceful/git-level shutdown before `TerminateProcess` where possible
  (e.g. `git worktree remove` fails first if a file is locked).

## Go+Rust mixed-pool owner-liveness

Go stores `OwnerStartedAt` (gopsutil CreateTime) in epoch **millis**; sysinfo
returns epoch **seconds**. This port stores millis (seconds × 1000, accepting
truncation). Mixed Go+Rust concurrency on one pool is **unsupported** — the
1-second resolution can cause a PID-reuse false positive. Documented in
`process.rs`.

## Verification

Race + recovery tests gated behind the `hardening` feature:

```sh
cargo test -p treehouse-core --features hardening
```

Covers: 6-way concurrent acquire (never double-issues), concurrent conditional
release (exactly-once), destroy re-acquire-mid-hook (never deletes),
crash-between-destroy-phases (self-heals), wedged-holder lock timeout.
