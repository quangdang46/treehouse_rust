# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet.

## [0.2.0] - 2026-10-01

A 0.x minor, so it carries breaking changes. The two that matter are the exit
status and `--env-path`; both are documented in `RELEASE_NOTES.md` and both have
a one-release migration path.

### Breaking

#### Exit 3 is now the default when a dirty worktree is left unreturned

`treehouse return` previously exited 0 even when it declined to clean a dirty
worktree, so a caller reading exit 0 as "the slot was released" was reading a
bug. It now exits 3 — the status Go shipped in v3.0.0 — and keeps the slot held.

`TREEHOUSE_EXIT_STRICT=0` restores the old exit 0 and its byte-identical output
for one minor release. The variable changed polarity: it used to be opt-in and is
now opt-out, so only a value Go's `strconv.ParseBool` reads as false (`0`, `f`,
`false`, `off`, `n`, `no`) restores exit 0. An unrecognised value is deliberately
not the opt-out — `TREEHOUSE_EXIT_STRICT=1` left over from 0.1.x keeps exit 3,
because handing the escape hatch to a typo is the one direction that can
silently under-report a slot that was never released.

#### `--env-path` is now an alias of `--root`

`--env-path <DIR>` used to be accepted and ignored, falling back to
`~/.treehouse`. It is now a visible alias of the global `--root` flag, so it
resolves to `<DIR>/.treehouse` (`<repo>/.treehouse` for a relative `DIR`).
Precedence is `--root` > `TREEHOUSE_ROOT` > `treehouse.toml` `root` >
`~/.treehouse`, matching Go.

### Fixed

Four paths could destroy work a user had not asked treehouse to destroy.

- **A non-interactive confirmation no longer reads as consent.** At stdin EOF
  `read_line` returns `Ok(0)`, not `Err`, so `treehouse return <dirty-path>
  < /dev/null` used to take the documented `[Y/n]` bare-Enter default and clean
  uncommitted work. The bytes actually read now decide the outcome, and EOF is
  a refusal. This is the fix with the widest blast radius — it can turn a script
  that used to succeed into one that stops.
- **An unlanded-commit guard now runs before reset.** A worktree holding commits
  that exist nowhere else was eligible to be reset away. `is_worktree_safe_to_reset`
  refuses unless the worktree is verified clean, and an unverifiable worktree is
  refused rather than assumed safe.
- **Pools are scoped by clone identity, not by origin URL alone.** Two clones of
  one origin used to resolve to the same pool and contend for the same slots.
  The requester's clone identity is resolved once, before the state lock, and a
  slot owned by a different clone is classified `OtherClone` and skipped rather
  than handed out. An identity that cannot be read is `Unverified` and is also
  skipped — never guessed at.
- **Destructive git calls carry a marker precondition.** `require_worktree_marker`
  gates the operations that rewrite a checkout, so a path that is not a
  treehouse worktree fails closed instead of being operated on. A marker that
  cannot be resolved — a dangling symlink, for instance — is a read failure, not
  an absent marker.

### Added

#### VCS seam

The `GitBackend` trait every backend must implement, with a registry, per-marker
backend resolution (`.git` → git, `.jj` → jj), and `destructive_backend_for_worktree`
as the single dispatch point for operations that rewrite a checkout. A path
carrying no recognised marker is refused by name rather than answered by git.
Every trait method has a default that refuses with a clear "the <backend>
backend does not support <operation>", so adding a backend needs no trait edit
and no dispatch change. The jj backend is registered in the same registry and
implements 21 of the trait's 24 methods; see "Known gaps" for the three it leaves
on their defaults and for why none of that has been run against a real jj yet.

#### Worktree seeding

`GitBackend::seed_worktree` copies backend-specific ignored files into a new or
recycled worktree and returns the paths it wrote, so cleanup trusts that
inventory instead of re-reading mutable worktree metadata. The git backend reads
a committed `.worktreeinclude` manifest from the destination worktree's HEAD.
`.worktreeinclude` can never widen what gets copied into a worktree: it can only
authorize paths that are already ignored, and an uncommitted manifest authorises
nothing.

#### Commands

- `treehouse lease <name>` — durably lease an existing worktree by name without
  touching it.
- `treehouse doctor [--strict]` — read-only health report; `--strict` treats any
  warning as a failure.
- `treehouse gc [--all] [--prune-orphans] [-v] [--yes]` — reclaim stale,
  orphaned, and dead-owner worktrees. Dry-run by default.
- `treehouse run [--ttl <d>] [--lease-holder <who>] -- <cmd>` — acquire, run a
  command in the worktree, and guarantee cleanup on every exit. Everything from
  the first non-flag token on is the command, so its own flags are never eaten
  by treehouse's parser.

#### Flags

- `--format human|json|toon`, global, on agent-facing commands. `toon` is a
  token-oriented encoding for the list-shaped output `status` and
  `lease --json` produce. **It is not smaller than `json` here** — on a measured
  `status` payload the `toon` rendering came out *larger* (335 B against 319 B
  for one slot, on a pool with long paths; 291 B against 275 B on another). The
  absolute counts move with pool path length, but the direction did not across
  either fixture: `toon` lost by a consistent ~16 bytes. The usual "40–60%
  smaller" folklore does not hold for this payload, so `json` is the format to
  benchmark against and `toon` is worth reaching for only after measuring your
  own.
- `--root <DIR>`, global (see the breaking change above).
- `get --lease`, `--lease-holder <who>`, `--ttl <d>`, `--json` — non-interactive
  acquire that prints only the path.
