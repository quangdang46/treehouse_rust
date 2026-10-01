//! E2E coverage for the bulk verbs and the two acquisition-flag regressions.
//!
//! Each test here fails if the behaviour it names is removed:
//! * the JSON tests parse stdout, so a command that prints nothing fails them;
//! * the `--base` / `--no-fetch` tests assert the resulting worktree contents, so
//!   a flag that parses but is dropped fails them;
//! * the `held_only_by_cwd` test writes a marker into the worktree and checks it
//!   survives, so a `return --all` that reclaims the caller's own slot fails it.

#[path = "e2e/common.rs"]
mod common;

use std::path::Path;

/// Parses stdout as a JSON document, failing with the raw text so a broken
/// contract names what was actually printed.
fn json(out: &str) -> serde_json::Value {
    serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON ({e}): {out:?}"))
}

// ─── M-018: the two paths that emitted zero bytes on stdout ───────────────────

/// `return --all --format json` on a pool with nothing held must still emit a
/// document. It shipped printing nothing at all, so a script reading stdout got
/// an empty string and could not tell "no worktrees were held" from "the command
/// died before it spoke".
#[test]
fn e2e_return_all_json_on_an_empty_pool() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["return", "--all", "--format", "json"],
    );
    assert_eq!(code, 0, "return --all failed: {err}");
    assert!(
        !out.trim().is_empty(),
        "return --all --format json wrote nothing to stdout"
    );

    let v = json(&out);
    assert_eq!(v["returned"], serde_json::json!([]));
    assert_eq!(v["pool_count"], serde_json::json!(0));
    assert_eq!(v["considered"], serde_json::json!(0));
}

/// `gc --all --format json` over a clean machine must emit a document. The empty
/// sweep used to return before the render, printing only a stderr banner.
#[test]
fn e2e_gc_all_json_on_an_empty_sweep() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["gc", "--all", "--format", "json"],
    );
    assert_eq!(code, 0, "gc --all failed: {err}");
    assert!(
        !out.trim().is_empty(),
        "gc --all --format json wrote nothing to stdout"
    );

    let v = json(&out);
    assert_eq!(v["candidates"], serde_json::json!(0));
    assert_eq!(v["errors"], serde_json::json!(0));
}

/// The human format keeps its banner on stderr and still says nothing on stdout,
/// so the fix did not just move prose into the machine channel.
#[test]
fn e2e_gc_all_human_banner_stays_on_stderr() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let (out, err, code) = common::run(&bin, &repo, &home, &[], &["gc", "--all"]);
    assert_eq!(code, 0, "gc --all failed: {err}");
    assert!(
        out.trim().is_empty(),
        "human format must keep stdout clean, got {out:?}"
    );
    assert!(
        err.contains("No stale worktrees to reclaim"),
        "expected the banner on stderr, got {err:?}"
    );
}

/// A sweep that actually returns worktrees names them, so the empty case above
/// is a real zero and not a constant.
#[test]
fn e2e_return_all_json_names_what_it_returned() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    common::run(&bin, &repo, &home, &[], &["get", "--lease"]);

    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["return", "--all", "--format", "json"],
    );
    assert_eq!(code, 0, "return --all failed: {err}");

    let v = json(&out);
    assert_eq!(
        v["returned"].as_array().map(|a| a.len()),
        Some(2),
        "both leased slots must be returned, got {out}"
    );
    assert_eq!(v["pool_count"], serde_json::json!(2));
}

// ─── M-018 regression: `return --all` lost the HeldOnlyByCwd exclusion ────────

