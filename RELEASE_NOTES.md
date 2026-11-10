# treehouse 0.2.3

A patch release. No new commands, no changed flags, no changed output formats,
no changed exit codes, no changed state-file format. Existing pools keep
working; nothing needs `treehouse init` again.

It fixes a batch of defects found by code review and end-to-end testing —
signal handling in `treehouse run`, a git spawn failure that was reported as
success, a permanently bricked pool after an externally deleted worktree, a
state lock held across slow operations, and a correctness/cleanup sweep.

---

## `treehouse run` now survives signals

`run` had no signal handling at all. A SIGINT or SIGTERM killed the process
instantly: `Child::wait` never returned, the cleanup guard never ran, the
child — which runs in its **own process group**, so a terminal Ctrl-C hits
`treehouse` only — kept running orphaned inside the worktree, and its durable
lease stayed held for the whole TTL. `gc` and `prune` skip leased slots, so
the slot leaked until someone noticed.

`run` now installs a forwarding handler before spawning (a `sigaction` on
Unix, `SetConsoleCtrlHandler` + `GenerateConsoleCtrlEvent(CTRL_BREAK)` on
Windows). The first signal is forwarded to the child's process group so
`wait` returns and cleanup runs normally; a repeat signal escalates to
SIGKILL so a child that traps the first one cannot hang `run`. Once the
child is reaped the handler restores the default disposition and re-raises,
so later signals — including a second Ctrl-C during cleanup — kill normally.

A cleanup failure is also no longer silent: `run` prints a stderr warning and
exits nonzero even when the child itself succeeded.

Verified end to end: `treehouse run -- sh -c 'sleep 60'` + SIGTERM exits 143,
the child's whole process group is dead, the slot goes back to `available`,
and no `sleep` is left behind.

---

## A missing `git` binary is now an error, not a silent success

When the `git` executable failed to *spawn* — not found on `PATH`, not
executable, out of memory — the shell backend synthesized a successful
`ExitStatus` with empty output. Every consumer read that as a real answer:
`is_dirty` returned `false`, `is_head_merged_into_ref` returned `true`, and
`gc`/`prune`/`destroy` would classify an **unverified** worktree as clean and
merged — then `remove_dir_all` deleted it. `return` and `reset` reported
success while nothing had run.

Spawn failures now propagate as `GitError` through every git call. Destructive
paths fail closed: if treehouse cannot verify a worktree's state, it does not
delete it. Boolean probes (`local_branch_exists`, `ref_exists`, `is_ancestor`,
`has_remote`, `remote_url`) degrade to `false`/`None` so the caller's own git
invocation surfaces the missing binary with a real error.

---

## A worktree deleted outside git no longer bricks the pool

Delete a pooled worktree's directory with `rm -rf` (or let a cleanup tool do
it) and git still lists the path as a registered worktree. The next
`treehouse get --lease` reuses that slot name, `git worktree add` refuses with
`missing but already registered worktree`, `heal_state` drops the pool entry,
and **every subsequent `get` fails the same way** — permanently, until someone
runs `git worktree prune` by hand. `gc --yes` could not help: the state entry
was already gone.

`worktree_add` now runs `git worktree prune` first — which only drops
registrations whose directories are actually gone; locked worktrees are never
pruned — and retries the add once on failure. Verified end to end: `get
--lease`, `rm -rf` the slot, `get --lease` succeeds again.

---

## The state lock no longer spans slow operations

The "short-lock" protocol was being violated: the exclusive state lock was
held across per-slot process-table scans, `is_dirty` git calls, `git worktree
add`/`remove` (seconds on a large repository), `terminate_with_grace`, and
`remove_dir_all`. Two agents touching the same pool got spurious
`Lock(Timeout)` errors exactly when the pool was busiest.

Acquisition, `status`, `destroy`, `prune`, and `gc` now classify outside the
lock, stamp a reservation inside it, and re-validate on commit — the same
shape the reset path already used. Git subprocesses and filesystem mutations
run outside the critical section.

---

## Correctness batch

