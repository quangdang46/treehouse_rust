//! treehouse: the CLI binary crate.
//!
//! Thin adapter over `treehouse-core`: parses clap args, invokes the pool, and
//! renders each command's result. Business logic lives in `treehouse-core`.

mod cli;

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
        let lease = pool.acquire_lease(&holder)?;
        match format {
            OutputFormat::Human => {
                println!("{}", lease.path);
            }
            OutputFormat::Json => {
                println!("{}", serde_json::to_string(&lease)?);
            }
            OutputFormat::Toon => {
                // TOON requires the toon dep; fall back to JSON for now.
                println!("{}", serde_json::to_string(&lease)?);
            }
        }
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

    if statuses.is_empty() {
        eprintln!("🌳 No worktrees in pool.");
        return Ok(());
    }

    match format {
        OutputFormat::Human => {
            // Table to stdout.
            for s in &statuses {
                let holder = if s.lease_holder.is_empty() {
                    String::new()
                } else {
                    format!("  (held by {})", s.lease_holder)
                };
                println!("{:<4}  {:<11}  {}{}", s.name, s.status, s.path, holder);
                for p in &s.processes {
                    println!("{:19}{}", "", p);
                }
            }
        }
        OutputFormat::Json => {
            let arr: Vec<serde_json::Value> = statuses
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "path": s.path,
                        "status": s.status,
                        "lease_id": s.lease_id,
                        "lease_holder": s.lease_holder,
                        "leased_at": if s.leased_at == treehouse_core::state::ZERO_TIME { serde_json::Value::Null } else { serde_json::Value::String(s.leased_at.to_rfc3339()) },
                        "processes": s.processes.iter().map(|p| serde_json::json!({"pid": p.pid, "name": p.name})).collect::<Vec<_>>(),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string(&arr)?);
        }
        OutputFormat::Toon => {
            // TOON dep not wired yet; fall back to JSON.
            let arr: Vec<serde_json::Value> = statuses
                .iter()
                .map(|s| serde_json::json!({ "name": s.name, "path": s.path, "status": s.status }))
                .collect();
            println!("{}", serde_json::to_string(&arr)?);
        }
    }
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

    if result.candidates.is_empty() && result.skipped.is_empty() {
        eprintln!("🌳 No stale worktrees to prune.");
        return Ok(());
    }
    if opts.dry_run {
        println!(
            "🌳 Dry run: would prune {} stale worktree(s) and reclaim {}.",
            result.candidates.len(),
            treehouse_core::prune::format_bytes(result.reclaimable_bytes)
        );
        println!("🌳 Re-run with --yes to delete these worktrees.");
        for c in &result.candidates {
            let tag = if c.orphaned { "[orphaned] " } else { "" };
            println!(
                "  {}{} {}",
                tag,
                treehouse_core::prune::format_bytes(c.bytes),
                c.path
            );
        }
    } else {
        println!(
            "🌳 Pruned {} stale worktree(s) and freed {}.",
            result.pruned.len(),
            treehouse_core::prune::format_bytes(result.freed_bytes)
        );
    }
    if !result.skipped.is_empty() {
        eprintln!(
            "🌳 Skipped {} unsafe idle worktree(s):",
            result.skipped.len()
        );
        for s in &result.skipped {
            eprintln!("  [{}] {} ({})", s.category, s.path, s.reason);
        }
    }
    Ok(())
}

/// Destroy worktrees (safe-by-default).
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

    if opts.dry_run {
        println!(
            "🌳 Dry run: would destroy {} worktree(s) in {} and reclaim {}.",
            result.planned.len(),
            result.scope,
            treehouse_core::prune::format_bytes(result.planned_bytes)
        );
        for t in &result.planned {
            println!(
                "  [{}] {} {}",
                t.class,
                treehouse_core::prune::format_bytes(t.bytes),
                t.path
            );
        }
    } else {
        println!(
            "🌳 Destroyed {} worktree(s) in {} and freed {}.",
            result.destroyed.len(),
            result.scope,
            treehouse_core::prune::format_bytes(result.freed_bytes)
        );
    }
    if !result.skipped.is_empty() {
        for s in &result.skipped {
            eprintln!("  [{}] {} ({})", s.detail, s.target.path, s.target.class);
        }
    }
    // Single-target executed with 0 destroyed + a skip -> exit 1.
    if !opts.dry_run && !args.all && result.destroyed.is_empty() && !result.skipped.is_empty() {
        let skip = &result.skipped[0];
        return Err(anyhow!(
            "did not destroy {} ({}); re-run with {}",
            skip.target.name,
            skip.target.class,
            skip.needed_flags.join(", ")
        ));
    }
    Ok(())
}

/// Create a default treehouse.toml.
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
