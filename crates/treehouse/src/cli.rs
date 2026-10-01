//! The clap CLI surface for treehouse.
//!
//! Each subcommand calls into `treehouse-core` and renders the result via the
//! formatter. The Go CLI contract (plan Appendix B) is reproduced: exit codes,
//! stdout/stderr routing, and human message strings (byte-exact where Go-tested).

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use treehouse_core::git::GitBackend;

/// Treehouse — manage a pool of reusable git worktrees for parallel AI coding agents.
#[derive(Debug, Parser)]
#[command(
    name = "treehouse",
    version = treehouse_core::VERSION,
    about = "Manage a pool of git worktrees for parallel AI agent workflows",
    disable_help_subcommand = true,
    propagate_version = true
)]
pub struct Cli {
    /// Hidden background-update-check argument handled in main before clap.
    #[arg(long, hide = true)]
    pub update_check: bool,

    /// Output format for agent-facing commands.
    #[arg(long, value_enum, global = true, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Custom pool root directory, overriding TREEHOUSE_ROOT and
    /// treehouse.toml's root; relative paths (e.g. "." for an in-project pool)
    /// resolve from the repo root. Falls back to ~/.treehouse.
    #[arg(long, visible_alias = "env-path", value_name = "DIR", global = true)]
    pub root: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Toon,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Alias for `get` — acquire a worktree and open a subshell.
    #[command(name = "get")]
    Get(GetArgs),
    /// Attach to an existing worktree by name, even if in use.
    Enter(EnterArgs),
    /// Release a lease, terminate lingering processes, reset, return to pool.
    Return(ReturnArgs),
    /// Durably lease an existing worktree by name, without touching it.
    Lease(LeaseArgs),
    /// Show pool status.
    Status(StatusArgs),
    /// Dry-run removal of stale idle worktrees.
    Prune(PruneArgs),
    /// Dry-run removal of worktrees.
    Destroy(DestroyArgs),
    /// Reclaim stale, orphaned, and dead-owner worktrees (dry-run default).
    Gc(GcArgs),
    /// Acquire -> run an agent -> cleanup guaranteed on every exit.
    Run(RunArgs),
    /// Read-only health report.
    Doctor(DoctorArgs),
    /// Sweep all pools (scheduled cleanup entrypoint).
    Watch(WatchArgs),
    /// Create a default treehouse.toml.
    Init,
    /// Update treehouse to the latest release.
    Update,
}

/// Acquisition options for `treehouse get`.
///
/// `Default` is derived so a bare `treehouse` (no subcommand) can be expressed
/// as `GetArgs::default()`. A hand-written literal here would have to be
/// updated every time a flag is added, and the compiler would NOT catch the
/// omission at the point where it matters — a new flag would simply be absent
/// from the default `get`, which is the same silent drop that once cost
/// `get --lease` its `AcquireOptions`.
#[derive(Debug, Args, Default)]
pub struct GetArgs {
    /// Durably lease instead of opening a subshell; path-only on stdout.
    #[arg(long)]
    pub lease: bool,
    /// Record who holds the lease.
    #[arg(long)]
    pub lease_holder: Option<String>,
    /// Make the lease expire (e.g. 30m, 1h30m).
    #[arg(long)]
    pub ttl: Option<String>,
    /// Print the lease as JSON (requires --lease).
    #[arg(long)]
    pub json: bool,
    /// Cut from this branch instead of the repository default (Go
    /// `BaseBranch`). This is NOT Go's `-b`, which *creates* a branch.
    #[arg(long, value_name = "BRANCH")]
    pub base: Option<String>,
    /// Do not fetch origin before acquiring (offline / air-gapped).
    #[arg(long)]
    pub no_fetch: bool,
    /// Create and check out this new branch at the acquired commit (Go `-b`,
    /// `AcquireOptions.Branch`). Distinct from `--base`, which only changes
    /// where the worktree is cut FROM. Fails if the branch already exists.
    #[arg(short = 'b', long, value_name = "BRANCH")]
    pub branch: Option<String>,
    /// Name a newly created worktree directory `<repo>-<slot>` instead of
    /// `<repo>` (Go `UniqueLeaf`). Defaults to $TREEHOUSE_UNIQUE_LEAF.
    #[arg(long)]
    pub unique_leaf: bool,
    /// Template for a newly created worktree's directory (Go
    /// `WorktreePath`), e.g. `{pool}/{slot}/{repo}`. Supported placeholders:
    /// {pool}, {slot}, {repo}, {repo_parent}. Overrides --unique-leaf.
    /// Defaults to $TREEHOUSE_WORKTREE_PATH.
    #[arg(long, value_name = "TEMPLATE")]
    pub worktree_path: Option<String>,
    /// Replace the committed `.worktreeinclude` with this file for THIS
    /// acquisition (Go `IncludeManifest`). An empty file seeds nothing; it
    /// does NOT fall back to the committed manifest.
    #[arg(long, value_name = "FILE")]
    pub include_file: Option<String>,
    /// Share tracked file data in fresh Git slots on macOS/APFS: `off` or
    /// `fresh`. Overrides $TREEHOUSE_APFS_SHARING. Never fails an acquisition
    /// — a slot that cannot be shared keeps its plain copy.
    #[arg(long, value_name = "MODE")]
    pub apfs_sharing: Option<String>,
}

