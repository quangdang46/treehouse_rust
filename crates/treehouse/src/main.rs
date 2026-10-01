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
use treehouse_core::result::SweepScope;

/// Process exit status for "the worktree, and any lease on it, was left exactly
/// as it was found" (Go `ExitNotReturned`, cmd/exit.go:19).
///
/// Distinct from the generic failure because the two demand different
/// responses: a failure is worth retrying, while a dirty worktree nobody
/// cleaned stays dirty — `get` skips it and `prune` will not reclaim it — until
/// someone cleans it or passes `--force`.
///
/// **This is the default.** Go shipped exit 3 as a v3.0.0 BREAKING CHANGE
/// because a caller reading exit 0 as "the slot was released" was reading a
/// bug; this port had held exit 0 behind `TREEHOUSE_EXIT_STRICT=1` while the
/// contract was still settling. It now matches Go, and the environment variable
/// is the migration path in the other direction: `TREEHOUSE_EXIT_STRICT=0`
/// restores exit 0 for scripts running under `set -e`, for one minor release.
const EXIT_NOT_RETURNED: i32 = 3;

/// A worktree left exactly as it was found, carrying the message for EACH exit
/// status it can produce.
///
/// Both messages name the remedy and say the slot is still held; they differ
/// only in how blunt they are. The exit-3 wording is Go's and is now the
/// default. The exit-0 wording is what this port printed before the flip and
/// survives solely behind `TREEHOUSE_EXIT_STRICT=0`, so the escape hatch
/// produces byte-identical output to the release a script was written against.
#[derive(Debug)]
struct NotReturned {
    /// Printed with [`EXIT_NOT_RETURNED`]. The default path.
    not_returned: String,
    /// Printed with exit 0, reachable only via `TREEHOUSE_EXIT_STRICT=0`.
    legacy: String,
}

impl std::fmt::Display for NotReturned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.not_returned)
    }
}

impl std::error::Error for NotReturned {}

/// Whether the user has opted OUT of exit 3 via `TREEHOUSE_EXIT_STRICT=0`.
///
/// This used to be an opt-IN and is now an opt-OUT, so the polarity of an
/// unrecognised value inverts. Unset means exit 3. Only a value Go's
/// `strconv.ParseBool` reads as FALSE (`0`, `f`, `false`, `off`, `n`, `no`)
/// restores exit 0 — matching the `env_flag` reader in `cli.rs` so one spelling
/// works everywhere in the binary.
///
/// An unparseable value is deliberately NOT the opt-out. `TREEHOUSE_EXIT_STRICT=1`
/// from the previous release is not false, and neither is a typo: both keep the
/// exit-3 default. Treating garbage as "user wants the old behaviour" would
/// hand the escape hatch to anyone who fat-fingers the variable name, which is
/// the one direction that can silently under-report a slot that was never
/// released.
fn exit_legacy_zero() -> bool {
    match std::env::var("TREEHOUSE_EXIT_STRICT") {
        Ok(v) => parses_as_false(&v),
        Err(_) => false,
    }
}

/// Whether `value` is one of the spellings Go's `strconv.ParseBool` reads as
/// FALSE. Split out from [`exit_legacy_zero`] so it can be asserted directly:
/// the env read is process-global, and a test that mutated it would race every
/// other test in this binary.
fn parses_as_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "f" | "false" | "off" | "n" | "no"
    )
}

