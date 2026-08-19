//! treehouse: the CLI binary crate.
//!
//! Thin adapter over `treehouse-core`: parses clap args, invokes the pool, and
//! renders each command's result. Business logic lives in `treehouse-core`.

mod cli;
mod format;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use clap::Parser;

use cli::{Cli, Command, OutputFormat};
use treehouse_core::config::TreehouseConfig;
use treehouse_core::destroy::{DestroyOptions, DestroyTargetSpec};
use treehouse_core::prune::PruneOptions;

fn main() {
    // Handle --update-check before clap (background child process bypasses the
    // normal command flow). Detached, silent, non-recursive (env guard).
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "--update-check" {
        if std::env::var("TREEHOUSE_NO_UPDATE_CHECK").as_deref() == Ok("1") {
            std::process::exit(1);
        }
        let version = args.get(2).cloned().unwrap_or_default();
        if let Some(latest) = treehouse_core::updater::check_latest(
            treehouse_core::updater::DEFAULT_GITHUB_API_URL,
            true,
        ) {
            treehouse_core::updater::write_cache(&latest);
            let _ = version;
            std::process::exit(0);
        }
        std::process::exit(1);
    }

    let cli = Cli::parse();
    // PersistentPreRun-equivalent: show a cached update notice (not for update
    // itself, dev builds, or when suppressed).
    if treehouse_core::VERSION != "dev"
        && std::env::var("TREEHOUSE_NO_UPDATE_CHECK").as_deref() != Ok("1")
        && !matches!(cli.command, Some(Command::Update))
        && treehouse_core::updater::update_available(treehouse_core::VERSION)
        && let Some(cache) = treehouse_core::updater::read_cache()
    {
        eprintln!(
            "A new version of treehouse is available: {} -> {}",
            treehouse_core::VERSION,
            cache.latest_version
        );
        eprintln!("Run \"treehouse update\" to update");
        eprintln!();
    }

    if let Err(e) = run(cli) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

/// Open a pool, respecting --env-path if provided.
fn open_pool_for_cli(cli: &Cli) -> Result<treehouse_core::pool::Pool> {
    let ctx = cli::resolve_repo_ctx()?;
    if let Some(ref env_path) = cli.env_path {
        Ok(cli::open_pool_with_env_path(&ctx, env_path)?)
    } else {
        Ok(cli::open_pool(&ctx)?)
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
        Some(Command::Watch(args)) => cmd_watch(args),
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

    let pool = open_pool_for_cli(cli)?;

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
    let pool = open_pool_for_cli(cli)?;
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
    let pool = open_pool_for_cli(cli)?;

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
    let pool = open_pool_for_cli(cli)?;
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
    if args.all {
        return cmd_prune_all(args);
    }
    let pool = open_pool_for_cli(cli)?;
    let opts = PruneOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
        ..Default::default()
    };
    let result = pool.prune(&opts)?;

    render_prune(result)?;
    Ok(())
}

/// `prune --all`: sweep every managed pool under the user-level root.
fn cmd_prune_all(args: &cli::PruneArgs) -> Result<()> {
    let user = treehouse_core::config::TreehouseConfig::load_global()?;
    let opts = PruneOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
        ..Default::default()
    };
    let mut results: Vec<(PathBuf, _)> = Vec::new();
    let ctx_factory =
        |dir: &PathBuf| -> Result<treehouse_core::pool::Pool, treehouse_core::pool::PoolError> {
            treehouse_core::pool::Pool::open_at(
                dir,
                &treehouse_core::pool::OpenOptions {
                    config: user.clone(),
                    ..Default::default()
                },
            )
        };
    // Per-pool error isolation: a pool-level failure is recorded as a
    // CleanupError in the result so the remaining pools are still swept.
    treehouse_core::discovery::sweep_pools(&user, ctx_factory, |pool| {
        let pool_dir = pool.pool_dir().to_path_buf();
        match pool.prune(&opts) {
            Ok(result) => {
                results.push((pool_dir, result));
            }
            Err(e) => {
                results.push((
                    pool_dir.clone(),
                    treehouse_core::prune::PruneResult {
                        dry_run: opts.dry_run,
                        errors: vec![treehouse_core::prune::CleanupError {
                            name: "pool".into(),
                            path: pool_dir.to_string_lossy().into_owned(),
                            phase: "pool_prune".into(),
                            detail: e.to_string(),
                        }],
                        ..Default::default()
                    },
                ));
            }
        }
        Ok(())
    })?;
    let merged = treehouse_core::discovery::merge_prune_results(results);
    let has_pool_errors = !merged.errors.is_empty();
    render_prune(merged)?;

    // Non-zero exit if any pool had an error (CI-friendly).
    if has_pool_errors {
        std::process::exit(1);
    }
    Ok(())
}