#[derive(Debug, Args)]
pub struct LeaseArgs {
    /// Label recorded as the lease holder (defaults to
    /// $TREEHOUSE_LEASE_HOLDER).
    #[arg(long)]
    pub lease_holder: Option<String>,
    /// Print the lease identity as JSON instead of the bare path.
    #[arg(long)]
    pub json: bool,
    /// The worktree name, as printed by `treehouse status`.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct EnterArgs {
    /// Print only the worktree path.
    #[arg(long)]
    pub print_path: bool,
    /// The worktree name (from status).
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ReturnArgs {
    /// Clean, reset, and return without prompting.
    #[arg(long)]
    pub force: bool,
    /// Return every held worktree in this repository's pool (leaves alone
    /// slots nobody holds).
    #[arg(long)]
    pub all: bool,
    /// Return only if the current lease has this identity.
    #[arg(long)]
    pub if_lease_id: Option<String>,
    /// Return only if the current lease has this holder.
    #[arg(long)]
    pub if_lease_holder: Option<String>,
    /// The worktree to return: an absolute path, or a slot name as printed by
    /// `treehouse status` (defaults to $TREEHOUSE_DIR).
    pub path: Option<String>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Make the lease expire (e.g. 30m, 24h).
    #[arg(long)]
    pub ttl: Option<String>,
    /// Label recorded as the lease holder (defaults to run:<pid>).
    #[arg(long)]
    pub lease_holder: Option<String>,
    /// The command to run inside the worktree. Everything from the first
    /// non-flag token on is the command, so its own flags are never eaten by
    /// treehouse's own parser.
    #[arg(last = true, required = true, value_name = "CMD")]
    pub cmd: Vec<String>,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Print status + lease metadata as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Execute instead of dry-run.
    #[arg(long)]
    pub yes: bool,
    /// Sweep every managed pool under the user-level root.
    #[arg(long = "all", visible_alias = "global")]
    pub all: bool,
    /// Include backing-repository-missing orphans.
    #[arg(long)]
    pub prune_orphans: bool,
    /// Show detailed skip diagnostics.
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Treat any warning as a failure.
    #[arg(long)]
    pub strict: bool,
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Run a single sweep and exit (no background loop).
    #[arg(long)]
    pub once: bool,
    /// Sweep interval for the foreground loop (e.g. 30s, 5m). Default: 60s.
    /// Ignored when --once is set.
    #[arg(long, value_parser = parse_duration)]
    pub interval: Option<std::time::Duration>,
    /// Execute instead of dry-run.
    #[arg(long)]
    pub yes: bool,
    /// Include backing-repository-missing orphans.
    #[arg(long)]
    pub prune_orphans: bool,
}

/// Parse a human-readable duration string (e.g. "30s", "5m", "1h30m").
pub(crate) fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    humantime::parse_duration(s).map_err(|e| format!("invalid duration '{s}': {e}"))
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Execute instead of dry-run.
    #[arg(long)]
    pub yes: bool,
    /// Sweep every managed pool under the user-level root.
    #[arg(long = "all", visible_alias = "global")]
    pub all: bool,
    /// Include backing-repository-missing orphans.
    #[arg(long)]
    pub prune_orphans: bool,
    /// Show detailed skip diagnostics.
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

#[derive(Debug, Args)]
pub struct DestroyArgs {
    /// Remove all worktrees in the named pool.
    #[arg(long)]
    pub all: bool,
    /// Execute instead of dry-run.
    #[arg(long)]
    pub yes: bool,
    /// Also remove dirty, unmerged, or unverified worktrees.
    #[arg(long)]
    pub include_unlanded: bool,
    /// Also remove worktrees with a running process.
    #[arg(long)]
    pub include_in_use: bool,
    /// Also remove a leased worktree (single named path only).
    #[arg(long)]
    pub include_leased: bool,
    /// A single worktree path, or a pool path with --all.
    pub path: Option<String>,
}

impl GetArgs {
    /// Builds the core [`AcquireOptions`] this invocation requests.
    ///
    /// Precedence for the two placement options is `flag > env > config`
    /// (Go `config.ResolveUniqueLeaf` / `ResolveWorktreePath`,
    /// config.go:126-155). The config tiers are not read here — this crate does
    /// not own `TreehouseConfig` — so the chain stops at `env`, and a pool
    /// configured with `unique_leaf = true` is picked up by the core instead.
    ///
    /// `--base` and `-b` stay separate all the way down: `base` is the branch a
    /// worktree is cut FROM, `branch` is the branch that gets CREATED. Merging
    /// them here would silently turn `--base main -b main` into a request to
    /// create the branch the worktree is already on.
    ///
    /// `--include-file` and `--apfs-sharing` are resolved HERE rather than in
    /// `main.rs` for the same reason `--base` is: this is the one place both
    /// acquisition modes read, so a new option cannot reach `get` while being
    /// silently dropped by `get --lease`.
    pub fn acquire_options(&self) -> anyhow::Result<treehouse_core::pool::AcquireOptions> {
        // `--branch` must name something: an empty value would otherwise read
        // as "no branch requested" and the flag would be a silent no-op.
        if let Some(b) = self.branch.as_deref()
            && b.is_empty()
        {
            anyhow::bail!("--branch requires a non-empty branch name");
        }

        let unique_leaf =
            self.unique_leaf || env_flag(treehouse_core::pool::TREEHOUSE_UNIQUE_LEAF_VAR);
        let worktree_path = self
            .worktree_path
            .clone()
            .or_else(|| non_empty_env(treehouse_core::pool::TREEHOUSE_WORKTREE_PATH_VAR));

        Ok(treehouse_core::pool::AcquireOptions {
            branch: self.base.clone(),
            skip_fetch: self.no_fetch,
            new_branch: self.branch.clone(),
            unique_leaf,
            worktree_path,
            include_manifest: self.read_include_manifest()?,
            apfs_sharing: self.resolve_apfs_sharing()?,
            ..Default::default()
        })
    }

