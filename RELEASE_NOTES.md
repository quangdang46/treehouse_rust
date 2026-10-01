# treehouse 0.2.0

Worktrees for parallel coding agents, ported from Go. This release closes 27
audited gaps against upstream v3.1.0, four of which could destroy uncommitted
work, and adds the VCS seam, four commands, and structured output for agents.

This is a `0.x` minor, so it carries breaking changes. Two of them will bite
scripts.

---

## Read this first: three changes that affect how you run treehouse

### 1. Exit code 3 is now the default

When `treehouse return` leaves a dirty worktree unreturned — because you
declined the clean, or because the confirmation could not be answered — it now
**exits 3** instead of 0. The slot stays held either way; only the status
changes.

0.1.2 exited 0 here, which meant a script doing this:

```bash
treehouse return "$p" && deploy
```

would run `deploy` against a worktree that still has your uncommitted changes in
it. Exit 3 is what Go shipped in v3.0.0 for exactly that reason, and this port
had been holding exit 0 behind `TREEHOUSE_EXIT_STRICT=1` while the contract was
still settling. That wait is over.

**If you have `set -e`, or any `&&` chain keyed on treehouse's exit status,
this changes your control flow today.** A script that used to continue will now
stop. That is the intended fix, but it is still a stop.

To get the old behavior back for one minor release:

```bash
export TREEHOUSE_EXIT_STRICT=0
```

Note the **polarity flipped**. It used to be opt-in (`=1` enabled the new
behavior); it is now opt-out (only a false value restores exit 0). If you set
`TREEHOUSE_EXIT_STRICT=1` back in 0.1.x, leave it — you get exit 3 either way.
Only `0`, `f`, `false`, `off`, `n`, or `no` restore exit 0. Anything else,
including a typo, keeps the strict default. We would rather a fat-fingered
variable name leave a slot reported as held than silently unreleased.

### 2. `--env-path` changed meaning

`--env-path <DIR>` used to be **accepted and silently ignored** — the pool went
to `~/.treehouse` regardless of what you passed. It is now an alias of the
global `--root` flag, so it resolves to `<DIR>/.treehouse`:

```bash
treehouse --env-path ~/scratch get      # pool at ~/scratch/.treehouse
treehouse --root . get                  # pool at <repo>/.treehouse
```

Resolution order is `--root` > `TREEHOUSE_ROOT` > `treehouse.toml` `root` >
`~/.treehouse`, matching Go. A relative path resolves from the repository root,
so `--root .` gives you an in-project pool.

If you were passing `--env-path` and it had no effect, it now has one — which is
the point, but it does mean a script that passed it harmlessly will suddenly use
a different pool than before.

### 3. Four data-loss fixes

These were the reason the release exists. In every case the old behavior was
"assume the user meant yes, or assume the path was safe."

**A non-interactive confirmation no longer counts as consent.** `treehouse
return <dirty-path> < /dev/null` used to take the documented `[Y/n]` bare-Enter
default and **clean away your uncommitted work**. The prompt reads as though
Enter means yes because that is the safe convention, but with no one there to
answer, "yes" was not an answer. EOF is now a refusal:

```bash
treehouse return "$p" < /dev/null   # refuses, exits 3, keeps your changes
treehouse return "$p" --force        # what you actually wanted in a script
```

**Unlanded commits block a reset.** A worktree holding commits that exist
nowhere else — not pushed, not on any other branch — was eligible to be reset
away. A worktree is now verified safe before anything rewrites it, and one that
cannot be verified is refused rather than assumed.

**Two clones of one origin no longer share a pool slot.** Slots were keyed by
origin URL, so cloning a repository twice gave you two working trees competing
for the same slot directory. The requesting clone's identity is now resolved
before the pool lock is taken, and a slot belonging to a different clone is
skipped. If identity cannot be read, the slot is skipped too — never guessed.

**Destructive git calls now require proof the path is a worktree.** Before
resetting or rewriting a checkout, treehouse verifies it is actually one of its
own worktrees. An unrelated directory is refused instead of operated on, and a
marker that cannot be read — a dangling symlink, a damaged checkout — is treated
as a failure to verify, not as an absent marker.

### Not data loss, but the same class of "the flag did nothing"

**`destroy --include-in-use` now terminates, on Linux and on Windows.** After
killing the processes in a worktree, the survivor re-scan still found the pid it
had just killed, so destroy skipped with *"worktree processes still running
after termination"* and `--include-in-use` was a no-op.