- `get --base <branch>` — cut from this branch instead of the repository default.
  Distinct from `-b/--branch`, which *creates* a branch; they are kept separate
  all the way down, because `--base main -b main` otherwise reads as "create the
  branch the worktree is already on".
- `get --no-fetch` — skip the origin fetch, for offline and air-gapped use.
- `get --worktree-path <template>` — place a new worktree at a templated path
  (`{pool}`, `{slot}`, `{repo}`, `{repo_parent}`); overrides `--unique-leaf`.
- `get --include-file <FILE>` — replace the committed `.worktreeinclude` with
  this file for this acquisition. An empty file seeds nothing and does *not*
  fall back to the committed manifest.
- `get --apfs-sharing off|fresh` — share tracked file data with the source
  checkout on macOS/APFS. Defaults to `$TREEHOUSE_APFS_SHARING`, then `off`.
  Every failure is a recorded skip, never a fatal error. Note the symlinked
  pool-root limitation in "Known gaps".
- `return <name>` — the path argument now also accepts a slot name as printed by
  `treehouse status`.
- `return --all` — return every held worktree in this repository's pool, leaving
  alone slots nobody holds.
- `return --if-lease-id <id>` / `--if-lease-holder <who>` — return only if the
  current lease matches, so a second agent cannot return a worktree out from
  under its holder.

#### Self-update

`treehouse update` resolves this platform's asset from the latest GitHub
release, verifies it against the published `.sha256` sidecar, and replaces the
running binary atomically — staged temp file in the target's own directory, then
rename — so an interrupted update can never leave a half-written executable at
the target path. macOS quarantine is cleared before the replace. A missing or
unreadable checksum fails closed: the binary is not replaced. Version checks run
in the background and are cached, so a machine that is offline is not blocked.
The `release_workflow_contract` tests in `updater.rs` hold this honest: they
parse `.github/workflows/release.yml` and assert that every asset it publishes
has a sidecar name the updater accepts, which is the claim that was false until
this release.

### Fixed (self-update)

- **The checksum sidecar is now found.** The updater looked for
  `<archive>.sha256` (for example
  `treehouse-v0.2.0-linux-x86_64.tar.gz.sha256`) while
  `.github/workflows/release.yml` publishes `<archive-base>.sha256` (for example
  `treehouse-v0.2.0-linux-x86_64.sha256`), so every platform failed closed with
  "no checksums file in release assets". The updater now accepts both spellings,
  preferring the exact one, so no existing release has to be re-uploaded. A
  `release_workflow_contract` test module in `updater.rs` reads the real
  workflow and proves every asset it publishes has a sidecar the updater will
  look for; a fixture could not, because it would stay green after a rename.
- The update pipeline refuses to proceed without a checksum, rather than
  installing an unverified binary and reporting success.

### Known gaps in 0.2.0

Named here because each has a doc comment or a module that reads as finished.
None of them change existing behavior.

- **The jj backend is implemented and registered, and has never been run against
  a real jj.** `vcs/jj.rs` implements 21 of the trait's 24 methods behind a
  `JjRunner` seam, and `vcs/mod.rs` registers it unconditionally. Registration
  is deliberately infallible: a machine with no `jj` binary still gets a
  registered backend, so a `.jj` marker fails with `jj binary not found on PATH
  (set JJ_BIN)` rather than a configuration-shaped error. Because no `jj` binary
  exists on the build machine, all of its coverage is unit tests against a
  scripted runner — **jj's own output formats are unverified by this
  repository's suite.**
- **Worktree creation bypasses the seam.** Reset (`pool.rs:731`,
  `pool.rs:968`) and dirty checks dispatch on the worktree's marker; creation
  calls `git.worktree_add` directly (`pool.rs:1230`). The pool also resolves its
  repository root through `ShellGitBackend` (`pool.rs:472`), so a `.jj`-only
  directory fails there before creation is reached — as of this build
  `TREEHOUSE_VCS=jj treehouse get --lease` exits 1 with
  `git git rev-parse --show-toplevel: fatal: not a git repository (or any of
  the parent directories): .git`. Fail-closed, but the jj acquisition path is
  unreachable rather than merely incomplete, and the message names git rather
  than jj. Root resolution is being moved behind the seam; when it lands the
  message becomes a refusal that names the jj backend and points at `JJ_BIN`.
- **Seeding is not implemented for jj.** `seed_worktree` and
  `reset_worktree_with_seeded_paths` keep the trait's refusing default, so a jj
  workspace cannot use `.worktreeinclude`. The third inherited default,
  `resolve_for_worktree`, dispatches back through the seam and costs nothing. jj
  implements the rest, including the guarded-reset pair.
- **VCS selection reads `TREEHOUSE_VCS` only.** The `vcs` key in
  `treehouse.toml` and in `~/.config/treehouse/config.toml` is not consulted,
  because this port has no `vcs` field on its config type.
- **APFS sharing never engages through a symlinked pool root, and on macOS that
  includes `/tmp` and the default `$TMPDIR` (`/var/folders/…`).** The share
  opens the destination with `O_NOFOLLOW` on every path component, so a symlink
  between the pool root and the slot is a skip, not a redirection. Reproduced
  on one repository and volume with two pool roots differing only in symlinks:
  the `/var/folders/…` root printed `APFS sharing skipped: destination path
  unavailable or symlinked: Not a directory (os error 20)`; the canonical
  `/private/var/…` root printed `APFS sharing: cloned=1 logical_bytes=200000
  private_data_reduced_bytes=200704 below_threshold=1`. The refusal is correct
  but silent, so a symlinked pool root reads as a broken feature.

[Unreleased]: https://github.com/quangdang46/treehouse_rust/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/quangdang46/treehouse_rust/compare/v0.1.2...v0.2.0