    /// The `--include-file` manifest bytes, read ONCE and before acquisition
    /// can reset the checkout holding it.
    ///
    /// Go reads it in `getRunE` for the same reason (cmd/get.go:120-127): the
    /// file usually lives in the repository being cut from, and an acquisition
    /// may reset or move that checkout. Reading it lazily, after the pool has
    /// already started rewriting worktrees, turns a race into a truncated
    /// manifest.
    ///
    /// `Some(vec![])` is a valid, meaningful value — "replace the committed
    /// `.worktreeinclude` with nothing" — and must reach the core as an empty
    /// manifest rather than as `None`, which would select the committed one.
    fn read_include_manifest(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(path) = self.include_file.as_deref() else {
            return Ok(None);
        };
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("failed to read include file {path:?}: {e}"))?;
        Ok(Some(bytes))
    }

    /// Resolves `--apfs-sharing` through `flag > env > off` (Go
    /// `config.ResolveAPFSSharing`, config.go:77-95).
    ///
    /// The config tier is absent for the same reason it is absent for
    /// `unique_leaf`: `TreehouseConfig` is not this crate's type. The chain
    /// therefore stops at the environment, and a `apfs_sharing = "fresh"` in
    /// `treehouse.toml` is picked up by the core rather than here.
    fn resolve_apfs_sharing(&self) -> anyhow::Result<bool> {
        let value = self
            .apfs_sharing
            .clone()
            .or_else(|| non_empty_env(APFS_SHARING_VAR));
        match value.as_deref().map(str::trim) {
            None | Some("") | Some("off") => Ok(false),
            Some("fresh") => Ok(true),
            Some(other) => anyhow::bail!("invalid apfs_sharing {other:?}: use off or fresh"),
        }
    }
}

