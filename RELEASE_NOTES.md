# treehouse 0.2.1

A patch release. No new commands, no changed flags, no changed output formats,
no changed exit codes, no changed state-file format. Existing pools keep
working; nothing needs `treehouse init` again.

It fixes seven defects, all of them found by installing 0.2.0 from its release
and driving every command against real git repositories — a plain repo, a
multi-branch repo, a repo with a committed `.worktreeinclude`, a repo with a
submodule, and a repo under a deliberately hostile global git config.

Two of the seven are inherited from upstream Go, which fixed them after this
port's v3.1.0 baseline; the rest are this port's own.

---

## Read this first: `treehouse get` failed on any repository with a submodule

This is the one that bites hardest, because it is not a corner case. If your
global git config sets `submodule.recurse = true` — a common setting, and the
default in some setups — then `treehouse get` **failed outright** on any
repository that has a submodule:

```
fatal: could not reset submodule index
```

The pooled git operations (`worktree add`, `reset`, `checkout`, `status`) were
running with your own submodule-recursion setting. In a linked worktree, a
submodule's gitdir points into the *main* repository's metadata, where it does
not exist — so any operation that recursed into a submodule failed.

Every git runner now pins `-c submodule.recurse=false`, which is what upstream
Go does. `fetch` is deliberately left alone: a repository that wants its
submodules fetched should keep getting them.

You do not need to change anything. If you were working around this by
avoiding submodules, you can stop.

---

## The rest

### A git failure named the program twice

Every git error read `git git rev-parse --show-toplevel: fatal: not a git
repository`. `GitError`'s `Display` spells the program itself, and the shell
backend behind the VCS seam also spelled it into the label. The seam had
already been fixed for this; the backend underneath it had not, and the backend
was the one answering.

### `treehouse enter` never said where it dropped you

`enter` opened the subshell and printed only `Pool state unchanged.` — no
banner naming the worktree, and none on the way out. It also reported "the
pool is empty" for *any* unknown name, even in a pool full of slots, which
sent you off to create another worktree instead of correcting the name.

It now matches upstream Go:

```
🌳 Entered worktree 3 at /path/to/pool/3/myrepo. Type 'exit' to leave.
🌳 Left worktree. Pool state unchanged.
```

and an unknown name lists the slots that exist:

```
no worktree named "9" in pool (available: 1, 2, 3). Run 'treehouse status' for details
```

A shell that cannot be started is now an error instead of a silent success.

### `treehouse watch --root <DIR>` was ignored

`--root` is documented as available on every command, and `watch` lists it in
its own flag table — but the sweep always covered the user-level root. A
`watch` pointed at another root swept the **wrong pools, on your own machine**.

`watch`, `gc --all` and `prune --all` now honour `--root` and
`TREEHOUSE_ROOT`, and sweep the tree they name. With neither set, they sweep
`~/.treehouse` exactly as before.

### The update notice could name a version that could not be installed

The background update check cached whatever `tag_name` the release API
returned, without looking at the release's assets. A release shipping no
archive for your OS/arch was advertised as available — and `treehouse update`
could never install it, so the notice could never be cleared.

The check now resolves the release's assets first and only advertises a
version your platform can actually install. `treehouse update` writes the cache
only after it has really replaced the binary, so a failed update no longer
leaves a poisoned cache behind.

### A Nix install was told to run a command that cannot work

A treehouse in the Nix store cannot be replaced in place — the store is
content-addressed and read-only, and such an install is upgraded through Nix.
`treehouse update` now refuses it with that explanation, and the update notice
stays silent for it.

### Empty leftover slot directories were never reclaimed

A slot directory left behind by a crashed run — with no state entry — was
skipped by the allocator forever. It was a directory no command could reach,
reclaim, or report, sitting on a slot that was never usable again.

An empty leftover is now reclaimed before a new slot is chosen, so the slot
becomes available again. A leftover that holds anything is left exactly where
it is — it may be a worktree whose state entry was lost, and its contents are
somebody's work — and a symlinked slot is never followed and never removed.

---

## Still open

Naming these because a reader skimming the source would otherwise assume more
than has been proven. None of them changed in 0.2.1.

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
    guarded reset dispatch per marker, but worktree *creation* still calls
    `git.worktree_add` directly. A jj workspace that reached creation would get
    a git worktree. In practice it never gets that far: the pool opens by
    resolving the repository root through `ShellGitBackend`, which shells out
    to `git rev-parse --show-toplevel`, so as of this build
    `TREEHOUSE_VCS=jj treehouse get --lease` in a `.jj`-only directory exits 1
    with

    ```
    git rev-parse --show-toplevel: fatal: not a git repository (or any of the parent directories): .git
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
treehouse update
# or
curl -fsSL https://raw.githubusercontent.com/quangdang46/treehouse_rust/main/install.sh | bash
```

Nothing in your scripts needs changing: no flag, output format, exit code, or
state-file field changed. Existing pools keep working.

Full technical detail is in [CHANGELOG.md](CHANGELOG.md).
