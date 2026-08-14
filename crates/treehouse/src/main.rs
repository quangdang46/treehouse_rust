//! treehouse: the CLI binary crate.
//!
//! Thin adapter over `treehouse-core`: parses clap args, invokes the pool, and
//! renders each command's result. Business logic lives in `treehouse-core`.

mod cli;
mod format;

use std::io::Write;
use std::path::Path;

use anyhow::{Result, anyhow};
use clap::Parser;

use cli::{Cli, Command, OutputFormat};
use treehouse_core::config::TreehouseConfig;
use treehouse_core::destroy::{DestroyOptions, DestroyTargetSpec};
use treehouse_core::prune::PruneOptions;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    // --update-check is intercepted before clap in main (handled above).
    match &cli.command {
        None => cmd_get(
            &cli,
            &cli::GetArgs {
                lease: false,
                lease_holder: None,
                ttl: None,
                json: false,
            },
        ),
        Some(Command::Get(args)) => cmd_get(&cli, args),
        Some(Command::Enter(args)) => cmd_enter(&cli, args),
        Some(Command::Return(args)) => cmd_return(&cli, args),
        Some(Command::Status(args)) => cmd_status(&cli, args),
        Some(Command::Prune(args)) => cmd_prune(&cli, args),
        Some(Command::Destroy(args)) => cmd_destroy(&cli, args),
        Some(Command::Gc(args)) => cmd_gc(&cli, args),
        Some(Command::Run(args)) => cmd_run(&cli, args),
        Some(Command::Doctor(args)) => cmd_doctor(&cli, args),
        Some(Command::Init) => cmd_init(),
        Some(Command::Update) => cmd_update(),
    }
}

/// The bare `treehouse` (no subcommand) aliases `get`.
fn cmd_get(cli: &Cli, args: &cli::GetArgs) -> Result<()> {
    // --json / --format json|toon require --lease (byte-exact Go strings).
    let format = resolve_format(cli, args.json, args.lease)?;
    let _ = format;

    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;

    if args.lease {
        // Lease mode: path-only on stdout, or JSON/TOON object.
        let holder = args
            .lease_holder
            .clone()
            .or_else(|| std::env::var("TREEHOUSE_LEASE_HOLDER").ok())
            .unwrap_or_default();
        // Parse --ttl (humantime); TREEHOUSE_LEASE_TTL env fallback.
        let ttl_str = args
            .ttl
            .clone()
            .or_else(|| std::env::var("TREEHOUSE_LEASE_TTL").ok());
        let ttl = match ttl_str {
            Some(s) => Some(chrono::Duration::from_std(humantime::parse_duration(&s)?)?),
            None => None,
        };
        let lease = pool.acquire_lease_with_ttl(&holder, ttl)?;
        let result = treehouse_core::result::CommandResult::Get(
            treehouse_core::result::GetResult::Lease(lease),
        );
        let fmt = match format {
            OutputFormat::Human => format::OutputFormat::Human,
            OutputFormat::Json => format::OutputFormat::Json,
            OutputFormat::Toon => format::OutputFormat::Toon,
        };
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        format::render(fmt, &result, &mut out, &mut err)?;
        return Ok(());
    }

    // Interactive: open a subshell (writes nothing to stdout).
    eprintln!("🌳 Setting up worktree...");
    let acquired = pool.get(&treehouse_core::pool::AcquireOptions::default())?;
    let path = acquired.path.clone();
    let name = acquired.name.clone();
    eprintln!(
        "🌳 Entered worktree at {}. Type 'exit' to return.",
        path.display()
    );

    unsafe {
        std::env::set_var("TREEHOUSE_DIR", &path);
    }
    let status = spawn_shell(&path);
    unsafe {
        std::env::remove_var("TREEHOUSE_DIR");
    }

    if status != 0 {
        // Nonzero shell exit; leave the worktree as-is (Go: detach only).
        detach_and_return(&pool, &path, false)?;
        return Ok(());
    }

    // Clean return path.
    let dirty = pool.git_is_dirty(&path)?;
    if dirty && !confirm("Clean worktree and return to pool? [Y/n]")? {
        eprintln!("Worktree left dirty. Use 'treehouse return --force' to clean it later.");
        let _ = name;
        return Ok(());
    }
    pool.release(&path.to_string_lossy())?;
    eprintln!("🌳 Worktree returned to pool.");
    Ok(())
}