/// Env var that opts into APFS sharing without the flag (Go
/// `config.APFSSharingEnvVar`, config.go:75).
pub const APFS_SHARING_VAR: &str = "TREEHOUSE_APFS_SHARING";

/// Reads an env var that only reads as a boolean when it parses (Go
/// `strconv.ParseBool`, config.go:135). An unparseable value is treated as
/// UNSET rather than as an accidental opt-in or opt-out.
fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "t" | "yes" | "y" | "on"
        ),
        Err(_) => false,
    }
}

/// Reads an env var, treating an empty value as unset (Go
/// `os.Getenv(name) != ""`, config.go:150).
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The repo root + pool dir resolved for the current invocation.
pub struct RepoCtx {
    pub repo_root: PathBuf,
    pub remote_url: Option<String>,
    pub config: treehouse_core::config::TreehouseConfig,
}

/// Resolves the current repo context (Go: FindRepoRoot + GetRemoteURL + Load).
pub fn resolve_repo_ctx() -> anyhow::Result<RepoCtx> {
    let git = treehouse_core::git::ShellGitBackend::discover()?;
    let cwd = std::env::current_dir()?;
    let repo_root = git.repo_root(&cwd)?;
    let remote_url = git.remote_url(
        &treehouse_core::git::GitRepo {
            common_dir: repo_root.clone(),
            worktree: None,
        },
        "origin",
    );
    let config = treehouse_core::config::TreehouseConfig::load(&repo_root)?;
    Ok(RepoCtx {
        repo_root,
        remote_url,
        config,
    })
}

/// Opens a pool for the current repo context, with `--root` applied.
///
/// `--root` travels as [`OpenOptions::root_override`] rather than as an
/// injected `TreehouseEnv` pool root: the injected tier is the LAST fallback
/// in the resolver, below `TREEHOUSE_ROOT`, and Go ranks an explicit flag above
/// both (config.go:110-117).
pub fn open_pool_with_root(
    ctx: &RepoCtx,
    root: Option<&str>,
) -> anyhow::Result<treehouse_core::pool::Pool> {
    let opts = treehouse_core::pool::OpenOptions {
        config: ctx.config.clone(),
        root_override: root.map(|r| r.to_string()),
        ..Default::default()
    };
    Ok(treehouse_core::pool::Pool::open(
        &ctx.repo_root,
        ctx.remote_url.as_deref(),
        &opts,
    )?)
}

// ─── Backend selection ───────────────────────────────────────────────────────

/// The environment variable that selects the VCS backend (Go `vcsOverride`,
/// vcs.go:196-207 — the highest-precedence tier).
pub const TREEHOUSE_VCS_VAR: &str = "TREEHOUSE_VCS";

/// Whether `value` names a backend this port knows, matching Go's
/// `normalizeVCSName` (vcs.go:219-224).
///
/// An unrecognised value normalises to "nothing", which leaves the repository
/// on git — deliberately not an error, because Go treats a typo in this
/// variable as "no override" and a repository that selects jj through the
/// config tiers must not be dragged onto git by a stray `TREEHOUSE_VCS=jjj`.
fn normalize_vcs(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "git" => Some("git"),
        "jj" => Some("jj"),
        _ => None,
    }
}