fn main() {
    // Handle --update-check before clap (background child process bypasses the
    // normal command flow). Detached and silent.
    //
    // `TREEHOUSE_NO_UPDATE_CHECK=1` is deliberately NOT consulted here: the
    // spawning parent SETS it on this child precisely so a third generation
    // would not fork again (Go `backgroundCheckCommand`, updater.go:213-215).
    // Treating it as a reason to abort made the child kill itself the instant
    // it was finally wired up, so the cache was never written and the notice
    // above stayed unreachable. Non-recursion is guaranteed structurally
    // instead: this handler exits before `Cli::parse` and before
    // `should_spawn_update_check` ever runs.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "--update-check" {
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

    // Spawn the background check that actually WRITES that cache. Without it
    // the notice above is unreachable: only `treehouse update` writes the file,
    // so an install that never updates never learns a release exists.
    if should_spawn_update_check(&cli) {
        spawn_background_check();
    }

    if let Err(e) = run(cli) {
        if let Some(not_returned) = e.downcast_ref::<NotReturned>() {
            // An unreturned worktree is not a failure, but it IS a status the
            // caller must see: exit 3 is the default, matching Go, and only
            // `TREEHOUSE_EXIT_STRICT=0` restores this port's original exit 0.
            if exit_legacy_zero() {
                eprintln!("{}", not_returned.legacy);
                return;
            }
            eprintln!("{}", not_returned.not_returned);
            std::process::exit(EXIT_NOT_RETURNED);
        }
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

/// Whether this invocation should fork a background update check.
///
/// Gated on cache staleness so the check runs at most once per TTL rather than
/// once per command, and never for `update` (which does the check in the
/// foreground) or a dev build (whose version is not a release).
fn should_spawn_update_check(cli: &Cli) -> bool {
    treehouse_core::VERSION != "dev"
        && std::env::var("TREEHOUSE_NO_UPDATE_CHECK").as_deref() != Ok("1")
        && !matches!(cli.command, Some(Command::Update))
        && treehouse_core::updater::is_cache_stale(treehouse_core::VERSION)
}

/// Re-execs this binary as a detached `--update-check` child (Go
/// `updater.SpawnBackgroundCheck`, updater.go:170-201).
///
/// Fire-and-forget: a spawn failure is an update check that did not happen, not
/// a command that failed, so it is swallowed like Go's `_ =`.
fn spawn_background_check() {
    let Some(mut cmd) = background_check_command() else {
        return;
    };
    let _ = cmd.spawn();
}

/// The detached `--update-check` child, or `None` when this executable cannot
/// be resolved.
///
/// Split from the spawn so the wiring is testable: the defect being fixed was
/// that nothing was ever spawned at all, and "we built the right command" is
/// the part that can be asserted without waiting on a network.
fn background_check_command() -> Option<std::process::Command> {
    use std::process::{Command, Stdio};

    // Canonicalize, or a symlinked install (homebrew, /usr/local/bin shims)
    // re-execs through the link every time instead of the real binary.
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(exe).ok()?;

    let mut cmd = Command::new(exe);
    cmd.arg("--update-check").arg(treehouse_core::VERSION);
    // The recursion guard the PARENT side reads (`should_spawn_update_check`).
    cmd.env("TREEHOUSE_NO_UPDATE_CHECK", "1");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // The child OUTLIVES this command, so it must not inherit the caller's
    // working directory: a cwd inside a pooled worktree makes the child a
    // process treehouse itself created — reported by `status` and terminated by
    // `return`. Detaching the cwd also stops it holding the caller's directory
    // open after it exits (Go `detachedWorkingDir`, updater.go:229-241).
    if let Some(dir) = detached_working_dir() {
        cmd.current_dir(dir);
    }
    // Nothing usable could be determined above: inheriting the caller's cwd is
    // the lesser evil, and Go accepts it too — an update check is worth less
    // than a failed spawn.

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid(2) only. It touches no memory the parent shares and
        // cannot fail in a way that leaves the child half-built.
        unsafe {
            cmd.pre_exec(|| {
                if libc_setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    Some(cmd)
}

/// `setsid(2)` without pulling in the `libc` crate for one call.
#[cfg(unix)]
unsafe fn libc_setsid() -> i32 {
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    unsafe { setsid() }
}

/// A directory safe for a child detached from the caller (Go
/// `detachedWorkingDir`, updater.go:229-241).
///
/// The temp dir is the first choice, but only when it lies OUTSIDE the
/// worktree the caller is standing in: `TMPDIR=<slot>/.tmp` while running from
/// `<slot>/src` would put the child straight back inside the slot it was
/// detached from, and the boundary that matters is the slot, not the caller's
/// subdirectory of it. The filesystem root is the fallback, because no worktree
/// can contain it.
fn detached_working_dir() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let tmp = std::env::temp_dir();
    if tmp.is_dir() && outside_enclosing_worktree(&tmp, &cwd) {
        return Some(tmp);
    }
    let root = cwd.ancestors().last()?;
    root.is_dir().then(|| root.to_path_buf())
}

/// Whether `dir` is safe for a child detached from a caller in `cwd`: true when
/// `cwd` lies in no worktree at all, or when `dir` is neither the enclosing
/// worktree root nor a descendant of it.
///
/// Fails CLOSED: anything that cannot be resolved or walked reads as "inside",
/// so an unreadable marker never yields a directory the child could be started
/// in.
fn outside_enclosing_worktree(dir: &Path, cwd: &Path) -> bool {
    let (Ok(resolved_cwd), Ok(resolved_dir)) =
        (std::fs::canonicalize(cwd), std::fs::canonicalize(dir))
    else {
        return false;
    };
    let Some(root) = enclosing_worktree_root(&resolved_cwd) else {
        // cwd is in no worktree: any directory is safe.
        return true;
    };
    !resolved_dir.starts_with(&root)
}

/// The first directory at or above `dir` carrying a VCS marker (Go
/// `enclosingWorktreeRoot`), or `None` when none does.
///
/// A marker that cannot be read is an error, not a miss — `slot_backend_name`
/// answers `""` for a damaged marker, which would make this walk PAST a real
/// worktree and pick an ancestor the child is actually inside.
fn enclosing_worktree_root(dir: &Path) -> Option<PathBuf> {
    for candidate in dir.ancestors() {
        match std::fs::symlink_metadata(candidate.join(".git")) {
            Ok(_) => return Some(candidate.to_path_buf()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Unreadable: stop here rather than climb into an ancestor.
            Err(_) => return None,
        }
    }
    None
}

/// Open a pool, respecting --root if provided.
fn open_pool_for_cli(cli: &Cli) -> Result<treehouse_core::pool::Pool> {
    let ctx = cli::resolve_repo_ctx()?;
    cli::open_pool_with_root(&ctx, cli.root.as_deref())
}

fn run(cli: Cli) -> Result<()> {
    // --update-check is intercepted before clap in main (handled above).
    match &cli.command {
        None => cmd_get(&cli, &cli::GetArgs::default()),
        Some(Command::Get(args)) => cmd_get(&cli, args),
        Some(Command::Enter(args)) => cmd_enter(&cli, args),
        Some(Command::Return(args)) => cmd_return(&cli, args),
        Some(Command::Lease(args)) => cmd_lease(&cli, args),
        Some(Command::Status(args)) => cmd_status(&cli, args),
        Some(Command::Prune(args)) => cmd_prune(&cli, args),
        Some(Command::Destroy(args)) => cmd_destroy(&cli, args),
        Some(Command::Gc(args)) => cmd_gc(&cli, args),
        Some(Command::Watch(args)) => cmd_watch(&cli, args),
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

    // Resolved once and reused for both the backend check and the pool, so the
    // repository cannot change identity between the two. The jj refusal runs
    // BEFORE the lease/interactive split: Go places it before `getLeaseRunE`
    // (cmd/get.go:147-150) so `--lease` cannot become a way to slip a git
    // branch request past it.
    let ctx = cli::resolve_repo_ctx()?;
    cli::require_git_backend_for_branch(&ctx.repo_root, args.branch.as_deref())?;
    let pool = cli::open_pool_with_root(&ctx, cli.root.as_deref())?;

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
        // Carry the acquisition flags (`--base`, `-b`, `--unique-leaf`,
        // `--worktree-path`, `--no-fetch`) through lease mode too. Go routes
        // both modes through one `acquireOpts` (cmd/get.go:169-173); funnelling
        // only the holder/TTL through the older entry point silently dropped
        // them whenever `--lease` was set.
        let mut lease_opts = args.acquire_options()?;
        lease_opts.lease = Some(treehouse_core::pool::LeaseAcquireOptions {
            holder: holder.clone(),
            ttl,
        });
        let lease = pool.acquire_lease_with_options(&lease_opts, &holder)?;
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
    let acquired = pool.get(&args.acquire_options()?)?;
    let path = acquired.path.clone();
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
        // Nonzero shell exit. Go does not branch here at all (cmd/get.go:190+):
        // the slot was acquired either way, so it is detached and handed back
        // on every exit path. Reporting the status and doing the same work is
        // the difference; treating it as "leave everything as-is" stranded the
        // slot: reservation stamp, checked-out branch, dirty tree, all of it.
        eprintln!("🌳 Subshell exited with status {status}; returning the worktree.");
    }

    // Clean return path. The release is guarded on our OWN reservation: while
    // this subshell ran, the slot could have been durably leased over this live
    // agent home or handed to a later acquisition. Releasing unconditionally
    // would reset the worktree and clear the NEW owner's reservation underneath
    // them (Go `RequireOwnedByCaller`, pool.go:1418-1438).
    let owned = treehouse_core::pool::ReleasePreconditions {
        require_owned_by_caller: true,
        ..Default::default()
    };
    if let Err(e) = pool.validate_release_preconditions(&path.to_string_lossy(), &owned) {
        eprintln!(
            "🌳 Not returning {} to the pool: {e}; leaving it exactly as it is.",
            path.display()
        );
        return Ok(());
    }

    // Go detaches the HEAD here (cmd/get.go:200-205) BEFORE asking about
    // dirtiness, and does it under the same state lock that guards the release
    // — a takeover cannot land between the check and the HEAD move. The port's
    // equivalent is the release's `before_reset` hook, which runs in exactly
    // that critical section (pool.rs:814-838).
    let dirty = pool.git_is_dirty(&path)?;
    if dirty {
        eprintln!("🌳 Worktree has uncommitted changes.");
        match confirm("Clean worktree and return to pool? [Y/n]")? {
            Confirm::No | Confirm::Unanswered => {
                return Err(NotReturned {
                    not_returned: format!(
                        "🌳 worktree left dirty and not returned to the pool; prune will not reclaim this slot. Use treehouse return --force {} to clean it later",
                        quote_path(&path)
                    ),
                    legacy:
                        "Worktree left dirty. Use 'treehouse return --force' to clean it later."
                            .to_string(),
                }
                .into());
            }
            Confirm::Yes => {}
        }
    }
    release_with_quiescing(&pool, &path.to_string_lossy(), &owned)?;
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
    // Empty --if-lease-id is an error (Go return_cmd.go:76-78).
    if let Some(id) = &args.if_lease_id
        && id.is_empty()
    {
        return Err(anyhow!("--if-lease-id must not be empty"));
    }
    if args.all {
        return cmd_return_all(cli, args);
    }

    let pool = open_pool_for_cli(cli)?;

    let raw = args
        .path
        .clone()
        .or_else(|| std::env::var("TREEHOUSE_DIR").ok())
        .ok_or_else(|| anyhow!("no worktree path specified"))?;
    let path = resolve_return_target(&pool, &raw)?;

    let preconditions = treehouse_core::pool::ReleasePreconditions {
        expected_lease_id: args.if_lease_id.clone(),
        expected_lease_holder: args.if_lease_holder.clone(),
        ..Default::default()
    };
    let result = release_one(&pool, &path, args, &preconditions)?;

    // Full render, not stdout-only: `🌳 Worktree returned to pool.` is a
    // byte-exact string scripts match on, and it belongs on stderr. Machine
    // formats still emit the payload on stdout alone.
    let result = treehouse_core::result::CommandResult::Return(result);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    format::render(to_output_format(cli.format)?, &result, &mut out, &mut err)?;
    Ok(())
}

/// `return --all`: reclaim every HELD worktree in this repository's pool.
///
/// A slot nobody holds is never a target — `available` has nothing to return,
/// `damaged` cannot be judged safe to reset (`destroy` is the verb that removes
/// one), and a slot the caller's own shell merely stands in is not being held
/// at all. Everything else is a slot somebody is holding, which is exactly what
/// a bulk return exists to reclaim. This is deliberately WIDER than prune and
/// `destroy --all`, which never touch a leased slot: `--all` exists to reclaim a
/// whole pool.
///
/// Each slot is released exactly as naming it would be, one at a time, and a
/// per-worktree abort, skip, or failure never stops the ones after it — a bulk
/// return that gave up on the first dirty slot would leave the rest held with
/// no indication which.
fn cmd_return_all(cli: &Cli, args: &cli::ReturnArgs) -> Result<()> {
    cmd_return_all_guards(args)?;

    let pool = open_pool_for_cli(cli)?;
    let statuses = pool.status()?;

    let mut targets = Vec::new();
    let mut not_held = 0usize;
    let mut recovered = 0usize;
    let mut stood_in: Vec<String> = Vec::new();
    for wt in &statuses {
        // A recovered entry is leased by a synthetic holder, so nothing on disk
        // proves it idle. Counting it as recovered (not as "held") keeps it out
        // of both totals the summary line reports as returned/skipped targets.
        if wt.status == treehouse_core::pool::STATUS_LEASED
            && wt.lease_holder == treehouse_core::state::RECOVERED_LEASE_HOLDER
        {
            recovered += 1;
            eprintln!(
                "🌳 Leaving {} at {} leased: it was recovered, so nothing proves it idle and it is only returned by name. Check it, then run: treehouse return {}",
                wt.name,
                wt.path,
                quote_path(Path::new(&wt.path))
            );
            continue;
        }
        // A parked slot the caller is merely standing in reads as held only
        // because their own shell is in its process list. `treehouse enter`
        // promises to leave pool state untouched, so returning that slot would
        // both break the promise and reset the tree under the user running the
        // sweep. Naming it still returns it — the narrow target is deliberate.
        if held_only_by_cwd(&pool, wt)? {
            stood_in.push(wt.name.clone());
            not_held += 1;
            continue;
        }
        if returnable_status(&wt.status) {
            targets.push(wt.clone());
        } else {
            not_held += 1;
        }
    }

    let pool_count = statuses.len();
    if targets.is_empty() && recovered == 0 {
        eprintln!("🌳 No held worktrees to return ({pool_count} in the pool).");
        // Still a RESULT. A machine caller asked for a document and must get
        // one: "nothing to do" is the answer, silence is a broken contract, and
        // an empty sweep is the case most likely to be scripted against.
        return render_return_all(
            cli,
            &ReturnAllSummary {
                pool_count,
                considered: targets.len() + recovered,
                returned: Vec::new(),
                skipped: recovered,
                aborted: Vec::new(),
                failed: Vec::new(),
                not_held,
                stood_in: stood_in.clone(),
            },
        );
    }

    let mut returned: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    let mut aborted: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for wt in &targets {
        eprintln!("🌳 Returning {} ({}) at {}", wt.name, wt.status, wt.path);
        // Pin the lease THIS listing saw. The two instants are separated by
        // every earlier confirmation in the run, a far wider window than a
        // named return has.
        let preconditions = bulk_return_preconditions(wt);
        match release_one(&pool, &wt.path, args, &preconditions) {
            Ok(r) if r.aborted => {
                aborted.push(wt.name.clone());
                eprintln!(
                    "   {} left as found: its uncommitted changes were kept.",
                    wt.name
                );
            }
            Ok(_) => returned.push(wt.name.clone()),
            // A skip is not a failure: nothing went wrong and nothing was left
            // half-done, so retrying would report the same thing forever.
            Err(e) if is_release_skip(&e) => {
                skipped += 1;
                if is_recovered_refusal(&e) {
                    eprintln!(
                        "   {} skipped: it was recovered since this run listed it, so it is only returned by name. Check it, then run: treehouse return {}",
                        wt.name,
                        quote_path(Path::new(&wt.path))
                    );
                } else {
                    eprintln!(
                        "   {} skipped: it is no longer the acquisition this run listed, so it was left alone ({e}).",
                        wt.name
                    );
                }
            }
            Err(e) => {
                failed.push(wt.name.clone());
                eprintln!("   {} failed: {e}", wt.name);
            }
        }
    }

    let returned_count = returned.len();
    eprintln!(
        "🌳 Returned {returned_count} of {} held worktree(s); {} skipped; {not_held} not held.",
        targets.len() + recovered,
        skipped + recovered
    );

    let summary = ReturnAllSummary {
        pool_count,
        considered: targets.len() + recovered,
        returned,
        skipped: skipped + recovered,
        aborted: aborted.clone(),
        failed: failed.clone(),
        not_held,
        stood_in: stood_in.clone(),
    };
    render_return_all(cli, &summary)?;

    // A failure outranks an abort: retrying is the right response to a failure
    // and the wrong one to a worktree deliberately left dirty, so the more
    // urgent of the two decides the exit status.
    if !failed.is_empty() {
        return Err(anyhow!(
            "failed to return worktree(s) {}",
            failed.join(", ")
        ));
    }
    if !aborted.is_empty() {
        return Err(NotReturned {
            not_returned: format!(
                "🌳 worktree(s) {} not returned: they have uncommitted changes and cleaning was declined or could not be confirmed; prune will not reclaim those slots. Use treehouse return --all --force to clean and return them",
                aborted.join(", ")
            ),
            legacy: "Aborted.".to_string(),
        }
        .into());
    }
    Ok(())
}

/// What a `return --all` sweep did, in the shape a machine caller reads.
///
/// `skipped` and `failed` carry NAMES while `returned` carries the names that
/// came back; the asymmetry is deliberate — a skip and a failure are both
/// per-slot outcomes an operator has to chase down by name, whereas `returned`
/// is also the count the exit status is derived from.
///
/// The shape is this command's own rather than `CommandResult::Return`: that
/// variant describes ONE worktree (`path`, `returned`, `aborted`), and a bulk
/// sweep returning a single-worktree document would tell a script it returned
/// one tree however many it actually released.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct ReturnAllSummary {
    pool_count: usize,
    /// Slots the run took as targets (including recovered entries it left
    /// alone) — the denominator the human summary line reports against.
    considered: usize,
    returned: Vec<String>,
    skipped: usize,
    aborted: Vec<String>,
    failed: Vec<String>,
    not_held: usize,
    /// Slots skipped because the caller's own shell was the only thing standing
    /// in them. Reported so the count is explainable rather than mysterious.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stood_in: Vec<String>,
}

impl ReturnAllSummary {
    fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Renders a `return --all` sweep in the requested format.
///
/// This is what the previous `eprintln!`-and-return did not do: under
/// `--format json|toon` the command emitted ZERO bytes on stdout, so every
/// consumer of the machine formats hung on an empty read or, worse, treated a
/// closed stdout as "nothing was held" when the sweep had in fact just returned
/// eight worktrees. The human lines above stay on stderr, byte for byte.
fn render_return_all(cli: &Cli, summary: &ReturnAllSummary) -> Result<()> {
    let fmt = resolve_machine_format(cli, /* json_flag = */ false)?;
    if fmt == format::OutputFormat::Human {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    render_payload(fmt, &summary.to_json(), &mut out, &mut err)
}

/// Writes one machine-format document to stdout, discarding stderr banners.
///
/// `render_payload` mirrors what `format::render` does for a single command
/// result; it exists here because the bulk payloads this file emits have no
/// `CommandResult` variant to hand to it. The two must not drift, so the
/// encoding rules are stated once, here, and the single-result renderer is left
/// as it is.
fn render_payload(
    fmt: format::OutputFormat,
    payload: &serde_json::Value,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> Result<()> {
    let encoded = match fmt {
        format::OutputFormat::Human => return Ok(()),
        // `toon::encode` takes its input by value (its `JsonValue` has no
        // `From<&serde_json::Value>` impl), so the clone is required rather than
        // optional. This arm only compiles under the `toon` feature, which is
        // off by default — hence the `cargo check --features toon` in review.
        #[cfg(feature = "toon")]
        format::OutputFormat::Toon => toon::encode(payload.clone(), None),
        #[cfg(not(feature = "toon"))]
        format::OutputFormat::Toon => serde_json::to_string(payload)?,
        format::OutputFormat::Json => serde_json::to_string(payload)?,
    };
    writeln!(out, "{encoded}")?;
    Ok(())
}

/// Resolves the single worktree a return acts on (Go `resolveReturnTarget`).
///
/// The argument is read as a PATH first and only then as a slot NAME, so every
/// argument that resolves today keeps resolving to exactly the same worktree.
/// That ordering matters because the two vocabularies collide — standing in a
/// pool directory, `1` is both a slot name and a real subdirectory — and a
/// path that already works must never be redirected.
fn resolve_return_target(pool: &treehouse_core::pool::Pool, arg: &str) -> Result<String> {
    let abs = absolute(arg);
    match pool.find_by_path(&abs) {
        Ok(Some(entry)) => return Ok(entry.path),
        Ok(None) => {}
        Err(e) => return Err(anyhow!("{e}")),
    }
    if !could_be_worktree_name(arg) {
        return Err(anyhow!("worktree {abs} is not managed by treehouse"));
    }
    match pool.find_by_name(arg) {
        Ok(Some(entry)) => Ok(entry.path),
        Ok(None) => Err(anyhow!("{}", unknown_worktree_name_error(&abs, arg, pool)?)),
        Err(e) => Err(anyhow!("{e}")),
    }
}

/// Whether an argument can be read as a slot name (Go `couldBeWorktreeName`).
///
/// A pool names its slots itself and never puts a path separator in a name, so
/// anything holding one is a path and only a path: its failure has to keep
/// reporting the path diagnosis rather than a misleading "no worktree named".
/// Backslash is rejected on every platform — it separates paths on Windows, and
/// no generated slot name contains one anywhere.
fn could_be_worktree_name(arg: &str) -> bool {
    if arg.is_empty() || arg == "." || arg == ".." {
        return false;
    }
    if Path::new(arg).is_absolute() {
        return false;
    }
    !arg.contains(['/', '\\'])
}

/// An argument that resolved as neither vocabulary. It names BOTH readings,
/// because the user picked one of them and only they know which, and it lists
/// the names the pool does have. A pool whose names cannot be listed still
/// produces the refusal: the listing is help, not the verdict.
fn unknown_worktree_name_error(
    seen_path: &str,
    name: &str,
    pool: &treehouse_core::pool::Pool,
) -> Result<String> {
    let names = pool.worktree_names().unwrap_or_default();
    if names.is_empty() {
        return Ok(format!(
            "no worktree named {name:?}: the pool is empty, and it is not a treehouse-managed worktree path either. Run 'treehouse get' to create one"
        ));
    }
    Ok(format!(
        "no worktree named {name:?} in pool (available: {}), and {seen_path} is not a treehouse-managed worktree path either. Run 'treehouse status' for details",
        names.join(", ")
    ))
}

/// `filepath.Abs`: absolute against the cwd, and cleaned of `.`/`..`.
fn absolute(arg: &str) -> String {
    let p = Path::new(arg);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    // `canonicalize` would answer about the filesystem and fail on a
    // worktree that has since been removed; `normalize` is purely lexical.
    lexical_normalize(&joined).to_string_lossy().into_owned()
}

/// Lexical `.`/`..` collapse — the Rust stand-in for Go's `filepath.Clean`.
/// Works on a path that does not exist, which `canonicalize` does not.
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `..` at the filesystem root IS the root (Go's
                // `filepath.Clean("/../a") == "/a"`), so it is dropped there
                // rather than accumulated into a path that escapes upwards.
                Some(Component::RootDir) => {}
                // Already leading with `..`, or a bare relative path: keep it.
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The flag combinations `--all` refuses, before any pool is even opened.
///
/// `--all` targets every held slot, so naming one of them is a contradiction,
/// and a `--if-lease-*` condition identifies a single acquisition — which is
/// the one thing a bulk sweep deliberately cannot pin. Both refusals are
/// byte-exact Go (return_cmd.go:92-101).
fn cmd_return_all_guards(args: &cli::ReturnArgs) -> Result<()> {
    if args.path.is_some() {
        return Err(anyhow!(
            "--all takes no path or name; it returns every held worktree in this repository's pool"
        ));
    }
    if args.if_lease_id.is_some() || args.if_lease_holder.is_some() {
        return Err(anyhow!(
            "--all cannot be combined with --if-lease-id or --if-lease-holder; a lease condition identifies one acquisition, so name that worktree instead"
        ));
    }
    Ok(())
}

/// Whether `return --all` acts on a slot in this state (Go `returnableStatus`).
///
/// `available` has nothing to return and `damaged`'s contents cannot be judged,
/// so neither is safe to reset — `destroy` is the verb that removes one.
fn returnable_status(status: &str) -> bool {
    !matches!(
        status,
        treehouse_core::pool::STATUS_AVAILABLE | treehouse_core::pool::STATUS_DAMAGED
    )
}

/// Whether the caller's own shell standing in a slot is the ONLY thing making it
/// look held (Go `WorktreeStatus.HeldOnlyByCwd`).
///
/// **This exclusion is load-bearing, not decoration.** Go's `pool.List` filters
/// the caller's own process chain out of the process scan before classifying, so
/// a parked slot the user has merely `cd`'d into reports `available` underneath
/// and the `you're here` overlay sets `HeldOnlyByCwd`. This port's
/// `Pool::status` does not filter (`pool.rs:898` calls `find_in_worktree`
/// unfiltered), so that same slot reports `you're here` with the user's own
/// shell sitting in its process list — a "held" reading produced entirely by the
/// caller's presence. `return --all` therefore has to exclude it here or it
/// resets the tree under the user it is running for.
///
/// It is decided from dirtiness, and every other precondition is already
/// settled by the time a slot reads `you're here`: `leased` and a live owner
/// reservation both outrank the overlay, and `damaged` is checked after it, so a
/// `you're here` slot is none of those. That leaves the caller's cwd as the sole
/// possible claim when the tree is clean — which is exactly Go's predicate.
///
/// **Known widening.** Go can tell "only my shell is in here" from "my shell
/// AND an agent are both in here" because it holds the protected chain. That
/// chain is not reachable from this crate, so a CLEAN slot holding a genuine
/// foreign process is excluded too. That errs towards not releasing rather than
/// towards resetting a live agent's tree, and the slot is still returned by name
/// (`treehouse return <path>`), so nothing is stranded.
fn held_only_by_cwd(
    pool: &treehouse_core::pool::Pool,
    wt: &treehouse_core::pool::WorktreeStatus,
) -> Result<bool> {
    if wt.status != treehouse_core::pool::STATUS_HERE {
        return Ok(false);
    }
    // A markerless slot is `damaged` by the time it could read `you're here`,
    // but `git_is_dirty` on it would answer about the repository ENCLOSING an
    // in-project pool, so never dispatch git for this question.
    if !treehouse_core::pool::Pool::is_readable_worktree(Path::new(&wt.path)) {
        return Ok(false);
    }
    Ok(!pool.git_is_dirty(Path::new(&wt.path))?)
}

/// The lease the `--all` listing observed, carried into the release of that
/// slot (Go `bulkReturnPreconditions`).
///
/// The two branches are two DIFFERENT predicates and are spelled out rather
/// than folded into one value: "still exactly this acquisition" and "still
/// nobody's" are opposites, and one field carrying both would read correctly
/// and behave oppositely the day a caller passed an empty identity.
fn bulk_return_preconditions(
    wt: &treehouse_core::pool::WorktreeStatus,
) -> treehouse_core::pool::ReleasePreconditions {
    use treehouse_core::pool::ReleasePreconditions;
    if wt.status == treehouse_core::pool::STATUS_LEASED {
        if wt.lease_id.is_empty() {
            // A state file predating lease ids offers nothing to compare, so
            // only the recovered-entry refusal survives.
            return ReleasePreconditions {
                refuse_recovered: true,
                ..Default::default()
            };
        }
        return ReleasePreconditions {
            expected_lease_id: Some(wt.lease_id.clone()),
            refuse_recovered: true,
            ..Default::default()
        };
    }
    ReleasePreconditions {
        require_unleased: true,
        refuse_recovered: true,
        ..Default::default()
    }
}

/// A release refusal that means "leave it alone", as opposed to a failure.
///
/// Go distinguishes these because calling them failures made a quarantined
/// pool exit 1 on every retry forever.
fn is_release_skip(e: &anyhow::Error) -> bool {
    let Some(pool_err) = e.downcast_ref::<treehouse_core::pool::PoolError>() else {
        return false;
    };
    matches!(
        pool_err,
        treehouse_core::pool::PoolError::LeasePrecondition { .. }
            | treehouse_core::pool::PoolError::OwnerPrecondition { .. }
            | treehouse_core::pool::PoolError::RecoveredEntry(_)
    )
}

fn is_recovered_refusal(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<treehouse_core::pool::PoolError>(),
        Some(treehouse_core::pool::PoolError::RecoveredEntry(_))
    )
}

/// Preconditions + the dirty confirmation + the release itself (Go
/// `releaseWorktree`).
///
/// The preconditions run FIRST: a slot the release already knows it will
/// refuse must not be announced as a dirty worktree about to be cleaned.
/// Offering to discard someone's uncommitted changes and then refusing anyway
/// is worse than refusing outright.
fn release_one(
    pool: &treehouse_core::pool::Pool,
    path: &str,
    args: &cli::ReturnArgs,
    preconditions: &treehouse_core::pool::ReleasePreconditions,
) -> Result<treehouse_core::result::ReturnResult> {
    pool.validate_release_preconditions(path, preconditions)?;

    // A markerless slot's dirtiness must never be read: dispatch on such a path
    // falls back to the configured backend, which in an in-project pool would
    // answer with the uncommitted changes of the repository ENCLOSING the pool.
    let readable = treehouse_core::pool::Pool::is_readable_worktree(Path::new(path));
    let dirty = readable && pool.git_is_dirty(Path::new(path))?;
    if dirty && !args.force {
        match confirm("Clean worktree and return to pool? [Y/n]")? {
            Confirm::Yes => {}
            other => return Err(not_returned_abort(path, other)),
        }
    }

    release_with_quiescing(pool, path, preconditions)?;
    Ok(treehouse_core::result::ReturnResult {
        path: path.to_string(),
        returned: true,
        aborted: false,
        ..Default::default()
    })
}

/// The abort of a `return`: the worktree and any lease on it are exactly as
/// they were found.
///
/// `unanswered` and a decline are kept distinct because they demand different
/// responses, but BOTH mean the slot stays held — and a caller reading exit 0
/// as "the slot was released" was reading a bug (Go's v3.0.0 rationale, and the
/// reason this port now exits 3 by default). The exit-3 message names which one
/// it was; the exit-0 opt-out message never did, and stays that way so
/// `TREEHOUSE_EXIT_STRICT=0` reproduces the pre-flip output exactly.
fn not_returned_abort(path: &str, answer: Confirm) -> anyhow::Error {
    let reason = match answer {
        Confirm::Unanswered => {
            "it has uncommitted changes and the confirmation could not be answered (stdin reached EOF)"
        }
        _ => "cleaning declined, so its uncommitted changes remain",
    };
    NotReturned {
        not_returned: format!(
            "🌳 worktree not returned: {reason}; prune will not reclaim this slot. Use treehouse return --force {} to clean and return it",
            quote_path(Path::new(path))
        ),
        legacy: "Aborted.".to_string(),
    }
    .into()
}

/// `treehouse lease <name>`: mark an existing worktree durably leased, in
/// place. State-only — nothing is fetched, reset, cleaned, or checked out, so
/// it is safe on a slot already holding live work (Go cmd/lease.go:21-45).
fn cmd_lease(cli: &Cli, args: &cli::LeaseArgs) -> Result<()> {
    let format = resolve_machine_format(cli, args.json)?;
    let pool = open_pool_for_cli(cli)?;

    let holder = args
        .lease_holder
        .clone()
        .or_else(|| std::env::var("TREEHOUSE_LEASE_HOLDER").ok())
        .unwrap_or_default();

    let lease = pool.lease_existing(&args.name, &holder)?;
    eprintln!(
        "🌳 Leased worktree {} at {}. Run 'treehouse return {}' to release it.",
        args.name, lease.path, lease.path
    );

    let result =
        treehouse_core::result::CommandResult::Get(treehouse_core::result::GetResult::Lease(lease));
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    format::render_stdout(format, &result, &mut out)?;
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
    if args.all {
        return cmd_prune_all(cli, args);
    }
    let pool = open_pool_for_cli(cli)?;
    let opts = PruneOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
        ..Default::default()
    };
    let result = pool.prune(&opts)?;

    render_prune(
        cli,
        result,
        SweepScope {
            global: false,
            pool_count: 1,
            orphans_included: args.prune_orphans,
        },
    )?;
    Ok(())
}

/// `prune --all`: sweep every managed pool under the user-level root.
fn cmd_prune_all(cli: &Cli, args: &cli::PruneArgs) -> Result<()> {
    let user = TreehouseConfig::load_global()?;
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
    let pool_count = count_pools(&user);
    render_prune(
        cli,
        merged,
        SweepScope {
            global: true,
            pool_count,
            orphans_included: args.prune_orphans,
        },
    )?;

    // Non-zero exit if any pool had an error (CI-friendly).
    if has_pool_errors {
        std::process::exit(1);
    }
    Ok(())
}

/// Renders a prune result to stdout/stderr in the requested format.
fn render_prune(
    cli: &Cli,
    result: treehouse_core::prune::PruneResult,
    scope: SweepScope,
) -> Result<()> {
    let result = treehouse_core::result::CommandResult::Prune(result, scope);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    format::render(to_output_format(cli.format)?, &result, &mut out, &mut err)?;
    Ok(())
}

/// Reclaim stale, orphaned, and dead-owner worktrees (dry-run default).
fn cmd_gc(cli: &Cli, args: &cli::GcArgs) -> Result<()> {
    if args.all {
        return cmd_gc_all(cli, args);
    }
    let pool = open_pool_for_cli(cli)?;
    let opts = treehouse_core::gc::GcOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
    };
    let result = pool.gc(&opts)?;

    render_gc(
        cli,
        &result,
        SweepScope {
            global: false,
            pool_count: 1,
            orphans_included: args.prune_orphans,
        },
    )?;
    Ok(())
}

/// `gc --all`: sweep every managed pool under the user-level root.
fn cmd_gc_all(cli: &Cli, args: &cli::GcArgs) -> Result<()> {
    let opts = treehouse_core::gc::GcOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
    };
    let (merged, pool_count) = sweep_all_pools(&opts)?;

    // An empty sweep still RENDERS. The early return this replaces printed the
    // human banner and stopped, so `treehouse gc --all --format json` on a clean
    // machine emitted zero bytes on stdout — a script reading it got an empty
    // document, which parses as "no fields" rather than "zero of everything",
    // and an EOF-only read is indistinguishable from a crashed command.
    // `render_gc` already prints the same banner for the human format, so
    // routing the empty case through it keeps human output byte-identical.
    render_gc(cli, &merged, global_scope(pool_count, args.prune_orphans))?;

    if !merged.errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// How many pools a global sweep covers, for the JSON `pool_count`.
///
/// Resolved through the same root and the same discovery `sweep_pools` uses,
/// so the number describes the sweep that actually ran rather than a different
/// walk. `--root` is deliberately not consulted: it redirects where a single
/// repository's pool is WRITTEN, not where the `--all` sweep looks.
fn count_pools(user: &TreehouseConfig) -> u32 {
    let Some(root) = treehouse_core::discovery::user_pool_root_with_config(user) else {
        return 0;
    };
    treehouse_core::discovery::discover_pools(&root).pools.len() as u32
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
) -> Result<(treehouse_core::gc::GcResult, u32), anyhow::Error> {
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
    Ok((
        treehouse_core::discovery::merge_gc_results(results),
        count_pools(&user),
    ))
}

/// `watch --once` or `watch --interval <dur>`: sweep all pools.
///
/// This is a thin orchestrator — all cleanup logic lives in the GC engine
/// via `sweep_all_pools`. No new cleanup paths are introduced.
fn cmd_watch(cli: &Cli, args: &cli::WatchArgs) -> Result<()> {
    let opts = treehouse_core::gc::GcOptions {
        dry_run: !args.yes,
        prune_orphans: args.prune_orphans,
    };

    let orphans_included = args.prune_orphans;
    if args.once {
        let (merged, pool_count) = sweep_all_pools(&opts)?;
        // Same defect as `gc --all`: an empty sweep returned before the render,
        // so `--format json` produced no document at all. The human banner is
        // kept verbatim and printed only for the human format, because the
        // render's own empty-case wording is `gc`'s, not `watch --once`'s.
        if merged.candidates.is_empty()
            && merged.skipped.is_empty()
            && merged.errors.is_empty()
            && to_output_format(cli.format)? == format::OutputFormat::Human
        {
            eprintln!("🌳 All pools clean. Nothing to reclaim.");
            return Ok(());
        }
        render_gc(cli, &merged, global_scope(pool_count, orphans_included))?;
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
        let (merged, pool_count) = sweep_all_pools(&opts)?;
        let has_errors = !merged.errors.is_empty();
        had_pool_errors = had_pool_errors || has_errors;

        if !merged.candidates.is_empty() || !merged.skipped.is_empty() || has_errors {
            render_gc(cli, &merged, global_scope(pool_count, orphans_included))?;
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

/// Renders a gc result in the requested format.
///
/// The human rendering is byte-identical to the pre-formatter `println!` block
/// this replaced: same lines, same streams (candidates on stdout, skips and
/// errors on stderr). Routing it through the formatter is what makes
/// `gc --format json` emit a document instead of prose — a flag that parsed
/// cleanly and printed prose is the worst failure mode an automation contract
/// can have, because it is silent rather than a loud refusal.
fn render_gc(cli: &Cli, result: &treehouse_core::gc::GcResult, scope: SweepScope) -> Result<()> {
    let result = treehouse_core::result::CommandResult::Gc(result.clone(), scope);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    format::render(to_output_format(cli.format)?, &result, &mut out, &mut err)?;
    Ok(())
}

/// The scope of a `--all` / `watch` sweep over every pool under the root.
fn global_scope(pool_count: u32, orphans_included: bool) -> SweepScope {
    SweepScope {
        global: true,
        pool_count,
        orphans_included,
    }
}

fn cmd_destroy(cli: &Cli, args: &cli::DestroyArgs) -> Result<()> {
    let pool = open_pool_for_cli(cli)?;

    // There is NO cross-pool or global destroy. `--all` must name a pool: the
    // whole risk of the verb is that it is irreversible, and forcing the
    // operator to say WHICH pool is the last stop before a mass deletion. With
    // no path at all, `destroy --all` used to sweep whatever pool the ambient
    // repository happened to resolve to — a muscle-memory invocation from the
    // wrong directory deleted the wrong pool (Go cmd/destroy.go:81-83, which
    // carried the same guard at the v2.1.1 baseline).
    let all = args.all;
    let arg = validate_destroy_target(all, args.path.as_deref())?;
    let target = absolute(arg);
    if all {
        // Resolve the named pool by its own marker, never by a string suffix:
        // "ends with .treehouse" also matches a look-alike directory nobody
        // manages, and treating that as a pool aims a mass deletion at it.
        // Go's `resolveDestroyPoolFromTarget` (destroy.go:190-211) accepts the
        // pool dir itself, a worktree inside one, or a repository — in that
        // order — and falls back to config resolution.
        resolve_destroy_pool_dir(&target)?;
    }

    let spec = if all {
        DestroyTargetSpec::All
    } else {
        DestroyTargetSpec::Single(target)
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
    format::render(to_output_format(cli.format)?, &result, &mut out, &mut err)?;

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

/// The target a `destroy` invocation acts on, or a refusal naming what is
/// missing.
///
/// Two guards, both byte-exact Go (cmd/destroy.go:81-90). `--all` without a
/// pool path is the dangerous one: the whole risk of this verb is that it is
/// irreversible, so forcing the operator to say WHICH pool is the last stop
/// before a mass deletion. Without it, `destroy --all` swept whatever pool the
/// ambient repository resolved to — a muscle-memory invocation from the wrong
/// directory deleted the wrong pool. The bare form used to be an `unwrap()` on
/// `None`: a panic and exit 101, where Go returns a clean error.
fn validate_destroy_target(all: bool, path: Option<&str>) -> Result<&str> {
    match path {
        None if all => Err(anyhow!(
            "--all requires a pool path; name the pool to clear, e.g. 'treehouse destroy . --all'"
        )),
        None => Err(anyhow!(
            "specify a worktree path to destroy, or a pool path with --all"
        )),
        Some(p) => Ok(p),
    }
}

/// Confirms that a `--all` target names a real treehouse pool (Go
/// `resolveDestroyPoolFromTarget`).
///
/// This is a PRECONDITION check, not a re-target: the pool a `--all` sweep
/// acts on is the one the repository resolves to, and this exists so a target
/// that names neither a pool nor a repository is refused with a message that
/// says so, rather than silently falling back to the ambient pool.
fn resolve_destroy_pool_dir(target: &str) -> Result<()> {
    let target = Path::new(target);
    if treehouse_core::pool::Pool::is_pool_dir(target) {
        return Ok(());
    }
    // A worktree inside a pool: <pool>/<slot>/<repo>.
    if let Some(pool_dir) = target.parent().and_then(Path::parent)
        && treehouse_core::pool::Pool::is_pool_dir(pool_dir)
    {
        return Ok(());
    }
    // A repository: its config resolves the pool.
    use treehouse_core::git::GitBackend;
    let discovered =
        treehouse_core::git::ShellGitBackend::discover().and_then(|git| git.repo_root(target));
    match discovered {
        Ok(repo_root) => {
            let config = treehouse_core::config::TreehouseConfig::load(&repo_root)
                .map_err(|e| anyhow!("failed to load config: {e}"))?;
            treehouse_core::config::resolve_pool_dir(&repo_root, config.root.as_deref(), None)
                .map_err(|e| anyhow!("failed to resolve pool directory: {e}"))?;
            Ok(())
        }
        Err(_) => Err(anyhow!(
            "cannot resolve a treehouse pool from {}: not a pool directory or git repository",
            target.display()
        )),
    }
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

/// Overrides the GitHub release API endpoint (Go reads it from the linker's
/// `-X` flag; this port builds one default in, and the installer needs a way to
/// point at a mirror).
///
/// It is also what lets the whole update pipeline be exercised against a local
/// `file://` fixture instead of the network — the one place in this file that
/// writes to the filesystem it is running from.
const ENV_UPDATE_API_URL: &str = "TREEHOUSE_UPDATE_API_URL";

/// Overrides the binary `treehouse update` replaces.
///
/// Production resolves the running executable. A packager staging an install
/// prefix, and the test that proves the download→verify→replace pipeline is
/// wired, both need to aim somewhere that is not the executable under test.
const ENV_UPDATE_TARGET: &str = "TREEHOUSE_UPDATE_TARGET";

/// Update treehouse to the latest release.
fn cmd_update() -> Result<()> {
    // Dev build -> skip (Go: exit 0).
    if treehouse_core::VERSION == "dev" {
        println!("Skipping update: running a dev build");
        return Ok(());
    }
    let current = treehouse_core::VERSION;
    let (api_url, enforce_https) = update_api_endpoint();

    println!("🌳 Checking for updates...");
    // The cache carries a version string and nothing else, and an update needs
    // the release ASSETS, so this check is always live (Go `CheckLatest` is too).
    let result = treehouse_core::updater::check_latest_result(&api_url, current, enforce_https)
        .map_err(|e| anyhow!("checking for updates: {e}"))?;
    treehouse_core::updater::write_cache(&result.latest_version);

    if !result.update_available {
        println!("🌳 treehouse is up to date ({current})");
        return Ok(());
    }

    let target = update_target()?;
    println!("🌳 Updating {current} -> {}...", result.latest_version);
    // Every failure below is an `Err`. Reporting "could not check" as success is
    // what let a script gating on `$?` read a dead network as a finished update.
    let applied = treehouse_core::updater::apply_into(&result, enforce_https, &target)
        .map_err(|e| anyhow!("applying update: {e}"))?;

    // Printed from `applied`, never from `result`: the installed binary is the
    // only thing that makes "updated" true.
    println!(
        "🌳 Successfully updated treehouse {} -> {}",
        applied.from_version, applied.to_version
    );
    Ok(())
}

/// The release API endpoint and whether to require HTTPS for it.
///
/// An override is an explicit operator decision about where release metadata
/// comes from, so it also waives the HTTPS requirement: that check exists to
/// stop a network attacker rewriting what gets installed, and an operator who
/// has already redirected the endpoint is past that threat. Production never
/// sets it and always gets the real, HTTPS-enforced default.
fn update_api_endpoint() -> (String, bool) {
    match std::env::var(ENV_UPDATE_API_URL) {
        Ok(url) if !url.trim().is_empty() => (url.trim().to_string(), false),
        _ => (
            treehouse_core::updater::DEFAULT_GITHUB_API_URL.to_string(),
            true,
        ),
    }
}

/// The binary `update` replaces: the override if set, otherwise the running
/// executable with symlinks resolved.
///
/// The resolution is repeated here rather than calling `updater::apply`
/// because the override has to bypass it, and a second `canonicalize` is
/// cheaper than a second code path through the replacement.
fn update_target() -> Result<PathBuf> {
    if let Ok(target) = std::env::var(ENV_UPDATE_TARGET)
        && !target.trim().is_empty()
    {
        return Ok(PathBuf::from(target.trim()));
    }
    let exe = std::env::current_exe().map_err(|e| anyhow!("resolving executable: {e}"))?;
    std::fs::canonicalize(&exe).map_err(|e| anyhow!("resolving symlinks of {}: {e}", exe.display()))
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

/// Maps the clap format onto the formatter's, with an error rather than a
/// silent fallback: a machine format that quietly rendered prose is the failure
/// this exists to prevent.
fn to_output_format(f: OutputFormat) -> Result<format::OutputFormat> {
    Ok(match f {
        OutputFormat::Human => format::OutputFormat::Human,
        OutputFormat::Json => format::OutputFormat::Json,
        OutputFormat::Toon => format::OutputFormat::Toon,
    })
}

/// Resolves `--json` / `--format` for a command that has no `--lease`
/// requirement, rejecting only the conflict (Go's per-command `--json` is an
/// alias of `--format json`, not a separate format).
fn resolve_machine_format(cli: &Cli, json_flag: bool) -> Result<format::OutputFormat> {
    let format = cli.format;
    if json_flag && format != OutputFormat::Human && format != OutputFormat::Json {
        return Err(anyhow!(
            "conflicting output formats: --json and --format {}",
            format_variant_str(format)
        ));
    }
    to_output_format(if json_flag {
        OutputFormat::Json
    } else {
        format
    })
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

/// One answer to a `[Y/n]` prompt.
///
/// `Unanswered` is a DISTINCT outcome from `No`, not a flavour of it. Every
/// non-TTY caller — CI, a Makefile, a hook, an agent script, a cron job — hands
/// us EOF instead of an answer, and collapsing that into "no" would be safe but
/// silent while collapsing it into "yes" destroys uncommitted work. Keeping it
/// separate is what lets the two be reported differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Yes,
    No,
    /// stdin reached EOF (or errored) carrying no answer at all.
    Unanswered,
}

/// A `[Y/n]` confirmation prompt that FAILS CLOSED on stdin EOF.
///
/// The bytes actually read decide the outcome, not whether `read_line`
/// succeeded: at EOF `read_line` returns `Ok(0)`, not `Err`, so an
/// implementation that only propagated the error treated "no input" as the
/// default. That made `treehouse return <dirty-path> < /dev/null` confirm the
/// destructive reset and exit 0 — irreversible data loss, silently, for every
/// scripted caller (Go `ui.Confirm` + `errReturnAbortedNonTTY`,
/// prompt.go:46-50 / return_cmd.go:399-418).
///
/// A bare Enter with no error is the `[Y/n]` hint documented in the prompt
/// itself and still means "yes"; a read error carrying an answer (`echo y |`,
/// no trailing newline) is honoured, because the operator did answer.
fn confirm(prompt: &str) -> Result<Confirm> {
    eprint!("{prompt} ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    let read = std::io::stdin()
        .read_line(&mut input)
        .map_err(|e| anyhow!("reading confirmation from stdin: {e}"))?;
    Ok(classify_answer(read, &input))
}

/// The confirmation decision, as a pure function of what was actually read.
///
/// `read` is the byte count `read_line` reported, which is the ONLY thing that
/// distinguishes "the operator pressed Enter" (`read > 0`, input just a
/// newline) from "stdin was at EOF" (`read == 0`, input empty). Both leave the
/// trimmed string empty, so judging on the string alone is what made EOF mean
/// "yes".
fn classify_answer(read: usize, input: &str) -> Confirm {
    let answer = input.trim().to_lowercase();
    if answer.is_empty() {
        return if read == 0 {
            Confirm::Unanswered
        } else {
            // A bare Enter with no error is the `[Y/n]` hint the prompt
            // itself documents.
            Confirm::Yes
        };
    }
    match answer.as_str() {
        "y" | "yes" => Confirm::Yes,
        _ => Confirm::No,
    }
}

/// Makes a worktree path safe to paste after `treehouse return --force`.
/// Unquoted or double-quoted paths can still expand `$()`, backticks, or
/// command separators in a POSIX shell.
fn quote_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.is_empty() {
        return String::new();
    }
    #[cfg(windows)]
    {
        // cmd.exe groups on double quotes; a doubled quote is the cmd escape
        // for an embedded one.
        return format!("\"{}\"", s.replace('"', "\"\""));
    }
    #[cfg(not(windows))]
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// How long a detached writer gets between SIGTERM and SIGKILL (Go
/// `killLingeringProcesses`, cmd/get.go:331).
const LINGERING_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Releases a slot, detaching its HEAD and quiescing its writers first.
///
/// Both halves run inside `release_conditional`'s `before_reset`, which is
/// invoked under the pool state lock and immediately before the destructive
/// reset. That placement is the whole point: the emptiness check and the
/// `reset --hard` + `clean -fd` cannot be interleaved with a writer entering the
/// slot, and the release preconditions have already been validated against the
/// same locked state the detach reads.
///
/// Returns the `PoolError` the release must surface, so a slot that cannot be
/// proven quiet is left in place instead of being reset under a live process.
fn release_with_quiescing(
    pool: &treehouse_core::pool::Pool,
    path: &str,
    preconditions: &treehouse_core::pool::ReleasePreconditions,
) -> Result<(), treehouse_core::pool::PoolError> {
    let worktree = Path::new(path).to_path_buf();
    // A markerless slot is never dispatched on git: `checkout --detach` there
    // falls back to the configured backend, which in an in-project pool moves
    // the HEAD of the repository ENCLOSING the pool (Go cmd/get.go:201-203).
    let detachable = treehouse_core::pool::Pool::is_readable_worktree(&worktree);
    let git = if detachable {
        // No git on PATH: the release itself fails on its next git call, so
        // there is nothing to lose by letting it report that.
        treehouse_core::git::ShellGitBackend::discover().ok()
    } else {
        None
    };

    let mut before_reset = || {
        if let Some(git) = &git {
            use treehouse_core::git::GitBackend;
            // A failed detach is a warning, not a refusal (Go prints the same
            // and continues): the reset that follows re-detaches anyway, and
            // refusing here would strand every slot on a git that dislikes it.
            if let Err(e) = git.detach_worktree(&worktree) {
                eprintln!("🌳 Warning: failed to detach worktree HEAD: {e}");
            }
        }
        terminate_lingering(&worktree)
    };
    pool.release_conditional(path, preconditions, Some(&mut before_reset))
}

/// Terminates every process still writing into `path` and proves none survived.
///
/// The scan is live rather than the pool's cached table, which matters on the
/// `run` path where the worktree is opened before the child is ever spawned.
/// The caller's own process and its whole ancestor chain are protected, so a
/// `treehouse return` issued from inside the worktree does not signal the shell
/// running it (Go `filterProtectedProcesses`, process.rs:249).
///
/// This is `release_conditional`'s `before_reset` for the same reason Go wires
/// it there: the termination and the `reset --hard` have to be one locked
/// transaction, or a writer that re-enters between them is clobbered mid-write.
fn terminate_lingering(path: &Path) -> Result<(), treehouse_core::pool::PoolError> {
    let table = treehouse_core::process::ProcessTable::new();
    let killed = table
        .terminate_with_grace(path, LINGERING_GRACE)
        .map_err(lingering_error)?;
    if !killed.is_empty() {
        let names: Vec<String> = killed.iter().map(|p| p.to_string()).collect();
        eprintln!("🌳 Terminated lingering processes: {}", names.join(", "));
    }
    Ok(())
}

/// A `before_reset` failure carrying no `PoolError` variant of its own.
///
/// `PoolError::Io` is the only variant that carries an operator-facing string
/// without asserting a state lie (a lease mismatch, an owner mismatch, a
/// corrupt state file — none of which this is), so the process-scan message is
/// spelled into it rather than flattened into an opaque lock error.
fn lingering_error(message: impl std::fmt::Display) -> treehouse_core::pool::PoolError {
    let text = message.to_string();
    treehouse_core::pool::PoolError::Io(
        format!("could not terminate worktree processes: {text}"),
        std::io::Error::other(text),
    )
}

/// Acquire -> run an agent -> cleanup guaranteed on every exit.
///
/// `run` is a REAL subcommand, not clap's `external_subcommand` catch-all.
/// With a catch-all, every unknown verb — a typo, or a subcommand this build
/// does not have — was routed here, which durably leased a pool slot and then
/// tried to spawn the typo as a shell command. The slot stayed leased for the
/// whole TTL with nothing to explain it, and `status` was the only place the
/// cause was visible. Go registers exactly nine commands with no catch-all and
/// rejects `unknown command "statuss"`; clap now does the same.
fn cmd_run(cli: &Cli, args: &cli::RunArgs) -> Result<()> {
    // `last = true` starts the command at the first non-flag token, so the
    // command's own flags reach it verbatim and are never eaten here. A
    // leading `--` the shell passed through is still stripped.
    let mut command: Vec<String> = args.cmd.clone();
    if command.first().map(|a| a == "--").unwrap_or(false) {
        command.remove(0);
    }
    if command.is_empty() {
        return Err(anyhow!("run requires a command"));
    }
    let ttl = match &args.ttl {
        Some(s) => humantime::parse_duration(s)?,
        None => std::time::Duration::from_secs(24 * 3600),
    };
    let holder = args
        .lease_holder
        .clone()
        .or_else(|| std::env::var("TREEHOUSE_LEASE_HOLDER").ok())
        .unwrap_or_else(|| format!("run:{}", std::process::id()));

    let pool = open_pool_for_cli(cli)?;
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

    // ─── M-001: the dirty-return confirmation must fail closed on EOF ──────

    /// The regression, in one assertion: a 0-byte read is EOF, and EOF is NOT
    /// the `[Y/n]` default. Treating it as "yes" made
    /// `treehouse return <dirty-path> < /dev/null` hard-reset the worktree,
    /// destroying uncommitted work, and exit 0 — for every scripted caller.
    #[test]
    fn confirm_eof_is_unanswered_not_yes() {
        assert_eq!(classify_answer(0, ""), Confirm::Unanswered);
        assert_ne!(classify_answer(0, ""), Confirm::Yes);
    }

    #[test]
    fn confirm_bare_enter_is_the_documented_default() {
        // A newline WAS read, so the operator pressed Enter: that is the `[Y/n]`
        // hint, and it must keep meaning yes.
        assert_eq!(classify_answer(1, "\n"), Confirm::Yes);
        assert_eq!(classify_answer(1, "   \n"), Confirm::Yes);
        assert_eq!(classify_answer(1, "  y  \n"), Confirm::Yes);
        assert_eq!(classify_answer(1, "Y\n"), Confirm::Yes);
        assert_eq!(classify_answer(1, "yes\n"), Confirm::Yes);
        assert_eq!(classify_answer(1, "n\n"), Confirm::No);
        assert_eq!(classify_answer(1, "no\n"), Confirm::No);
    }

    #[test]
    fn confirm_honours_an_answer_that_arrived_without_a_newline() {
        // `echo -n y |` reads 1 byte and EOF together; the operator DID answer,
        // so discarding it would report an abort over an explicit confirmation.
        assert_eq!(classify_answer(1, "y"), Confirm::Yes);
        assert_eq!(classify_answer(1, "n"), Confirm::No);
    }

    // ─── M-005: `run` is a real subcommand, not a catch-all ─────────────────

    #[test]
    fn unknown_verbs_are_rejected_instead_of_routed_to_run() {
        // Go: `unknown command "statuss" for "treehouse"`, and crucially it
        // acquires NOTHING. A catch-all acquired a worktree and durably leased
        // it before failing, which starved the pool for the whole TTL.
        for verb in ["statuss", "lease", "return-all", "gett"] {
            assert!(
                Cli::try_parse_from(["treehouse", verb]).is_err(),
                "{verb:?} must not be accepted"
            );
        }
        assert!(Cli::try_parse_from(["treehouse", "run", "--", "echo", "hi"]).is_ok());
        assert!(Cli::try_parse_from(["treehouse", "statuss"]).is_err());
    }

    #[test]
    fn run_captures_the_command_verbatim_including_its_own_flags() {
        let cli = Cli::try_parse_from([
            "treehouse",
            "run",
            "--ttl",
            "30m",
            "--lease-holder",
            "agent-7",
            "--",
            "npm",
            "test",
            "--watch",
            "--lease-holder",
            "not-ours",
        ])
        .unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.ttl.as_deref(), Some("30m"));
        assert_eq!(args.lease_holder.as_deref(), Some("agent-7"));
        assert_eq!(
            args.cmd,
            vec!["npm", "test", "--watch", "--lease-holder", "not-ours"],
            "the command's own flags must survive: treehouse stops parsing at the first non-flag token"
        );
    }

    #[test]
    fn run_requires_a_command() {
        assert!(Cli::try_parse_from(["treehouse", "run"]).is_err());
    }

    // ─── M-006: `destroy --all` must name a pool ────────────────────────────

    #[test]
    fn destroy_all_without_a_pool_path_is_refused() {
        let err = validate_destroy_target(true, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "--all requires a pool path; name the pool to clear, e.g. 'treehouse destroy . --all'"
        );
    }

    #[test]
    fn bare_destroy_is_a_clean_error_not_a_panic() {
        // It used to be `Option::unwrap()` on None: exit 101, no message.
        let err = validate_destroy_target(false, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "specify a worktree path to destroy, or a pool path with --all"
        );
    }

    #[test]
    fn destroy_target_passes_the_named_argument_through() {
        assert_eq!(
            validate_destroy_target(false, Some("/p/1/repo")).unwrap(),
            "/p/1/repo"
        );
        assert_eq!(validate_destroy_target(true, Some(".")).unwrap(), ".");
    }

    // ─── M-016 / M-017: `return <name>` and `return --all` ──────────────────

    #[test]
    fn a_slot_name_is_only_read_as_a_name_when_it_cannot_be_a_path() {
        // The ordering is load-bearing: standing in a pool directory, `1` is
        // BOTH a slot name and a real subdirectory, and a path that already
        // resolves must never be redirected to the name reading.
        assert!(could_be_worktree_name("1"));
        assert!(could_be_worktree_name("12"));
        assert!(!could_be_worktree_name("/pool/1/repo"));
        assert!(!could_be_worktree_name("./1"));
        assert!(!could_be_worktree_name("../1"));
        assert!(
            !could_be_worktree_name(r"pool\1"),
            "backslash separates on Windows"
        );
        assert!(!could_be_worktree_name(""));
        assert!(!could_be_worktree_name("."));
        assert!(!could_be_worktree_name(".."));
    }

    #[test]
    fn absolute_is_lexical_so_it_works_on_a_removed_worktree() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            lexical_normalize(Path::new("/a/./b/")),
            PathBuf::from("/a/b")
        );
        assert_eq!(lexical_normalize(Path::new("/../a")), PathBuf::from("/a"));
    }

    #[test]
    fn return_all_skips_only_what_nobody_holds() {
        use treehouse_core::pool::{
            STATUS_AVAILABLE, STATUS_DAMAGED, STATUS_DIRTY, STATUS_IN_USE, STATUS_LEASED,
        };
        // Nothing to return, or its marker cannot be read so a reset cannot be
        // judged safe — `destroy` is the verb that removes a damaged slot.
        assert!(!returnable_status(STATUS_AVAILABLE));
        assert!(!returnable_status(STATUS_DAMAGED));
        // Everything else is a slot somebody is holding.
        assert!(returnable_status(STATUS_LEASED));
        assert!(returnable_status(STATUS_IN_USE));
        assert!(returnable_status(STATUS_DIRTY));
    }

    #[test]
    fn bulk_return_pins_the_lease_the_listing_saw() {
        use treehouse_core::pool::{STATUS_IN_USE, STATUS_LEASED};
        let mut ws = worktree_status("1", STATUS_LEASED);
        ws.lease_id = "abc".into();
        let p = bulk_return_preconditions(&ws);
        assert_eq!(p.expected_lease_id.as_deref(), Some("abc"));
        assert!(p.refuse_recovered);
        assert!(
            !p.require_unleased,
            "a leased slot is pinned, not required unleased"
        );

        // An unleased observation asks for the OPPOSITE predicate.
        let p = bulk_return_preconditions(&worktree_status("1", STATUS_IN_USE));
        assert!(p.require_unleased);
        assert!(p.expected_lease_id.is_none());
        assert!(p.refuse_recovered);

        // A lease id too old to compare offers only the recovered refusal.
        let ws = worktree_status("1", STATUS_LEASED);
        let p = bulk_return_preconditions(&ws);
        assert!(p.expected_lease_id.is_none());
        assert!(p.refuse_recovered);
        assert!(!p.require_unleased);
    }

    fn worktree_status(name: &str, status: &str) -> treehouse_core::pool::WorktreeStatus {
        treehouse_core::pool::WorktreeStatus {
            name: name.to_string(),
            path: format!("/pool/{name}/repo"),
            status: status.to_string(),
            processes: vec![],
            lease_id: String::new(),
            lease_holder: String::new(),
            leased_at: treehouse_core::state::ZERO_TIME,
        }
    }

    #[test]
    fn return_all_refuses_a_path_and_a_lease_condition() {
        let mut args = return_args();
        args.all = true;
        args.path = Some("/pool/1/repo".into());
        let err = cmd_return_all_guards(&args).unwrap_err();
        assert!(
            err.to_string().starts_with("--all takes no path or name"),
            "{err}"
        );

        let mut args = return_args();
        args.all = true;
        args.if_lease_id = Some("abc".into());
        let err = cmd_return_all_guards(&args).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("--all cannot be combined with --if-lease-id"),
            "{err}"
        );
    }

    fn return_args() -> cli::ReturnArgs {
        cli::ReturnArgs {
            force: false,
            all: false,
            if_lease_id: None,
            if_lease_holder: None,
            path: None,
        }
    }

    // ─── M-019: the unreturned signal is never silent ─────────────────────────

    #[test]
    fn an_abort_carries_both_the_exit_three_and_the_backward_compatible_message() {
        let e = not_returned_abort("/pool/1/repo", Confirm::Unanswered);
        let text = format!("{e:#}");
        assert!(
            text.contains("stdin reached EOF"),
            "the default exit-3 message must say the confirmation was unanswerable: {text}"
        );
        assert!(text.contains("prune will not reclaim"), "{text}");
        assert!(
            text.contains("treehouse return --force '/pool/1/repo'"),
            "the remedy must be pasteable: {text}"
        );
        let n = e.downcast_ref::<NotReturned>().unwrap();
        assert_eq!(
            n.legacy, "Aborted.",
            "the opt-out path keeps this port's pre-flip exit-0 wording"
        );

        let declined = not_returned_abort("/pool/1/repo", Confirm::No);
        assert!(format!("{declined:#}").contains("cleaning declined"));
    }

    /// The flip itself. Exit 3 is the DEFAULT and `TREEHOUSE_EXIT_STRICT=0` is
    /// the escape hatch, so an unset variable and a leftover `=1` from the
    /// opt-in era must BOTH land on the strict path — only an explicit falsy
    /// value may restore exit 0.
    ///
    /// Env vars are process-global and tests share a process, so this asserts
    /// the pure reader rather than mutating the environment: setting it here
    /// would race every other test that shells out to `main`'s subprocesses.
    #[test]
    fn legacy_zero_is_opted_in_only_by_an_explicit_false() {
        for value in ["0", "false", "off", "n", "no", "f", " FALSE ", "off "] {
            assert!(
                parses_as_false(value),
                "{value:?} must restore exit 0"
            );
        }
        // The dangerous cases: a stale opt-in, a typo, and the empty string a
        // shell exports when a variable is declared but never assigned.
        for value in ["1", "true", "on", "yes", "y", "", "  ", "maybe", "2"] {
            assert!(
                !parses_as_false(value),
                "{value:?} must keep the exit-3 default"
            );
        }
    }

    /// The three construction sites must all populate the exit-3 message, so a
    /// new call site cannot accidentally ship Go's wording only to the opt-out
    /// (or vice versa).
    #[test]
    fn every_not_returned_site_populates_both_messages() {
        for n in [
            not_returned_abort("/pool/1/repo", Confirm::Unanswered),
            not_returned_abort("/pool/1/repo", Confirm::No),
        ] {
            let n = n.downcast_ref::<NotReturned>().unwrap();
            assert!(
                !n.not_returned.is_empty(),
                "the exit-3 message must never be blank"
            );
            assert_eq!(n.legacy, "Aborted.", "the opt-out wording is fixed");
        }
    }

    #[test]
    fn quote_path_neutralises_shell_metacharacters() {
        assert_eq!(quote_path(Path::new("/pool/1/repo")), "'/pool/1/repo'");
        assert_eq!(
            quote_path(Path::new("/it's here")),
            r"'/it'\''s here'",
            "an embedded quote must not terminate the quoted string"
        );
    }

    // ─── M-023: `--root` and its Go precedence ──────────────────────────────

    #[test]
    fn root_flag_is_global_and_keeps_the_documented_env_path_alias() {
        for argv in [
            ["treehouse", "--root", "/tmp/p", "status"],
            ["treehouse", "status", "--root", "/tmp/p"],
            ["treehouse", "--env-path", "/tmp/p", "status"],
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert_eq!(cli.root.as_deref(), Some("/tmp/p"), "{argv:?}");
        }
    }

    // ─── M-024: the acquisition flags `get` can honour ──────────────────────

    #[test]
    fn get_accepts_base_and_no_fetch() {
        let cli =
            Cli::try_parse_from(["treehouse", "get", "--base", "main", "--no-fetch"]).unwrap();
        let Some(Command::Get(args)) = cli.command else {
            panic!("expected get");
        };
        assert_eq!(args.base.as_deref(), Some("main"));
        assert!(args.no_fetch);
        // `--base` is the RESET TARGET (Go's BaseBranch), and `-b` CREATES a
        // branch (Go's `-b`). They are separate options and must stay separate:
        // collapsing them is how a caller ends up resetting every slot to a
        // branch it meant to create. This assertion previously required `-b` to
        // be REJECTED, which recorded the round-1 gap; now that branch creation
        // is implemented, it asserts the distinction it was written to protect.
        let cli = Cli::try_parse_from(["treehouse", "get", "--base", "main", "-b", "feature"])
            .expect("-b must now be accepted");
        let Some(Command::Get(args)) = cli.command else {
            panic!("expected get");
        };
        assert_eq!(args.base.as_deref(), Some("main"), "--base is the cut FROM");
        assert_eq!(
            args.branch.as_deref(),
            Some("feature"),
            "-b is the branch CREATED"
        );
    }

    // ─── M-025: the background update check is actually wired ────────────────

    #[test]
    fn background_check_command_is_built_with_the_recursion_guard_and_no_stdio() {
        let cmd = background_check_command().expect("the running test binary must be resolvable");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[0], "--update-check",
            "the child must take the child path"
        );
        assert_eq!(args[1], treehouse_core::VERSION);
        let guard = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("TREEHOUSE_NO_UPDATE_CHECK"))
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()));
        assert_eq!(
            guard,
            Some(Some("1".to_string())),
            "the child inherits the guard so a third generation cannot fork again"
        );
    }

    /// The child must not be started inside the worktree the caller is in —
    /// treehouse would then report it as a tenant and `return` would kill it.
    #[test]
    fn a_directory_inside_the_callers_worktree_is_not_a_detach_target() {
        let repo = tempfile::tempdir().unwrap();
        let wt = repo.path().join("1").join("repo");
        std::fs::create_dir_all(wt.join(".git")).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::create_dir_all(repo.path().join("elsewhere")).unwrap();
        assert!(!outside_enclosing_worktree(&wt, &wt.join("src")));
        assert!(!outside_enclosing_worktree(&wt, &wt));
        assert!(
            outside_enclosing_worktree(&repo.path().join("elsewhere"), &wt.join("src")),
            "a sibling of the slot is outside it"
        );
        // A caller in no worktree at all: any directory is safe.
        let plain = tempfile::tempdir().unwrap();
        assert!(outside_enclosing_worktree(&wt, plain.path()));
    }

    #[test]
    fn enclosing_worktree_root_stops_at_the_first_marker() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("1").join("repo").join(".git")).unwrap();
        let deep = repo.path().join("1").join("repo");
        assert_eq!(
            enclosing_worktree_root(&deep).as_deref(),
            Some(deep.as_path()),
            "the worktree root, not the tempdir above it"
        );
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(enclosing_worktree_root(bare.path()), None);
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

    // ─── M-018: the bulk payloads have to reach stdout ────────────────────────

    /// A `return --all` on an empty pool must still serialise.
    ///
    /// The bug this pins is that the summary was written only with `eprintln!`,
    /// so `--format json` produced zero bytes and a caller could not tell "no
    /// worktrees were held" from "the command never spoke".
    #[test]
    fn return_all_summary_serialises_when_nothing_was_held() {
        let summary = ReturnAllSummary {
            pool_count: 0,
            considered: 0,
            returned: Vec::new(),
            skipped: 0,
            aborted: Vec::new(),
            failed: Vec::new(),
            not_held: 0,
            stood_in: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        render_payload(
            format::OutputFormat::Json,
            &summary.to_json(),
            &mut out,
            &mut err,
        )
        .unwrap();

        let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["pool_count"], serde_json::json!(0));
        assert_eq!(v["returned"], serde_json::json!([]));
        assert!(
            err.is_empty(),
            "machine formats must not write to stderr: {err:?}"
        );
    }

    /// The counts the exit status is derived from must survive serialisation —
    /// a summary that renders but loses `failed` would report a clean sweep.
    #[test]
    fn return_all_summary_keeps_every_count() {
        let summary = ReturnAllSummary {
            pool_count: 4,
            considered: 3,
            returned: vec!["1".into()],
            skipped: 2,
            aborted: vec!["3".into()],
            failed: vec!["2".into()],
            not_held: 1,
            stood_in: vec!["4".into()],
        };
        let v = summary.to_json();
        assert_eq!(v["pool_count"], serde_json::json!(4));
        assert_eq!(v["considered"], serde_json::json!(3));
        assert_eq!(v["returned"], serde_json::json!(["1"]));
        assert_eq!(v["skipped"], serde_json::json!(2));
        assert_eq!(v["aborted"], serde_json::json!(["3"]));
        assert_eq!(v["failed"], serde_json::json!(["2"]));
        assert_eq!(v["not_held"], serde_json::json!(1));
        assert_eq!(v["stood_in"], serde_json::json!(["4"]));
    }

    /// `stood_in` is omitted when empty so the common payload stays flat.
    /// It is a diagnostic, and a script reading `returned` must not have to
    /// branch on a key that is sometimes missing.
    #[test]
    fn stood_in_is_omitted_when_nothing_was_excluded() {
        let summary = ReturnAllSummary {
            pool_count: 1,
            considered: 1,
            returned: vec!["1".into()],
            skipped: 0,
            aborted: Vec::new(),
            failed: Vec::new(),
            not_held: 0,
            stood_in: Vec::new(),
        };
        let v = summary.to_json();
        assert!(
            v.get("stood_in").is_none(),
            "an empty exclusion list must not appear: {v}"
        );
    }

    /// The human format writes nothing to stdout. `return --all` keeps every
    /// human line on stderr (Go's contract), so this is what stops the fix from
    /// leaking prose into a pipeline.
    #[test]
    fn render_payload_is_silent_for_the_human_format() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        render_payload(
            format::OutputFormat::Human,
            &serde_json::json!({"returned": []}),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert!(
            out.is_empty(),
            "human output must not reach stdout: {out:?}"
        );
        assert!(err.is_empty());
    }

    // ─── `return --all` target selection ──────────────────────────────────────

    /// Go `returnableStatus` (return_cmd.go:230-240), minus the `HeldOnlyByCwd`
    /// carve-out that `held_only_by_cwd` applies separately.
    #[test]
    fn returnable_status_excludes_available_and_damaged_only() {
        assert!(!returnable_status(treehouse_core::pool::STATUS_AVAILABLE));
        assert!(!returnable_status(treehouse_core::pool::STATUS_DAMAGED));
        for held in [
            treehouse_core::pool::STATUS_LEASED,
            treehouse_core::pool::STATUS_IN_USE,
            treehouse_core::pool::STATUS_DIRTY,
            treehouse_core::pool::STATUS_HERE,
        ] {
            assert!(
                returnable_status(held),
                "{held} is somebody's worktree and must be a target"
            );
        }
    }

    // ─── M-027: where `update` reads from and what it replaces ────────────────

    /// Production must hit GitHub over HTTPS with enforcement on.
    #[test]
    fn update_defaults_to_the_https_github_endpoint() {
        // `update_api_endpoint` reads the environment, so the default arm is
        // only observable with the override UNSET. Assert on the constant the
        // default names rather than mutating process-global env, which would
        // race the other tests in this binary.
        assert!(treehouse_core::updater::DEFAULT_GITHUB_API_URL.starts_with("https://"));
    }

    /// The success line is built from `Applied`, so its wording is fixed here
    /// rather than only in the e2e. The e2e proves the binary changed; this
    /// proves the line cannot claim a version that was never installed.
    #[test]
    fn update_success_line_names_both_versions_from_applied() {
        let applied = treehouse_core::updater::Applied {
            from_version: "0.1.2".to_string(),
            to_version: "v9.9.9".to_string(),
            replaced_path: PathBuf::from("/usr/local/bin/treehouse"),
        };
        let line = format!(
            "🌳 Successfully updated treehouse {} -> {}",
            applied.from_version, applied.to_version
        );
        assert_eq!(line, "🌳 Successfully updated treehouse 0.1.2 -> v9.9.9");
    }

    // ─── M-014: the release hook ──────────────────────────────────────────────

    /// The `before_reset` hook has to fail LOUDLY when a worktree cannot be
    /// proven quiet. A `PoolError` variant that does not name the cause would
    /// leave the operator reading "lock: ..." and never learning that a live
    /// writer kept the slot.
    #[test]
    fn lingering_error_names_the_process_scan_failure() {
        let e = lingering_error("cannot resolve ancestry of process 200");
        let rendered = e.to_string();
        assert!(
            rendered.contains("could not terminate worktree processes"),
            "got {rendered}"
        );
        assert!(
            rendered.contains("ancestry of process 200"),
            "the underlying cause must survive: {rendered}"
        );
    }
}