The cause was in the process table, not the kill: sysinfo refreshes a process's
`cwd` only when that field is currently unset (`ProcessRefreshKind::everything()`
uses `UpdateKind::OnlyIfNotSet`), so it never re-reads a cwd it already holds.
On Linux a killed child survives as a zombie, its `/proc/<pid>/cwd` disappears,
and the cached path — the worktree it died in — is what destroy saw. A zombie
has already terminated: it runs nothing and blocks no removal. Windows has the
same symptom from the other direction — it keeps a terminated process in the
table until the last handle to it closes, and sysinfo's Windows backend assigns
`status` exactly once (`ProcessStatus::Run`) and never updates it, so a zombie
check alone can never fire. Both are now excluded from the scan; on Windows that
asks the OS directly via `GetExitCodeProcess`, failing closed if the process
cannot be opened.

**A seed inventory could name a path outside the worktree on Windows.** The
inventory is git's path language — every separator a `/`, on every platform — so
"absolute" there means "starts with `/`", which is what Go checks with
`path.IsAbs`. The port used `Path::is_absolute()` instead, and on Windows that
is `has_root() && prefix().is_some()`: a rooted-but-prefixless name like
`/etc/passwd` is not absolute, so it passed validation, and
`worktree.join("/etc/passwd")` then dropped everything after the drive prefix
and resolved to `C:\etc\passwd`. That turned a gate on deleting ignored files
*inside* a worktree into one that could name files outside it. Now rejected on
every platform.

### Why some of this is only being fixed now

`ci.yml` ran `cargo test --workspace` **without `--no-fail-fast`**, so cargo
stopped at the first failing test target and every later target reported nothing
at all. On Windows the `treehouse` bin target failed first, which meant
`treehouse-core`'s lib tests — 17 of them, spanning pool, state, updater and the
VCS seam — **never ran on Windows**. On Linux the lib target failed first, which
meant the `update_e2e` integration tests never ran there either. And
`--features hardening` did not compile, behind a step that only ran once the
step before it passed, so the hardening tests had never been built. All of it
looked green. `--no-fail-fast` is now set on every test step.

---

## New capabilities

### A VCS seam

Every git operation now goes through one trait with a backend registry, so
adding a second VCS means implementing a trait rather than rewriting the pool.
Paths are resolved to a backend by marker: `.git` means git, `.jj` means jj, and
a path carrying neither is **refused by name** rather than quietly answered by
git — a misroute is a binary built for the wrong thing.

Every trait method carries a default that refuses with a clear message, so a new
backend needs no dispatch-site changes.

### APFS file-cloning sharing

On macOS, seeding a worktree can share the file's data blocks with the source
instead of copying them, via the kernel's `clonefile`. Files below 64 KB are
skipped — the per-file setup costs more than the duplicate bytes save, and small
files are most of a checkout. Every failure is a recorded skip, never a fatal
error, so an acquisition never fails because sharing could not happen.

```bash
treehouse get --lease --apfs-sharing fresh   # off (default) or fresh
```

> **It does not engage through a symlinked pool root, and on macOS that
> includes the default `TMPDIR`.** The share opens the destination with
> `O_NOFOLLOW` on every path component, so any symlink between the pool root and
> the slot is a refusal, not a redirection. macOS makes `/tmp` and `/var` (and
> therefore `$TMPDIR`, `/var/folders/…`) symlinks to `/private`, so a pool under
> a temp directory skips. Verified: the same repository, same volume, two pool
> roots differing only in symlinks —
>
> ```
> --root "$TMPDIR/pool"     # APFS sharing skipped: destination path unavailable
>                           #   or symlinked: Not a directory (os error 20)
> --root /private/var/…/pool
>                           # APFS sharing: cloned=1 logical_bytes=200000
>                           #   private_data_reduced_bytes=200704 below_threshold=1
> ```
>
> A symlinked pool root is the safe answer anyway — the slot must not be
> redirectable — but the skip is silent and looks like the feature is broken. If
> you want sharing, give `--root` a canonical path (`cd "$dir" && pwd -P`).
> Off macOS it is a correct no-op with the same "skipped" line.

### `.worktreeinclude` seeding