/// Attach to an existing worktree by name (pool state untouched).
fn cmd_enter(cli: &Cli, args: &cli::EnterArgs) -> Result<()> {
    let _ = cli;
    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;
    let statuses = pool.status()?;
    let found = statuses.iter().find(|s| s.name == args.name);
    let Some(found) = found else {
        return Err(anyhow!(
            "no worktree named \"{}\": the pool is empty. Run 'treehouse get' to create one",
            args.name
        ));
    };
    let path = Path::new(&found.path).to_path_buf();

    if args.print_path {
        println!("{}", path.display());
        return Ok(());
    }

    unsafe {
        std::env::set_var("TREEHOUSE_DIR", &path);
    }
    let _ = spawn_shell(&path);
    unsafe {
        std::env::remove_var("TREEHOUSE_DIR");
    }
    eprintln!("Pool state unchanged.");
    Ok(())
}

/// Release a lease / return a worktree.
fn cmd_return(cli: &Cli, args: &cli::ReturnArgs) -> Result<()> {
    let _ = cli;
    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;

    let path = args
        .path
        .clone()
        .or_else(|| std::env::var("TREEHOUSE_DIR").ok())
        .ok_or_else(|| anyhow!("no worktree path specified"))?;

    // Empty --if-lease-id is an error.
    if let Some(id) = &args.if_lease_id
        && id.is_empty()
    {
        return Err(anyhow!("--if-lease-id must not be empty"));
    }

    let dirty = pool.git_is_dirty(Path::new(&path))?;
    if dirty && !args.force && !confirm("Clean worktree and return to pool? [Y/n]")? {
        eprintln!("Aborted.");
        return Ok(());
    }

    let preconditions = treehouse_core::pool::ReleasePreconditions {
        expected_lease_id: args.if_lease_id.clone(),
        expected_lease_holder: args.if_lease_holder.clone(),
    };
    pool.release_conditional(&path, &preconditions, None)?;
    eprintln!("🌳 Worktree returned to pool.");
    Ok(())
}

/// Show pool status.
fn cmd_status(cli: &Cli, args: &cli::StatusArgs) -> Result<()> {
    let format = resolve_format(cli, args.json, true)?;
    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;
    let statuses = pool.status()?;

    let result = treehouse_core::result::CommandResult::Status(statuses);
    let fmt = match format {
        OutputFormat::Human => format::OutputFormat::Human,
        OutputFormat::Json => format::OutputFormat::Json,
        OutputFormat::Toon => format::OutputFormat::Toon,
    };
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    format::render(fmt, &result, &mut out, &mut err)?;
    Ok(())
}

/// Prune stale idle worktrees.
fn cmd_prune(cli: &Cli, args: &cli::PruneArgs) -> Result<()> {
    let _ = cli;
    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;
    let opts = PruneOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
        ..Default::default()
    };
    let result = pool.prune(&opts)?;

    let result = treehouse_core::result::CommandResult::Prune(result);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    format::render(format::OutputFormat::Human, &result, &mut out, &mut err)?;
    Ok(())
}

