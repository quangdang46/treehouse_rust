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

    /// Custom pool root directory (overrides treehouse.toml root and default ~/.treehouse).
    #[arg(long = "env-path", value_name = "DIR", global = true)]
    pub env_path: Option<PathBuf>,

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
    /// Show pool status.
    Status(StatusArgs),
    /// Dry-run removal of stale idle worktrees.
    Prune(PruneArgs),
    /// Dry-run removal of worktrees.
    Destroy(DestroyArgs),
    /// Reclaim stale, orphaned, and dead-owner worktrees (dry-run default).
    Gc(GcArgs),
    /// Acquire -> run an agent -> cleanup guaranteed on every exit.
    #[command(external_subcommand)]
    Run(Vec<String>),
    /// Read-only health report.
    Doctor(DoctorArgs),
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
    /// Return only if the current lease has this identity.
    #[arg(long)]
    pub if_lease_id: Option<String>,
    /// Return only if the current lease has this holder.
    #[arg(long)]
    pub if_lease_holder: Option<String>,
    /// The worktree path to return (defaults to $TREEHOUSE_DIR).
    pub path: Option<String>,
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

/// Opens a pool for the current repo context.
pub fn open_pool(ctx: &RepoCtx) -> anyhow::Result<treehouse_core::pool::Pool> {
    let opts = treehouse_core::pool::OpenOptions {
        config: ctx.config.clone(),
        ..Default::default()
    };
    Ok(treehouse_core::pool::Pool::open(
        &ctx.repo_root,
        ctx.remote_url.as_deref(),
        &opts,
    )?)
}

/// Opens a pool with a custom env path.
pub fn open_pool_with_env_path(
    ctx: &RepoCtx,
    env_path: &std::path::Path,
) -> anyhow::Result<treehouse_core::pool::Pool> {
    use std::sync::Arc;
    use treehouse_core::env::DefaultEnv;

    struct CliEnv {
        pool_root: std::path::PathBuf,
    }

    impl treehouse_core::env::TreehouseEnv for CliEnv {
        fn pool_root(&self) -> Option<std::path::PathBuf> {
            Some(self.pool_root.clone())
        }
        fn update_cache_path(&self) -> Option<std::path::PathBuf> {
            DefaultEnv.update_cache_path()
        }
        fn user_config_path(&self) -> Option<std::path::PathBuf> {
            DefaultEnv.user_config_path()
        }
        fn read_file(&self, path: &std::path::Path) -> std::io::Result<String> {
            DefaultEnv.read_file(path)
        }
        fn read_bytes(&self, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
            DefaultEnv.read_bytes(path)
        }
        fn write_file(&self, path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
            DefaultEnv.write_file(path, data)
        }
        fn ensure_dir(&self, path: &std::path::Path) -> std::io::Result<()> {
            DefaultEnv.ensure_dir(path)
        }
        fn path_exists(&self, path: &std::path::Path) -> bool {
            DefaultEnv.path_exists(path)
        }
        fn list_dir(&self, path: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
            DefaultEnv.list_dir(path)
        }
        fn file_meta(
            &self,
            path: &std::path::Path,
        ) -> std::io::Result<treehouse_core::env::FileMeta> {
            DefaultEnv.file_meta(path)
        }
        fn env_var(&self, name: &str) -> Option<String> {
            DefaultEnv.env_var(name)
        }
        fn env_var_os(&self, name: &str) -> Option<std::path::PathBuf> {
            DefaultEnv.env_var_os(name)
        }
        fn cwd(&self) -> Option<std::path::PathBuf> {
            DefaultEnv.cwd()
        }
    }

    let env = CliEnv {
        pool_root: env_path.to_path_buf(),
    };
    let opts = treehouse_core::pool::OpenOptions {
        config: ctx.config.clone(),
        ..Default::default()
    };
    Ok(treehouse_core::pool::Pool::open_with_env(
        &ctx.repo_root,
        ctx.remote_url.as_deref(),
        &opts,
        Arc::new(env),
    )?)
}