Commit a `.worktreeinclude` manifest and treehouse copies those paths into every
new or recycled worktree — the `.env` files and local config that gitignore
keeps out of the checkout but that every agent needs. `get --include-file <FILE>`
replaces the committed manifest for one acquisition.

It cannot be used to smuggle tracked content in: the manifest can only
authorize paths that are **already ignored**, and an uncommitted `.worktreeinclude`
authorizes nothing. Both halves are observable:

```console
$ treehouse get --lease                      # .worktreeinclude written but uncommitted
$ ls "$SLOT"/local.yml                       # no such file — nothing was seeded
$ git add .worktreeinclude && git commit
$ treehouse get --lease && ls "$SLOT"/local.yml
local.yml
```

An empty `--include-file` selects nothing; it does **not** fall back to the
committed manifest.

### Four new commands

```bash
treehouse lease <name>              # durably lease a worktree, don't touch it
treehouse doctor                    # read-only health report
treehouse doctor --strict           # treat any warning as a failure
treehouse gc                        # reclaim stale / orphaned / dead-owner slots (dry-run)
treehouse gc --yes --all            # actually do it, everywhere
treehouse run -- claude             # acquire, run, guarantee cleanup on every exit
```

`run` is the one for agents: everything from the first non-flag token on is your
command, so its own flags are never eaten by treehouse's parser, and the worktree
is returned whether the command exits 0, exits non-zero, or is killed.

`lease` is the one for orchestration: it hands you a worktree without entering it,
so a scheduler can queue work without holding a subshell open.

### Structured output for agents

`--format json|toon` is global and applies to agent-facing commands.

```bash
treehouse status --format json
treehouse status --format toon     # same data, cheaper to tokenize
```

`toon` is a token-oriented encoding — list-shaped payloads stay flat and
delimiting punctuation mostly disappears, which matters when an agent is reading
the output. We have not published a token-size figure, so measure it against
your own payloads before committing to the switch. `human` remains the default,
so nothing changes unless you ask.

> **Measured, not claimed:** on a real `treehouse status` payload the `toon`
> rendering came out *larger* than the JSON one — 291 B against 275 B for one
> slot, 2358 B against 2265 B for eight; re-measured on a second pool with
> longer paths, 335 B against 319 B. The absolute counts move with pool path
> length, but the direction did not across either fixture: `toon` lost by a
> consistent ~16 bytes. That is the opposite of the usual "40–60% smaller"
> folklore, and it is why we are not repeating it. `json` is the format to
> benchmark against; reach for `toon` when you have measured your own payloads
> and it wins.

### Assorted flags

- `get --base <branch>` — cut from a branch other than the repository default.
  Distinct from `-b/--branch`, which *creates* a branch. They are kept separate
  all the way down, because `--base main -b main` otherwise quietly reads as
  "create the branch we are already on".
- `get --no-fetch` — skip the origin fetch for offline or air-gapped machines.
- `get --worktree-path <template>` — place a new worktree at a templated path
  (`{pool}`, `{slot}`, `{repo}`, `{repo_parent}`).
- `get --lease --ttl 30m` — acquire non-interactively and print only the path.
- `return <name>` — the argument now also accepts a slot name as printed by
  `treehouse status`, not just a path.
- `return --all` — return everything you hold, leaving alone slots nobody holds.
- `return --if-lease-id <id>` — return only if the lease is still yours, so a
  second agent cannot pull a worktree out from under its holder.

### Self-update

```bash
treehouse update
```

Resolves your platform's asset, verifies it against the published SHA-256, and
replaces the binary atomically. Background version checks are cached, so being
offline costs you a notice, not a hang.

> **Scope of what was verified for this release.** The asset-selection, sidecar
> resolution, checksum-mismatch refusal, and atomic-replace paths are covered by
> the `updater.rs` unit tests, including `release_workflow_contract`, which
> parses the real `release.yml` — those pass. The **end-to-end** path
> (real download → verify → replace) was not exercised against a live release:
> the release base URL is compiled in (`updater.rs:35`) with no environment
> override, so there is no local-fixture seam to drive it from. Treat the
> download-and-replace path as tested at the unit level, not demonstrated
> end-to-end in this release.

The checksum sidecar bug is fixed. The updater had looked for
`<archive>.sha256` while the workflow publishes `<archive-base>.sha256`, so
every platform failed closed with "no checksums file in release assets". The
updater now accepts both spellings, preferring the exact one, and a
`release_workflow_contract` test module in `updater.rs` parses the real
`.github/workflows/release.yml` to prove every published asset has a sidecar the
updater will look for — so the two cannot drift apart again. Nothing is
installed and nothing is corrupted if a checksum is missing or unreadable; the
update simply refuses.