/// Leases `n` slots, then returns the FIRST and leaves the rest held.
///
/// Every slot must be leased BEFORE any is returned: the acquire loop reuses
/// any available slot, so a `get` issued after a `return` is handed the same
/// slot back rather than creating a new one.
fn lease_n_return_first(
    repo: &std::path::Path,
    home: &std::path::Path,
    n: usize,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    Vec<std::path::PathBuf>,
) {
    let bin = common::treehouse_bin();
    let mut paths = Vec::new();
    for _ in 0..n {
        let (out, err, code) = common::run(&bin, repo, home, &[], &["get", "--lease"]);
        assert_eq!(code, 0, "get --lease failed: {err}");
        paths.push(std::path::PathBuf::from(out.trim()));
    }
    let first = paths.remove(0);
    let (_o, err, code) = common::run(
        &bin,
        repo,
        home,
        &[],
        &["return", first.to_str().unwrap(), "--force"],
    );
    assert_eq!(code, 0, "returning the first slot failed: {err}");
    (bin, first, paths)
}

/// Writes a file into `wt` and COMMITS it, leaving the worktree clean.
///
/// Clean matters: Go's `HeldOnlyByCwd` is set only when the classification the
/// overlay hides was `available`, so a DIRTY slot the caller stands in is a
/// legitimate `--all` target (design.md says so explicitly). An untracked
/// marker would make the slot dirty and the test would be asserting the
/// opposite of the contract.
///
/// A committed file is the right probe because the release resets to the base
/// branch: if the sweep really did reclaim this slot, the commit disappears.
fn commit_marker(wt: &Path, name: &str) {
    std::fs::write(wt.join(name), b"only on this commit\n").unwrap();
    common::git(Some(wt), &["add", name]);
    common::git(Some(wt), &["commit", "-m", name]);
}

/// `return --all` run from inside a parked slot must leave that slot alone.
///
/// The caller's own shell is in the slot's process list, so this port's
/// `status` reports `you're here` for a slot nobody is holding. Treating that
/// as held resets the worktree under the user running the sweep.
#[test]
fn e2e_return_all_skips_the_slot_the_caller_stands_in() {
    let (repo, home) = common::setup();
    let (bin, path, _still_leased) = lease_n_return_first(&repo, &home, 1);

    // Committed, so the slot stays clean: a dirty slot the caller stands in IS a
    // target by design, and this test is about the clean one.
    commit_marker(&path, "STANDING-IN-MARKER");
    let marker = path.join("STANDING-IN-MARKER");

    let (out, err, code) = common::run_from(
        &bin,
        &repo,
        &path,
        &home,
        &[],
        &["return", "--all", "--force", "--format", "json"],
    );
    assert_eq!(code, 0, "return --all failed: {err}");

    let v = json(&out);
    assert_eq!(
        v["returned"],
        serde_json::json!([]),
        "the caller's own slot must not be reclaimed: {out}"
    );
    assert_eq!(
        v["not_held"],
        serde_json::json!(1),
        "the stood-in slot must be counted as not held: {out}"
    );
    assert_eq!(
        v["stood_in"],
        serde_json::json!(["1"]),
        "the exclusion must be reported, not silent: {out}"
    );
    assert!(
        marker.exists(),
        "the worktree the caller stands in was reset to the base branch out from \
         under them, losing its committed work"
    );
}

/// The exclusion is narrow: other held slots in the same sweep are still
/// returned. Otherwise `--all` would stop reclaiming a pool the moment anyone
/// happened to be standing in one of its slots.
#[test]
fn e2e_return_all_still_returns_other_slots_while_you_stand_in_one() {
    let (repo, home) = common::setup();
    // One parked slot to stand in, one genuinely leased slot the same sweep
    // must still reclaim.
    let (bin, here, still_leased) = lease_n_return_first(&repo, &home, 2);
    let other = still_leased[0].clone();

    let (out, err, code) = common::run_from(
        &bin,
        &repo,
        &here,
        &home,
        &[],
        &["return", "--all", "--force", "--format", "json"],
    );
    assert_eq!(code, 0, "return --all failed: {err}");

    let v = json(&out);
    assert_eq!(
        v["returned"],
        serde_json::json!(["2"]),
        "the leased slot must still be returned: {out}"
    );

    // The named target really was released.
    let (sout, _serr, _code) = common::run(&bin, &repo, &home, &[], &["status", "--json"]);
    let statuses = json(&sout);
    let released = statuses
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["path"] == other.to_str().unwrap())
        .unwrap();
    assert_eq!(
        released["status"], "available",
        "slot 2 must be back in the pool"
    );
}

