//! The clap CLI surface for treehouse.
//!
//! Each subcommand calls into `treehouse-core` and renders the result via the
//! formatter. The Go CLI contract (plan Appendix B) is reproduced: exit codes,
//! stdout/stderr routing, and human message strings (byte-exact where Go-tested).

use std::path::PathBuf;

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

#[derive(Debug, Args)]
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
            ..Default::default()
        })
    }
}

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
}