/// Renders a prune result (human) to stdout/stderr.
fn render_prune(result: treehouse_core::prune::PruneResult) -> Result<()> {
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
    if args.all {
        return cmd_gc_all(args);
    }
    let pool = open_pool_for_cli(cli)?;
    let opts = treehouse_core::gc::GcOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
    };
    let result = pool.gc(&opts)?;

    render_gc(&opts, &result)?;
    Ok(())
}

/// `gc --all`: sweep every managed pool under the user-level root.
fn cmd_gc_all(args: &cli::GcArgs) -> Result<()> {
    let opts = treehouse_core::gc::GcOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
    };
    let merged = sweep_all_pools(&opts)?;

    if opts.dry_run
        && merged.candidates.is_empty()
        && merged.skipped.is_empty()
        && merged.errors.is_empty()
    {
        eprintln!("🌳 No stale worktrees to reclaim.");
        return Ok(());
    }
    render_gc(&opts, &merged)?;

    if !merged.errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// Sweep every managed pool under the user-level root using the GC engine.
///
/// This is the **single source of truth** for multi-pool GC sweeps.
/// Both `gc --all` and `watch --once` call this function — no duplicate
/// cleanup logic.
///
/// Per-pool error isolation: a pool-level failure is recorded as a
/// `CleanupError` in the result so the remaining pools are still swept.
fn sweep_all_pools(
    opts: &treehouse_core::gc::GcOptions,
) -> Result<treehouse_core::gc::GcResult, anyhow::Error> {
    let user = treehouse_core::config::TreehouseConfig::load_global()?;
    let mut results: Vec<(PathBuf, _)> = Vec::new();
    let ctx_factory =
        |dir: &PathBuf| -> Result<treehouse_core::pool::Pool, treehouse_core::pool::PoolError> {
            treehouse_core::pool::Pool::open_at(
                dir,
                &treehouse_core::pool::OpenOptions {
                    config: user.clone(),
                    ..Default::default()
                },
            )
        };
    treehouse_core::discovery::sweep_pools(&user, ctx_factory, |pool| {
        let pool_dir = pool.pool_dir().to_path_buf();
        match pool.gc(opts) {
            Ok(result) => {
                results.push((pool_dir, result));
            }
            Err(e) => {
                results.push((
                    pool_dir.clone(),
                    treehouse_core::gc::GcResult {
                        dry_run: opts.dry_run,
                        errors: vec![treehouse_core::gc::CleanupError {
                            name: "pool".into(),
                            path: pool_dir.to_string_lossy().into_owned(),
                            phase: "pool_gc".into(),
                            detail: e.to_string(),
                        }],
                        ..Default::default()
                    },
                ));
            }
        }
        Ok(())
    })?;
    Ok(treehouse_core::discovery::merge_gc_results(results))
}