// ─── Regression: `get --lease --base X` and `--no-fetch` were dropped ─────────

/// `--lease` must cut the worktree from the requested branch. It used to route
/// straight to `acquire_lease_with_ttl`, which built `AcquireOptions` with only
/// the `lease` field set, so `--base` parsed and was silently ignored.
#[test]
fn e2e_get_lease_honours_the_base_branch() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    // A branch whose content differs from the default, pushed so the pool can
    // resolve it as a remote-tracking ref.
    common::git(Some(&repo), &["checkout", "-b", "feature"]);
    std::fs::write(repo.join("FEATURE-ONLY.txt"), b"feature\n").unwrap();
    common::git(Some(&repo), &["add", "."]);
    common::git(Some(&repo), &["commit", "-m", "feature"]);
    common::git(Some(&repo), &["push", "-u", "origin", "feature"]);
    common::git(Some(&repo), &["checkout", "main"]);

    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["get", "--lease", "--base", "feature"],
    );
    assert_eq!(code, 0, "get --lease --base failed: {err}");

    let path = std::path::PathBuf::from(out.trim());
    assert!(
        path.join("FEATURE-ONLY.txt").exists(),
        "--base was ignored: the worktree is at the default branch, not `feature`"
    );
}

/// `--no-fetch` must reach the acquire in lease mode too. With the origin
/// unreachable, `--no-fetch` has to succeed and its absence has to fail.
#[test]
fn e2e_get_lease_honours_no_fetch() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    // Point origin at a path that does not exist: every fetch now fails.
    let gone = home.join("no-such-remote.git");
    common::git(
        Some(&repo),
        &["remote", "set-url", "origin", gone.to_str().unwrap()],
    );

    let (_out, err, code) = common::run(&bin, &repo, &home, &[], &["get", "--lease", "--no-fetch"]);
    assert_eq!(code, 0, "--no-fetch must skip the unreachable fetch: {err}");

    // The same acquisition without the flag must fail, which is what makes the
    // assertion above evidence that the flag did something.
    let (_out, _err, code) = common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    assert_ne!(
        code, 0,
        "without --no-fetch the unreachable origin must fail"
    );
}

// ─── M-014: the nonzero-shell-exit path used to be a no-op ───────────────────

/// A `get` subshell that exits nonzero must still hand the slot back.
///
/// The branch used to call a stub that returned `Ok(())`, so the slot kept its
/// reservation stamp, its branch, and its dirty tree with nothing to explain
/// why. Go detaches and returns on every exit path.
#[cfg(not(windows))]
#[test]
fn e2e_get_returns_the_slot_when_the_subshell_exits_nonzero() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let shell = home.join("fail.sh");
    std::fs::write(&shell, "#!/bin/sh\nexit 3\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Interactive `get` writes nothing to stdout; the subshell inherits it.
    let (_out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[("SHELL", shell.to_str().unwrap())],
        &["get"],
    );
    assert_eq!(code, 0, "get failed: {err}");
    assert!(
        err.contains("Worktree returned to pool."),
        "the slot must be returned to the pool, got {err:?}"
    );

    // The slot is parked again rather than stranded as `in-use`.
    let (sout, _serr, _code) = common::run(&bin, &repo, &home, &[], &["status", "--json"]);
    let statuses = json(&sout);
    let slot = &statuses.as_array().unwrap()[0];
    assert_eq!(
        slot["status"], "available",
        "the slot must be back in the pool: {sout}"
    );

    // And its HEAD is detached, which is the half the stub never did.
    let path = slot["path"].as_str().unwrap();
    let head = std::process::Command::new("git")
        .args(["-C", path, "symbolic-ref", "-q", "HEAD"])
        .output()
        .unwrap();
    assert!(
        !head.status.success(),
        "the returned slot must be left detached, as `git checkout --detach` leaves it"
    );
}