/// Refuses `--branch` against the jj backend (Go `cmd/get.go:148-150`).
///
/// `--branch` CREATES a branch. Inside a jj workspace the branch is not a git
/// ref at all, so carrying the flag through would either fail somewhere deep
/// in acquisition or — worse — report success for an operation that never
/// happened. Go refuses up front, and so does this.
///
/// The refusal is driven by the backend the repository **selects**, not by the
/// mere presence of a `.jj` directory. Go's `backendFor` (vcs.go:157-166)
/// deliberately falls back to git for a jj-marked tree that was never opted in,
/// so a git repository that merely sits next to a `.jj` directory keeps
/// `--branch`; refusing there would break a working invocation.
///
/// Only the `TREEHOUSE_VCS` tier is consulted. Go's two lower tiers — `vcs` in
/// `treehouse.toml` and in `~/.config/treehouse/config.toml` — need a `vcs`
/// field on `TreehouseConfig`, which this port does not have; until it does, a
/// repository that opts into jj through config rather than the environment is
/// not caught here. That gap is inert while the jj backend itself is
/// unimplemented, because a `.jj` path leads to "no backend is registered for
/// \"jj\"" rather than to a misbehaving acquisition — but it must be closed
/// before the jj backend ships.
pub fn require_git_backend_for_branch(
    repo_root: &Path,
    branch: Option<&str>,
) -> anyhow::Result<()> {
    let Some(branch) = branch.filter(|b| !b.is_empty()) else {
        return Ok(());
    };
    let override_name = normalize_vcs(&std::env::var(TREEHOUSE_VCS_VAR).unwrap_or_default());
    if override_name != Some(treehouse_core::vcs::BACKEND_JJ) {
        return Ok(());
    }
    // The backend the repository SELECTS (Go `BackendNameFor`, vcs.go:669) —
    // not the slot flavour, and not the mere presence of a `.jj` directory.
    // `configured_backend_name` is the seam's port of exactly that function, so
    // the two questions stay apart the way Go keeps them apart: in a colocated
    // repository `.git` wins here while [`worktree_backend_name`] would answer
    // for the slot, and jj colocates with git by design (`jj git init`), so the
    // colocated layout is the NORMAL one.
    //
    // A marker that cannot be READ is propagated as an error, not reported as
    // "git". This port separates "no marker" from "unreadable marker" precisely
    // so a damaged checkout is never quietly classified; collapsing the two
    // here would let `--branch {branch}` proceed in a repository whose backend
    // could not be identified, which is the exact case the check exists for.
    let selected = treehouse_core::vcs::configured_backend_name(repo_root, override_name)
        .map_err(|e| anyhow::anyhow!("resolving the vcs backend for {}: {e}", repo_root.display()))?;

    // Selection is not the same question as readability, and this gate must not
    // collapse them. `configured_backend_name` answers "which backend would
    // CREATE a worktree here", and it follows Go's `findMarkerRoot`, which
    // stats with `os.Stat` and therefore counts a marker it cannot read as
    // ABSENT — right for selection, where the alternative is inventing a
    // backend. It is wrong for a REFUSAL: a `.jj` pointing at a deleted
    // workspace reads as "no jj here", and `--branch` would walk straight into
    // the checkout this check exists to protect.
    jj_marker_readable(repo_root)?;

    if selected == treehouse_core::vcs::BACKEND_JJ {
        anyhow::bail!(
            "--branch is only supported by the git backend \
             (cannot create {branch:?} in a jj workspace)"
        );
    }
    Ok(())
}