---

## Still open

Naming these because a reader skimming the source would otherwise assume more
than has been proven.

- **The jj backend is implemented, registered, and not exercised end to end.**
  `vcs/jj.rs` implements 21 of the `GitBackend` trait's 24 methods behind a
  `JjRunner` seam, and it is registered unconditionally
  (`registry.register(BACKEND_JJ, Arc::new(jj::JjBackend::discover()))`), so a
  `.jj` marker always resolves. Three things are true and worth separating:

  - **No jj binary, no jj run.** `which jj` finds nothing on the machine this
    release was built on, so every jj test runs against a scripted fake runner.
    Argument construction, marker and flavour resolution, the fail-closed
    refusals, and the clean/dirty reset protocol are exercised; **jj's own
    output formats are not verified by anything in this repository.** Treat jj
    as built and unproven, not as shipped-and-working.
  - **Creation does not go through the seam.** Reset, dirty checks, and the
    guarded reset dispatch per marker (`pool.rs:731`, `pool.rs:968`), but
    worktree *creation* still calls `git.worktree_add` directly
    (`pool.rs:1230`). A jj workspace that reached creation would get a git
    worktree. In practice it never gets that far: the pool opens by resolving
    the repository root through `ShellGitBackend` (`pool.rs:472`), which shells
    out to `git rev-parse --show-toplevel`, so as of this build
    `TREEHOUSE_VCS=jj treehouse get --lease` in a `.jj`-only directory exits 1
    with

    ```
    git git rev-parse --show-toplevel: fatal: not a git repository (or any of the parent directories): .git
    ```

    — a git error naming git, which is the wrong thing to tell someone who
    correctly selected jj. Moving root resolution behind the seam is in
    progress; when it lands this string is replaced by a refusal that names the
    `jj` backend, says it will not fall back to git, and points at `JJ_BIN`. Do
    not script against either string.
  - **Seeding is not implemented for jj.** Of the three trait methods jj leaves
    on their defaults, `resolve_for_worktree` simply dispatches back through the
    seam, so it costs nothing — but `seed_worktree` and
    `reset_worktree_with_seeded_paths` refuse, so a jj workspace cannot use
    `.worktreeinclude`. Everything else jj implements, including
    `is_worktree_safe_to_reset` and `reset_worktree_to_ref`.

  What is genuinely solid today: `--branch` is refused in a jj workspace with
  `--branch is only supported by the git backend (cannot create "feat/x" in a jj
  workspace)` rather than silently writing a git ref that means nothing. One
  precondition the paragraph above implies and this one did not: that refusal is
  reached only *after* the repository root resolves, so it is what you see in a
  checkout git can still open. In a `.jj`-only directory the root never resolves
  and the git error above is what you get instead.
- **VCS selection reads `TREEHOUSE_VCS` only.** The `vcs` key in
  `treehouse.toml` and `~/.config/treehouse/config.toml` is not consulted —
  `TreehouseConfig` has no `vcs` field. Opt in through the environment variable.
- **A missing `jj` binary is not reported as a configuration problem.** The
  backend is registered even when discovery finds no binary, so a `.jj` marker
  fails with `jj binary not found on PATH (set JJ_BIN)` rather than `no backend
  is registered for "jj"`, which would send you looking for a setting that is
  already correct.
- **Windows ARM64 has no prebuilt asset.** The release matrix builds
  `windows-x86_64` only. On Windows ARM64, `install.ps1` falls back to building
  from source, which works but is slower.

---

## Upgrading

```bash
treehouse --root . update
# or
curl -fsSL https://raw.githubusercontent.com/quangdang46/treehouse_rust/main/install.sh | bash
```

Before you upgrade, check your scripts for two things:

1. Anything that keys on `treehouse return`'s exit status — add
   `TREEHOUSE_EXIT_STRICT=0` if you cannot fix them today.
2. Anything that passes `--env-path` — it was being ignored before and is not
   now.

Existing pools keep working. No data format changed, and nothing in 0.2.0
requires you to re-run `treehouse init`.

Full technical detail, including every gap closed against upstream v3.1.0, is in
[CHANGELOG.md](CHANGELOG.md).