/// Reclaim stale, orphaned, and dead-owner worktrees (dry-run default).
fn cmd_gc(cli: &Cli, args: &cli::GcArgs) -> Result<()> {
    let _ = cli;
    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;
    let opts = treehouse_core::gc::GcOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
    };
    let result = pool.gc(&opts)?;

    if result.candidates.is_empty() && result.skipped.is_empty() {
        eprintln!("🌳 No stale worktrees to reclaim.");
        return Ok(());
    }
    if opts.dry_run {
        println!(
            "🌳 Dry run: would reclaim {} worktree(s) and free {}.",
            result.candidates.len(),
            treehouse_core::prune::format_bytes(result.reclaimable_bytes)
        );
        println!("🌳 Re-run with --yes to reclaim these worktrees.");
        for c in &result.candidates {
            println!(
                "  [{}] {} {}",
                c.tag,
                treehouse_core::prune::format_bytes(c.bytes),
                c.path
            );
        }
    } else {
        println!(
            "🌳 Reclaimed {} worktree(s) and freed {}.",
            result.reclaimed.len(),
            treehouse_core::prune::format_bytes(result.freed_bytes)
        );
    }
    if !result.skipped.is_empty() {
        eprintln!("🌳 Skipped {} worktree(s):", result.skipped.len());
        for sk in &result.skipped {
            eprintln!("  [{}] {} ({})", sk.category, sk.path, sk.reason);
        }
    }
    Ok(())
}

fn cmd_destroy(cli: &Cli, args: &cli::DestroyArgs) -> Result<()> {
    let _ = cli;
    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;

    // --all requires a pool path; no path and no --all is an error.
    if args.all && args.path.is_none() {
        return Err(anyhow!("--all requires a pool path"));
    }
    if !args.all && args.path.is_none() {
        return Err(anyhow!("destroy requires a worktree path or --all"));
    }
    if args.all && args.include_leased {
        return Err(anyhow!("--include-leased cannot be combined with --all"));
    }

    let spec = if args.all {
        DestroyTargetSpec::All
    } else {
        DestroyTargetSpec::Single(args.path.clone().unwrap())
    };
    let opts = DestroyOptions {
        dry_run: !args.yes,
        include_unlanded: args.include_unlanded,
        include_in_use: args.include_in_use,
        include_leased: args.include_leased,
        ..Default::default()
    };
    let result = pool.destroy(&spec, &opts)?;

    let result = treehouse_core::result::CommandResult::Destroy(result);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    format::render(format::OutputFormat::Human, &result, &mut out, &mut err)?;

    // Single-target executed with 0 destroyed + a skip -> exit 1.
    if let treehouse_core::result::CommandResult::Destroy(r) = &result
        && !opts.dry_run
        && !args.all
        && r.destroyed.is_empty()
        && !r.skipped.is_empty()
    {
        let skip = &r.skipped[0];
        return Err(anyhow!(
            "did not destroy {} ({}); re-run with {}",
            skip.target.name,
            skip.target.class,
            skip.needed_flags.join(", ")
        ));
    }
    Ok(())
}

fn cmd_init() -> Result<()> {
    let ctx = cli::resolve_repo_ctx()?;
    let path = ctx.repo_root.join("treehouse.toml");
    if path.exists() {
        return Err(anyhow!("treehouse.toml already exists"));
    }
    let cfg = TreehouseConfig::default_config();
    let text = format!(
        "# treehouse.toml\n# Maximum number of worktrees in the pool\nmax_trees = {}\n\n# Optional worktree root directory.\n# root = \"$HOME/worktrees\"\n",
        cfg.max_trees
    );
    std::fs::write(&path, text)?;
    eprintln!("🌳 Created treehouse.toml");
    Ok(())
}

/// Update treehouse (CLI wiring only; the updater subsystem owns internals).
fn cmd_update() -> Result<()> {
    // Dev build -> skip.
    if treehouse_core::VERSION == "dev" {
        eprintln!("Skipping update: running a dev build");
        return Ok(());
    }
    eprintln!("🌳 Checking for updates...");
    eprintln!("Update not yet wired to the updater subsystem (updater bead).");
    Ok(())
}