/// Fails when `path` holds a `.jj` marker that exists but cannot be READ.
///
/// This is a guard on a REFUSAL, not a backend selector — deciding WHICH
/// backend a repository uses is [`treehouse_core::vcs::configured_backend_name`]
/// alone, and duplicating that here is what this function exists to avoid. What
/// it adds is only the distinction the seam draws for slot flavour
/// ([`treehouse_core::vcs::worktree_backend_name`], which reports an unreadable
/// marker as an error) but which the selection path deliberately does not: an
/// unreadable marker is a damaged checkout, never evidence of absence.
///
/// The check is scoped to `.jj` specifically because that is the marker this
/// refusal is about. Asking [`treehouse_core::vcs::worktree_backend_name`]
/// instead would be useless here — it answers on `.git` first and returns
/// without ever looking at `.jj`, so in a COLOCATED repository (the normal jj
/// layout, `jj git init`) a damaged `.jj` would sail straight through.
fn jj_marker_readable(path: &Path) -> anyhow::Result<()> {
    let marker = path.join(".jj");
    // `symlink_metadata` does not follow, so a DANGLING symlink counts as
    // present — its unresolvable target is damage to report, not absence.
    match std::fs::symlink_metadata(&marker) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => anyhow::bail!("reading .jj marker in {}: {e}", path.display()),
    }
    std::fs::metadata(&marker).map_err(|e| {
        anyhow::anyhow!("resolving .jj marker in {}: {e}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Parses argv the way the binary does, so a test that passes here proves
    /// the flag is really REGISTERED — not merely that a struct field exists.
    fn parse_get(args: &[&str]) -> GetArgs {
        match Cli::try_parse_from(args).expect("argv must parse").command {
            Some(Command::Get(a)) => a,
            _ => panic!("expected the get subcommand"),
        }
    }

    /// The three restored flags must each REACH `AcquireOptions` with the
    /// value the user typed. Before the fix these arguments were rejected
    /// outright with "unexpected argument", so parsing alone is already the
    /// first half of the proof; the option assertions are the second.
    #[test]
    fn the_acquisition_flags_reach_acquire_options() {
        let a = parse_get(&[
            "treehouse",
            "get",
            "--base",
            "develop",
            "-b",
            "feat/x",
            "--unique-leaf",
        ]);
        let o = a.acquire_options().expect("options must build");
        assert_eq!(
            o.branch.as_deref(),
            Some("develop"),
            "--base is the cut FROM"
        );
        assert_eq!(
            o.new_branch.as_deref(),
            Some("feat/x"),
            "-b is the branch CREATED"
        );
        assert!(o.unique_leaf, "--unique-leaf must reach the core");

        let a = parse_get(&[
            "treehouse",
            "get",
            "--worktree-path",
            "{pool}/{slot}/{repo}-w",
        ]);
        let o = a.acquire_options().expect("options must build");
        assert_eq!(o.worktree_path.as_deref(), Some("{pool}/{slot}/{repo}-w"));

        let a = parse_get(&["treehouse", "get", "--no-fetch"]);
        assert!(a.acquire_options().unwrap().skip_fetch);
    }

    /// `--base` and `-b` are DIFFERENT options and must never collapse into one.
    /// Conflating them is how a caller ends up resetting every slot to a branch
    /// it meant to create.
    #[test]
    fn base_and_branch_stay_distinct() {
        let a = parse_get(&["treehouse", "get", "--base", "main", "-b", "feature"]);
        let o = a.acquire_options().unwrap();
        assert_eq!(o.branch.as_deref(), Some("main"));
        assert_eq!(o.new_branch.as_deref(), Some("feature"));
        assert_ne!(o.branch, o.new_branch);
    }

    /// An empty `-b` must be an error, not a silent no-op — otherwise the user
    /// believes a branch was created when none was.
    #[test]
    fn an_empty_branch_name_is_refused() {
        let a = parse_get(&["treehouse", "get", "-b", ""]);
        let err = a
            .acquire_options()
            .expect_err("empty --branch must be refused");
        assert!(
            err.to_string().contains("non-empty branch name"),
            "got: {err}"
        );
    }

    /// With no flags, nothing is enabled: the defaults must keep the built-in
    /// behaviour, so adding the flags cannot change an existing invocation.
    #[test]
    fn defaults_are_off_when_no_flag_is_given() {
        let a = parse_get(&["treehouse", "get"]);
        let o = a.acquire_options().unwrap();
        assert!(o.branch.is_none());
        assert!(o.new_branch.is_none());
        assert!(!o.unique_leaf);
        assert!(!o.skip_fetch);
        assert!(o.worktree_path.is_none());
    }

    // ─── seeding and sharing flags ───────────────────────────────────────────

    /// `--include-file` must reach the core as BYTES, and an empty file must
    /// reach it as `Some([])` — not as `None`.
    ///
    /// `None` means "use the committed `.worktreeinclude`". Collapsing an empty
    /// file into it would make `--include-file /dev/null` seed MORE than the
    /// caller asked to exclude, which is the opposite of what the flag says.
    #[test]
    fn an_empty_include_file_is_a_selection_of_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("m.txt");
        std::fs::write(&manifest, b"*.env\n").unwrap();
        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, b"").unwrap();

        let a = parse_get(&[
            "treehouse",
            "get",
            "--include-file",
            manifest.to_str().unwrap(),
        ]);
        assert_eq!(
            a.acquire_options().unwrap().include_manifest,
            Some(b"*.env\n".to_vec())
        );

        let a = parse_get(&[
            "treehouse",
            "get",
            "--include-file",
            empty.to_str().unwrap(),
        ]);
        assert_eq!(
            a.acquire_options().unwrap().include_manifest,
            Some(Vec::new()),
            "an empty manifest selects nothing; it must not fall back to the committed one"
        );
    }

    /// No flag means "use the committed manifest", which is a DIFFERENT value
    /// from an empty manifest and must stay distinguishable all the way down.
    #[test]
    fn no_include_file_leaves_the_manifest_unset() {
        assert_eq!(
            parse_get(&["treehouse", "get"]).acquire_options().unwrap().include_manifest,
            None
        );
    }

    /// The manifest is read during option building, so an unreadable path fails
    /// before acquisition can reset the checkout holding it.
    #[test]
    fn an_unreadable_include_file_is_an_error_not_a_silent_empty_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let a = parse_get(&[
            "treehouse",
            "get",
            "--include-file",
            dir.path().join("nope.txt").to_str().unwrap(),
        ]);
        let err = a.acquire_options().expect_err("a missing manifest must fail");
        assert!(
            err.to_string().contains("failed to read include file"),
            "got: {err}"
        );
    }

    /// `off`/`fresh` are the only accepted values, and only `fresh` opts in.
    /// Anything else is a typo the user needs told about, not a silent default.
    #[test]
    fn apfs_sharing_takes_only_off_or_fresh() {
        let a = parse_get(&["treehouse", "get", "--apfs-sharing", "fresh"]);
        assert!(a.acquire_options().unwrap().apfs_sharing);

        let a = parse_get(&["treehouse", "get", "--apfs-sharing", "off"]);
        assert!(!a.acquire_options().unwrap().apfs_sharing);

        let a = parse_get(&["treehouse", "get", "--apfs-sharing", "sometimes"]);
        let err = a.acquire_options().expect_err("a bogus mode must be refused");
        assert!(
            err.to_string().contains("use off or fresh"),
            "got: {err}"
        );

        assert!(
            !parse_get(&["treehouse", "get"])
                .acquire_options()
                .unwrap()
                .apfs_sharing,
            "sharing must be off unless someone asked for it"
        );
    }

    /// Both new flags must survive into the lease path. `cmd_get` builds one
    /// `AcquireOptions` through this one method for both modes, so a flag
    /// resolved here cannot reach `get` while being dropped by `get --lease`.
    /// The lease/TTL pair is attached by `cmd_get` afterwards, which is why this
    /// asserts on the fields this method owns.
    #[test]
    fn the_seeding_and_sharing_flags_reach_the_lease_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("m.txt");
        std::fs::write(&manifest, b"*.env\n").unwrap();

        let a = parse_get(&[
            "treehouse",
            "get",
            "--lease",
            "--lease-holder",
            "ci",
            "--include-file",
            manifest.to_str().unwrap(),
            "--apfs-sharing",
            "fresh",
        ]);
        let o = a.acquire_options().expect("options must build");
        assert!(a.lease, "the mode under test is the lease path");
        assert_eq!(o.include_manifest, Some(b"*.env\n".to_vec()));
        assert!(o.apfs_sharing);
    }

    // ─── M-020: `--branch` against the jj backend ───────────────────────────

    /// A directory that looks like a jj workspace to the marker reader.
    fn jj_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        dir
    }

    /// A jj workspace in its NORMAL shape: a `.jj` directory COLOCATED with a
    /// git repository, which is what `jj git init` produces.
    ///
    /// This is the case a naive marker read gets wrong. `worktree_backend_name`
    /// is git-first, so it answers `git` here and never looks at `.jj` — a
    /// jj-only directory passes trivially and hides the bug. Every colocated
    /// assertion below exists because that distinction was found by running the
    /// real binary, not by reading the code.
    fn colocated_jj_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        dir
    }

    /// The case that actually broke: a real, colocated jj workspace with the
    /// opt-in set must refuse. Reading the slot flavour instead of the selected
    /// backend reports `git` for this layout and lets the flag through.
    #[test]
    fn branch_is_refused_in_a_colocated_jj_workspace() {
        let dir = colocated_jj_repo();
        let _env = VcsEnv::set("jj");
        let err = require_git_backend_for_branch(dir.path(), Some("feat/x"))
            .expect_err("a colocated jj workspace is still a jj workspace");
        assert!(
            err.to_string().contains("only supported by the git backend"),
            "got: {err}"
        );
    }

    /// The refusal itself, with the opt-in present in the environment.
    ///
    /// `TREEHOUSE_VCS` is process-global, and cargo runs every test in this
    /// binary as a THREAD in one process — so setting it is not a private act.
    /// Two tests that each set it raced, and the loser's assertion was decided
    /// by the winner's value: `a_jj_marker_alone_does_not_refuse_branch` set it
    /// to `""` and intermittently observed a `jj` another test had just set,
    /// failing an assertion that is true. The guard alone cannot fix that,
    /// because restoring a prior value is no help when another thread is
    /// reading it at that instant.
    ///
    /// The lock is what makes the guard sufficient: every test that TOUCHES the
    /// variable holds it for its whole body, so the set and the call it governs
    /// are one atomic region against every other such test. Poisoning is
    /// ignored deliberately — a panic inside one of these should report that one
    /// failure, not cascade into "VCS env lock poisoned" across the rest.
    static VCS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct VcsEnv {
        prior: Option<String>,
        // Held for the guard's lifetime; released on drop.
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl VcsEnv {
        fn set(value: &str) -> VcsEnv {
            let lock = VCS_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let prior = std::env::var(TREEHOUSE_VCS_VAR).ok();
            // SAFETY: the lock makes this the only thread reading or writing the
            // variable for as long as the guard lives.
            unsafe { std::env::set_var(TREEHOUSE_VCS_VAR, value) };
            VcsEnv {
                prior,
                _lock: lock,
            }
        }
    }

    impl Drop for VcsEnv {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => unsafe { std::env::set_var(TREEHOUSE_VCS_VAR, v) },
                None => unsafe { std::env::remove_var(TREEHOUSE_VCS_VAR) },
            }
        }
    }

    #[test]
    fn branch_is_refused_in_a_jj_workspace_that_selected_jj() {
        let dir = jj_workspace();
        let _env = VcsEnv::set("jj");
        let err = require_git_backend_for_branch(dir.path(), Some("feat/x"))
            .expect_err("a git branch cannot be created in a jj workspace");
        assert!(
            err.to_string().contains("only supported by the git backend"),
            "got: {err}"
        );
    }

    /// The opt-in matters: a `.jj` directory alone never selects jj, so a git
    /// repository that merely sits beside one keeps `--branch`. Refusing here
    /// would break an invocation that works today.
    #[test]
    fn a_jj_marker_alone_does_not_refuse_branch() {
        let dir = jj_workspace();
        let _env = VcsEnv::set("");
        assert!(require_git_backend_for_branch(dir.path(), Some("feat/x")).is_ok());
    }

    /// jj selected, but the tree is a git repository. The opt-in without a
    /// marker falls back to git (Go vcs.go:150-154), so `--branch` stands.
    #[test]
    fn jj_selected_without_a_jj_marker_still_allows_branch() {
        let dir = tempfile::tempdir().unwrap();
        let _env = VcsEnv::set("jj");
        assert!(require_git_backend_for_branch(dir.path(), Some("feat/x")).is_ok());
    }

    /// With no `--branch` there is nothing to refuse, whatever the backend.
    #[test]
    fn no_branch_means_no_refusal() {
        let dir = jj_workspace();
        let _env = VcsEnv::set("jj");
        for branch in [None, Some("")] {
            assert!(require_git_backend_for_branch(dir.path(), branch).is_ok());
        }
    }

    /// A damaged marker must surface as an error, never as "not jj". The seam
    /// separates unreadable from absent so a broken checkout is not silently
    /// classified, and the refusal must not undo that.
    #[test]
    fn an_unreadable_jj_marker_is_an_error_not_a_yes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("gone");
        let link = dir.path().join(".jj");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();

        let _env = VcsEnv::set("jj");
        let err = require_git_backend_for_branch(dir.path(), Some("feat/x"))
            .expect_err("an unreadable marker must not read as 'not jj'");
        assert!(
            err.to_string().contains(".jj marker"),
            "got: {err}"
        );
    }

    /// The backend name normaliser is Go's: only `git` and `jj` mean anything,
    /// and a typo is silence rather than a fallback that could drag a jj
    /// repository onto git.
    #[test]
    fn an_unknown_vcs_value_normalises_to_nothing() {
        assert_eq!(normalize_vcs("git"), Some("git"));
        assert_eq!(normalize_vcs("JJ"), Some("jj"));
        assert_eq!(normalize_vcs(" jj "), Some("jj"));
        for bogus in ["", "jjj", "git2", "j"] {
            assert_eq!(normalize_vcs(bogus), None, "{bogus:?} must be unknown");
        }
    }
}