- **Fresh slots are no longer reset after checkout.** `get` ran
  `read-tree --reset -u` + `clean -fd` even for a brand-new slot, deleting
  files that post-checkout hooks or smudge filters had just created. Fresh
  slots skip the reset; only reused slots are reset.
- **Hook output can no longer deadlock a command.** A lifecycle hook writing
  more than ~64 KiB to a piped stdout/stderr blocked in `wait` forever — no
  timeout, no drain — hanging `get`/`destroy`/`prune`. Hook pipes are now
  drained concurrently with a timeout.
- **Path comparison is canonicalized.** `find_by_path` compared paths
  lexically against git-canonicalized stored paths, so a symlinked prefix
  (`/var` vs `/private/var` on macOS) made a managed worktree look unmanaged.
  Both sides are canonicalized when possible.
- **`gc` actually runs `pre_destroy` hooks.** The code claimed it did;
  `execute_gc` never wired `GcOptions.pre_destroy`. It does now, matching
  `prune` and `destroy`.
- **`doctor` tells the truth.** `disk_free`/`disk_total` returned `0` on Unix,
  so the disk check always reported "100% free"; `inspect_worktrees` ran
  `git fetch` — a network *write* — inside a "READ-ONLY" diagnostic; and the
  dirty probe bypassed the slot's backend, reporting the enclosing
  repository's dirtiness. Disk probes are implemented, the fetch is gone, and
  probes route through the worktree's own backend.
- **Reservation is re-checked before `get` returns.** A concurrent
  `destroy --include-in-use` could delete the slot between reservation and
  commit; `get` re-verifies the reservation (owner pid / lease) inside the
  final critical section.
- **Leasing a damaged slot is refused.** `lease_existing` could hand out a
  valid lease on a markerless/damaged slot, which then sat behind the lease
  where `gc`/`prune`/`destroy` all skip it — bricked behind a valid lease.

## Cleanup batch

- `treehouse run` with an empty command list panicked indexing
  `command[0]`; it is now a proper error.
- Branch names are validated with `git check-ref-format --branch` — git's own
  authoritative check — instead of a partial reimplementation that missed
  cases like a lone `@` or per-component `.`-prefixes.
- `status()` no longer rewrites the state file on every call when healing
  changed nothing.
- `watch`'s SIGINT now interrupts an in-flight sweep instead of waiting for
  it to finish.
- `--format toon` on a build without the `toon` feature still emits JSON, but
  now says so on stderr instead of silently downgrading.
- `TREEHOUSE_UPDATE_TARGET` can no longer redirect a self-update replacement
  to an arbitrary path from a stray environment variable.
- `doctor` detects a leftover `HEAD.lock` from a crashed git process and
  reports that ref updates in that worktree will fail until it is removed.
- A `git worktree add` that succeeded but whose state write failed no longer
  orphans the directory silently — the orphan is reclaimed.

---

## Still open

Carried over unchanged from 0.2.2 — naming these because a reader skimming the
source would otherwise assume more than has been proven.

- **The jj backend is implemented, registered, and not exercised end to end.**
  See the 0.2.2 notes; nothing in this release changed it.
- **VCS selection reads `TREEHOUSE_VCS` only.** The `vcs` key in
  `treehouse.toml` and `~/.config/treehouse/config.toml` is not consulted.
- **A missing `jj` binary is not reported as a configuration problem.**
- **Windows ARM64 has no prebuilt asset.** The release matrix builds
  `windows-x86_64` only; on Windows ARM64, `install.ps1` falls back to
  building from source.
- **`treehouse run` cleanup still only kills processes whose cwd is inside
  the worktree.** A child that `chdir`s out, or double-forks, can escape the
  sweep; a SIGKILL/crash leaves a dirty worktree behind an expired lease with
  no blessed disposable path yet.

---

## Upgrading

```bash
treehouse update
# or
curl -fsSL https://raw.githubusercontent.com/quangdang46/treehouse_rust/main/install.sh | bash
```

Nothing in your scripts needs changing: no flag, output format, state-file
field, or documented exit code changed. Existing pools keep working.

Full technical detail is in [CHANGELOG.md](CHANGELOG.md).