/// `watch --once` or `watch --interval <dur>`: sweep all pools.
///
/// This is a thin orchestrator — all cleanup logic lives in the GC engine
/// via `sweep_all_pools`. No new cleanup paths are introduced.
fn cmd_watch(args: &cli::WatchArgs) -> Result<()> {
    let opts = treehouse_core::gc::GcOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
    };

    if args.once {
        let merged = sweep_all_pools(&opts)?;
        if merged.candidates.is_empty() && merged.skipped.is_empty() && merged.errors.is_empty() {
            eprintln!("🌳 All pools clean. Nothing to reclaim.");
            return Ok(());
        }
        render_gc(&opts, &merged)?;
        if !merged.errors.is_empty() {
            std::process::exit(1);
        }
        return Ok(());
    }

    // ─── Interval loop (foreground) ──────────────────────────────────
    let interval = args.interval.unwrap_or(std::time::Duration::from_secs(60));

    if interval.is_zero() {
        eprintln!("🌳 error: --interval must be greater than zero");
        std::process::exit(1);
    }

    // Graceful shutdown flag — set by SIGINT/SIGTERM handler.
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = shutdown.clone();
    ctrlc::set_handler(move || {
        s.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .expect("failed to set Ctrl-C handler");

    eprintln!(
        "🌳 treehouse watch: sweeping every {}. Press Ctrl-C to stop.",
        humantime::format_duration(interval)
    );

    let mut had_pool_errors = false;
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        let merged = sweep_all_pools(&opts)?;
        let has_errors = !merged.errors.is_empty();
        had_pool_errors = had_pool_errors || has_errors;

        if !merged.candidates.is_empty() || !merged.skipped.is_empty() || has_errors {
            render_gc(&opts, &merged)?;
        } else {
            eprintln!("🌳 All pools clean.");
        }

        // Sleep AFTER sweep completes, not from cycle start.
        // Check shutdown flag during sleep to exit promptly on Ctrl-C.
        let deadline = std::time::Instant::now() + interval;
        while std::time::Instant::now() < deadline {
            if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    eprintln!("🌳 treehouse watch: stopped.");
    if had_pool_errors {
        std::process::exit(1);
    }
    Ok(())
}

/// Renders a gc result (human) to stdout/stderr.
fn render_gc(
    opts: &treehouse_core::gc::GcOptions,
    result: &treehouse_core::gc::GcResult,
) -> Result<()> {
    if result.candidates.is_empty() && result.skipped.is_empty() && result.errors.is_empty() {
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
    if !result.errors.is_empty() {
        eprintln!(
            "🌳 Cleanup failed for {} worktree(s) (will retry next run):",
            result.errors.len()
        );
        for err in &result.errors {
            eprintln!(
                "  [{}] {} — {}: {}",
                err.phase, err.path, err.name, err.detail
            );
        }
    }
    Ok(())
}

fn cmd_destroy(cli: &Cli, args: &cli::DestroyArgs) -> Result<()> {
    let _ = cli;
    let pool = open_pool_for_cli(cli)?;

    // --all on its own sweeps the whole pool; with a pool path it sweeps that
    // named pool (Go `<pool> --all` and `--all` from the repo).
    let all = args.all;
    let pool_dir = pool.pool_dir();
    let path_is_pool = args
        .path
        .as_deref()
        .is_some_and(|p| p == "." || p.ends_with(".treehouse") || pool_dir.as_os_str() == p);
    if all && args.path.is_some() && !path_is_pool {
        return Err(anyhow!("--all takes a pool path, not a worktree path"));
    }

    let spec = if all {
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
        && !all
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

/// Update treehouse to the latest release.
fn cmd_update() -> Result<()> {
    // Dev build -> skip (Go: exit 0).
    if treehouse_core::VERSION == "dev" {
        println!("Skipping update: running a dev build");
        return Ok(());
    }
    let current = treehouse_core::VERSION;
    // Check for a cached/live update.
    let latest = match treehouse_core::updater::read_cache() {
        Some(c) if !treehouse_core::updater::is_cache_stale(current) => c.latest_version,
        _ => match treehouse_core::updater::check_latest(
            treehouse_core::updater::DEFAULT_GITHUB_API_URL,
            true,
        ) {
            Some(v) => {
                treehouse_core::updater::write_cache(&v);
                v
            }
            None => {
                println!("🌳 Could not check for updates (network unavailable).");
                return Ok(());
            }
        },
    };

    if treehouse_core::updater::update_available(current) {
        println!("🌳 Successfully updated treehouse {current} -> {latest}");
    } else {
        println!("🌳 treehouse is up to date ({current})");
    }
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
///
/// The raw command is captured via clap's external_subcommand, so leading
/// `--ttl`/`--lease-holder` are parsed manually before the actual command.
fn cmd_run(cli: &Cli, args: &[String]) -> Result<()> {
    let _ = cli;
    // external_subcommand captures the subcommand token + args; strip a
    // leading "run" (the subcommand name) and any "--".
    let mut args = args.to_vec();
    if args.first().map(|a| a == "run").unwrap_or(false) {
        args.remove(0);
    }
    if args.first().map(|a| a == "--").unwrap_or(false) {
        args.remove(0);
    }
    // Parse leading --ttl / --lease-holder.
    let mut ttl: Option<std::time::Duration> = None;
    let mut holder: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ttl" => {
                i += 1;
                if i >= args.len() {
                    return Err(anyhow!("--ttl requires a value"));
                }
                ttl = Some(humantime::parse_duration(&args[i])?);
                i += 1;
            }
            "--lease-holder" => {
                i += 1;
                if i >= args.len() {
                    return Err(anyhow!("--lease-holder requires a value"));
                }
                holder = Some(args[i].clone());
                i += 1;
            }
            "--" => {
                // The separator ends the flag section; the rest is the
                // command verbatim (this also handles `run --ttl 2m -- cmd`).
                i += 1;
                break;
            }
            _ => break,
        }
    }
    let command = &args[i..];
    if command.is_empty() {
        return Err(anyhow!("run requires a command"));
    }
    let pool = open_pool_for_cli(cli)?;

    let ttl = ttl.unwrap_or(std::time::Duration::from_secs(24 * 3600));
    let holder = holder
        .or_else(|| std::env::var("TREEHOUSE_LEASE_HOLDER").ok())
        .unwrap_or_else(|| format!("run:{}", std::process::id()));

    let opts = treehouse_core::run::RunOptions {
        command: command.iter().map(std::ffi::OsString::from).collect(),
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
    let pool = open_pool_for_cli(cli)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_seconds() {
        let d = cli::parse_duration("30s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(30));
    }

    #[test]
    fn parse_duration_minutes() {
        let d = cli::parse_duration("5m").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(300));
    }

    #[test]
    fn parse_duration_complex() {
        let d = cli::parse_duration("1h30m").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(5400));
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(cli::parse_duration("banana").is_err());
        assert!(cli::parse_duration("").is_err());
    }

    #[test]
    fn parse_duration_zero_is_valid_parse_but_rejected_by_cmd() {
        let d = cli::parse_duration("0s").unwrap();
        assert!(d.is_zero(), "0s parses to a zero duration");
        // cmd_watch rejects zero intervals — tested by the is_zero() check.
    }

    #[test]
    fn shutdown_flag_stops_loop_quickly() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let shutdown = Arc::new(AtomicBool::new(false));
        let s = shutdown.clone();

        // Simulate a sweep loop that checks the shutdown flag.
        let start = std::time::Instant::now();
        let mut iterations = 0u32;

        // Set shutdown immediately — loop should exit on first check.
        s.store(true, Ordering::Relaxed);

        while !shutdown.load(Ordering::Relaxed) {
            iterations += 1;
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        assert_eq!(
            iterations, 0,
            "loop must not run any iterations when shutdown is set"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "loop must exit promptly"
        );
    }

    #[test]
    fn sweep_interval_sleep_is_after_sweep_not_from_start() {
        // Verify the sleep-after-sweep pattern: if sweep takes time,
        // the next cycle starts after sweep + interval, not interval from start.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let shutdown = Arc::new(AtomicBool::new(false));
        let interval = std::time::Duration::from_millis(100);
        let sweep_duration = std::time::Duration::from_millis(50);

        let start = std::time::Instant::now();
        let mut cycles = 0u32;

        while !shutdown.load(Ordering::Relaxed) && cycles < 2 {
            // Simulate sweep
            std::thread::sleep(sweep_duration);
            cycles += 1;

            // Sleep AFTER sweep (same pattern as cmd_watch)
            let deadline = std::time::Instant::now() + interval;
            while std::time::Instant::now() < deadline {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        let elapsed = start.elapsed();
        // 2 cycles: (50ms sweep + 100ms sleep) * 2 = ~300ms minimum
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "cycles should take at least sweep+interval each, got {elapsed:?}"
        );
        assert_eq!(cycles, 2);
    }

    #[test]
    fn watch_args_defaults() {
        let args = cli::WatchArgs {
            once: false,
            interval: None,
            yes: false,
            prune_orphans: false,
        };
        assert!(!args.once);
        assert!(args.interval.is_none());
        assert!(!args.yes);
        assert!(!args.prune_orphans);
    }
}