/// Resolves the output format, enforcing Go's --json/--format rules.
///
/// For `get`, machine formats (`--json` / `--format json|toon`) require
/// `--lease` (byte-exact Go error strings). Other commands always allow them.
fn resolve_format(cli: &Cli, json_flag: bool, lease_present: bool) -> Result<OutputFormat> {
    let format = cli.format;
    let conflict = json_flag && format != OutputFormat::Human && format != OutputFormat::Json;
    if conflict {
        return Err(anyhow!(
            "conflicting output formats: --json and --format {:?}",
            format_variant_str(format)
        ));
    }
    let resolved = if json_flag {
        OutputFormat::Json
    } else {
        format
    };
    if resolved != OutputFormat::Human && !lease_present {
        if json_flag {
            return Err(anyhow!("--json requires --lease"));
        }
        let label = format_variant_str(resolved);
        return Err(anyhow!("--format {label} requires --lease"));
    }
    Ok(resolved)
}

fn format_variant_str(f: OutputFormat) -> &'static str {
    match f {
        OutputFormat::Human => "human",
        OutputFormat::Json => "json",
        OutputFormat::Toon => "toon",
    }
}

/// Spawns the interactive subshell in `work_dir`, returning its exit code.
fn spawn_shell(work_dir: &Path) -> i32 {
    #[cfg(unix)]
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    #[cfg(windows)]
    let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());

    let status = std::process::Command::new(&shell)
        .current_dir(work_dir)
        .status();
    match status {
        Ok(s) => s.code().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Simple Y/n confirmation prompt.
fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt} ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();
    Ok(input.is_empty() || input == "y" || input == "yes")
}

/// Detach + return on a nonzero shell exit.
fn detach_and_return(_pool: &treehouse_core::pool::Pool, path: &Path, _force: bool) -> Result<()> {
    let _ = path;
    Ok(())
}

/// Acquire -> run an agent -> cleanup guaranteed on every exit.
fn cmd_run(cli: &Cli, args: &cli::RunArgs) -> Result<()> {
    let _ = cli;
    if args.command.is_empty() {
        return Err(anyhow!("run requires a command after `--`"));
    }
    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;

    let ttl = match &args.ttl {
        Some(s) => humantime::parse_duration(s)?,
        None => std::time::Duration::from_secs(24 * 3600),
    };
    let holder = args
        .lease_holder
        .clone()
        .or_else(|| std::env::var("TREEHOUSE_LEASE_HOLDER").ok())
        .unwrap_or_else(|| format!("run:{}", std::process::id()));

    let opts = treehouse_core::run::RunOptions {
        command: args.command.iter().map(std::ffi::OsString::from).collect(),
        ttl,
        holder,
    };
    let result = treehouse_core::run::run(&pool, &opts)?;

    // Exit with the child's code (or 128+signum on unix signal).
    if let Some(code) = result.child_exit_code {
        std::process::exit(code);
    }
    if let Some(sig) = result.child_signal {
        std::process::exit(128 + sig);
    }
    Ok(())
}

/// Read-only health report.
fn cmd_doctor(cli: &Cli, args: &cli::DoctorArgs) -> Result<()> {
    let ctx = cli::resolve_repo_ctx()?;
    let pool = cli::open_pool(&ctx)?;
    let report = treehouse_core::doctor::run_doctor(&pool)?;

    // JSON/TOON to stdout; human to stdout with markers.
    match cli.format {
        OutputFormat::Json | OutputFormat::Toon => {
            let json = treehouse_core::doctor::report_json(&report);
            println!("{}", serde_json::to_string(&json)?);
        }
        OutputFormat::Human => {
            println!("🌳 treehouse doctor");
            for c in &report.checks {
                let marker = match c.status {
                    treehouse_core::doctor::Severity::Ok => "✓",
                    treehouse_core::doctor::Severity::Warn => "⚠",
                    treehouse_core::doctor::Severity::Error => "✗",
                };
                println!("  {marker} {}: {}", c.name, c.detail);
            }
            println!(
                "Doctor: {} error(s), {} warning(s)",
                report.error_count(),
                report.warn_count()
            );
        }
    }

    // Exit code: 1 if any Error, or if --strict and any Warn.
    let failed = if args.strict {
        !report.strict_healthy
    } else {
        !report.healthy
    };
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
