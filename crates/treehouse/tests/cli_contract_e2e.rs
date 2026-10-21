//! E2E coverage for the CLI contracts that were silently broken.
//!
//! Each test here fails if the behaviour it names is removed:
//! * the `enter` tests read stderr, so an `enter` that prints nothing — or
//!   prints only the exit banner — fails them;
//! * the sweep tests name a pool root, so a sweep that ignores `--root` and
//!   reaches for the user-level root instead fails them;
//! * the git-error test reads the message, so a label that names the program
//!   twice fails it;
//! * the pool-resolution tests run from inside a pooled worktree and from
//!   outside the repository, so a pool resolved from the caller's working
//!   directory instead of from the worktree fails them.

#[path = "e2e/common.rs"]
mod common;

use std::path::Path;

/// A repository with NO remote, plus an isolated HOME.
///
/// The shared harness gives every repo a bare remote, and a remote is what
/// keys the pool directory — so with one, every checkout of a repository
/// shares a pool and the wrong-pool bug hides. Without a remote the pool is
/// keyed on the repository's path, which is exactly the case where
/// resolving the pool from the wrong root shows up.
fn setup_without_remote() -> (std::path::PathBuf, std::path::PathBuf) {
    let base = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let base = base.path().to_path_buf();
    #[cfg(not(windows))]
    let base = std::fs::canonicalize(&base).unwrap_or(base);
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let repo = base.join("repo");
    common::git(
        None,
        &["init", "--initial-branch=main", repo.to_str().unwrap()],
    );
    common::git(Some(&repo), &["config", "user.email", "test@test.com"]);
    common::git(Some(&repo), &["config", "user.name", "Test"]);
    std::fs::write(repo.join("README.md"), b"hello\n").unwrap();
    common::git(Some(&repo), &["add", "."]);
    common::git(Some(&repo), &["commit", "-m", "initial commit"]);
    (repo, home)
}

/// The worktree path `get --lease` handed back.
fn worktree_path(out: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(out.trim())
}

/// `enter` says WHERE it dropped the caller.
///
/// Regression: `enter` printed only "Pool state unchanged." and never said
/// which worktree it entered, so a caller had a changed working directory
/// and no way to tell which pool entry they were in.
#[test]
fn e2e_enter_announces_the_worktree_it_entered() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    let (_out, err, code) =
        common::run(&bin, &repo, &home, &[("SHELL", "/bin/sh")], &["enter", "1"]);

    assert_eq!(code, 0, "enter failed: {err}");
    assert!(
        err.contains("Entered worktree 1 at"),
        "enter must name the worktree it entered, got {err:?}"
    );
    assert!(
        err.contains("Left worktree"),
        "enter must say it left the worktree, got {err:?}"
    );
}

/// `enter` on an unknown name lists the names that DO exist.
///
/// Regression: the not-found error always said "the pool is empty", even for
/// a pool with slots in it — which sent the caller off to create another one
/// instead of correcting the name.
#[test]
fn e2e_enter_lists_the_available_names_when_the_name_is_unknown() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    let (_out, err, code) = common::run(&bin, &repo, &home, &[], &["enter", "99"]);

    assert_ne!(code, 0, "enter on an unknown name must fail");
    assert!(
        !err.contains("the pool is empty"),
        "the pool is NOT empty; the error must not claim it is: {err:?}"
    );
    assert!(
        err.contains("available: 1"),
        "the error must list the slots that exist, got {err:?}"
    );
}

/// `enter` on an unknown name in an EMPTY pool says so.
///
/// The two messages are different on purpose: an empty pool needs "run
/// `treehouse get`", a populated one needs the list of names.
#[test]
fn e2e_enter_says_the_pool_is_empty_when_it_is() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let (_out, err, code) = common::run(&bin, &repo, &home, &[], &["enter", "1"]);

    assert_ne!(code, 0, "enter on an unknown name must fail");
    assert!(
        err.contains("the pool is empty"),
        "an empty pool must be reported as empty, got {err:?}"
    );
}

/// `watch --root <dir>` sweeps THAT root, not the user-level one.
///
/// Regression: `--root` was accepted and ignored, so a sweep pointed at a
/// test root still swept the real `~/.treehouse` — the wrong pools, on the
/// user's own machine.
#[test]
fn e2e_watch_honours_the_root_flag() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    // A second root, holding a pool of its own.
    let other_root = home.join("other-root");
    std::fs::create_dir_all(&other_root).unwrap();

    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["watch", "--once", "--root", other_root.to_str().unwrap()],
    );
    assert_eq!(code, 0, "watch failed: {err}");

    // The user-level root must NOT appear in the sweep: it is a different
    // tree, and reaching into it is the bug.
    let user_root = home.join(".treehouse");
    assert!(
        !out.contains(user_root.to_str().unwrap_or("\u{0}"))
            && !err.contains(user_root.to_str().unwrap_or("\u{0}")),
        "the sweep reached the user-level root ({user_root:?}) despite --root: {out:?} / {err:?}"
    );
}

/// `gc --all --root <dir>` sweeps THAT root, not the user-level one.
#[test]
fn e2e_gc_all_honours_the_root_flag() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let other_root = home.join("other-root");
    std::fs::create_dir_all(&other_root).unwrap();

    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["gc", "--all", "--root", other_root.to_str().unwrap()],
    );
    assert_eq!(code, 0, "gc --all failed: {err}");

    let user_root = home.join(".treehouse");
    assert!(
        !out.contains(user_root.to_str().unwrap_or("\u{0}"))
            && !err.contains(user_root.to_str().unwrap_or("\u{0}")),
        "the sweep reached the user-level root ({user_root:?}) despite --root: {out:?} / {err:?}"
    );
}

/// `prune --all --root <dir>` sweeps THAT root, not the user-level one.
#[test]
fn e2e_prune_all_honours_the_root_flag() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let other_root = home.join("other-root");
    std::fs::create_dir_all(&other_root).unwrap();

    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["prune", "--all", "--root", other_root.to_str().unwrap()],
    );
    assert_eq!(code, 0, "prune --all failed: {err}");

    let user_root = home.join(".treehouse");
    assert!(
        !out.contains(user_root.to_str().unwrap_or("\u{0}"))
            && !err.contains(user_root.to_str().unwrap_or("\u{0}")),
        "the sweep reached the user-level root ({user_root:?}) despite --root: {out:?} / {err:?}"
    );
}

/// A git failure names the program ONCE.
///
/// Regression: `GitError`'s Display spells the program itself, so a caller
/// that spelled it into the label too rendered `git git rev-parse
/// --show-toplevel: …`.
#[test]
fn e2e_a_git_failure_names_the_program_once() {
    let home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let outside = home.path().join("not-a-repo");
    std::fs::create_dir_all(&outside).unwrap();
    let bin = common::treehouse_bin();

    let (_out, err, code) = common::run(&bin, &outside, home.path(), &[], &["status"]);

    assert_ne!(code, 0, "status outside a repository must fail");
    assert!(
        !err.contains("git git"),
        "the git program is named twice: {err:?}"
    );
    assert!(
        err.contains("git rev-parse --show-toplevel"),
        "unexpected error: {err:?}"
    );
}

/// `treehouse get` on a repository with a submodule works even when the
/// operator's global git config turns submodule recursion on.
///
/// Regression: the pooled git operations walked into submodules and died with
/// "fatal: could not reset submodule index", so `treehouse get` failed
/// outright on any repository with a submodule (upstream #177).
#[cfg(not(windows))]
#[test]
fn e2e_get_survives_a_global_submodule_recurse_setting() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    // A repository with a submodule.
    let sub = home.join("sub");
    common::git(
        None,
        &["init", "--initial-branch=main", sub.to_str().unwrap()],
    );
    common::git(Some(&sub), &["config", "user.email", "test@test.com"]);
    common::git(Some(&sub), &["config", "user.name", "Test"]);
    std::fs::write(sub.join("f.txt"), b"submodule\n").unwrap();
    common::git(Some(&sub), &["add", "f.txt"]);
    common::git(Some(&sub), &["commit", "-m", "submodule content"]);
    common::git(
        Some(&repo),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub.to_str().unwrap(),
            "vendor/libfoo",
        ],
    );
    common::git(Some(&repo), &["commit", "-m", "add submodule"]);

    // A global config that turns submodule recursion ON.
    let gitconfig = home.join("gitconfig");
    std::fs::write(&gitconfig, b"[submodule]\n\trecurse = true\n").unwrap();

    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[
            ("GIT_CONFIG_GLOBAL", gitconfig.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ],
        &["get", "--lease"],
    );

    assert_eq!(
        code, 0,
        "get failed on a repository with a submodule: {err}"
    );
    assert!(
        Path::new(out.trim()).exists(),
        "get must hand back a worktree that exists, got {out:?}"
    );
}

/// `status` from inside a pooled worktree reports THAT pool.
///
/// Regression: the pool was resolved from the working directory, and inside
/// a linked worktree `git rev-parse --show-toplevel` answers with the
/// worktree itself — so a repository with no remote (where the pool is keyed
/// on the repository's path) resolved a DIFFERENT, empty pool. The caller
/// standing in the worktree saw "No worktrees in pool" while the pool held
/// the worktree they were standing in.
#[test]
fn e2e_status_from_inside_a_worktree_reports_the_right_pool() {
    let (repo, home) = setup_without_remote();
    let bin = common::treehouse_bin();

    let (out, _err, code) = common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    assert_eq!(code, 0, "get failed");
    let wt = worktree_path(&out);

    // The human status table goes to stdout; stderr carries the banners.
    let (out, err, code) = common::run_from(&bin, &repo, &wt, &home, &[], &["status"]);
    assert_eq!(code, 0, "status from inside the worktree failed: {err}");
    assert!(
        out.contains(&wt.to_string_lossy().to_string())
            || out.contains(wt.file_name().unwrap().to_str().unwrap()),
        "status must report the pool the worktree belongs to, got {out:?}"
    );
    assert!(
        !out.contains("No worktrees in pool") && !err.contains("No worktrees in pool"),
        "the pool holding this worktree is not empty: {out:?} / {err:?}"
    );
}

/// `return` from inside a pooled worktree returns THAT worktree.
///
/// This is the `TREEHOUSE_DIR` default: `get` sets it before spawning the
/// shell, so `treehouse return` with no argument is how a caller hands the
/// worktree back from inside it.
#[test]
fn e2e_return_from_inside_a_worktree_returns_that_worktree() {
    let (repo, home) = setup_without_remote();
    let bin = common::treehouse_bin();

    let (out, _err, code) = common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    assert_eq!(code, 0, "get failed");
    let wt = worktree_path(&out);

    let (_out, err, code) = common::run_from(
        &bin,
        &repo,
        &wt,
        &home,
        &[("TREEHOUSE_DIR", wt.to_str().unwrap())],
        &["return"],
    );
    assert_eq!(code, 0, "return from inside the worktree failed: {err}");

    let (out, _err, _code) = common::run(&bin, &repo, &home, &[], &["status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        v[0]["status"], "available",
        "the worktree must be back in the pool: {out}"
    );
}

/// `return <path>` works from outside the repository entirely.
///
/// Regression: the pool was resolved from the caller's working directory, so
/// returning a worktree from outside any checkout failed with "not a git
/// repository" — the one case where naming a path is most useful.
#[test]
fn e2e_return_by_path_works_from_outside_the_repository() {
    let (repo, home) = setup_without_remote();
    let bin = common::treehouse_bin();

    let (out, _err, code) = common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    assert_eq!(code, 0, "get failed");
    let wt = worktree_path(&out);

    // A directory that is not a repository at all.
    let outside = home.join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    let (_out, err, code) = common::run_from(
        &bin,
        &repo,
        &outside,
        &home,
        &[],
        &["return", wt.to_str().unwrap()],
    );
    assert_eq!(code, 0, "return by path failed: {err}");

    let (out, _err, _code) = common::run(&bin, &repo, &home, &[], &["status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        v[0]["status"], "available",
        "the worktree must be back in the pool: {out}"
    );
}

/// `return <slot-name>` still resolves the NAME, not a path.
///
/// Regression guard for the pool-resolution change: the argument is read as a
/// path FIRST and only then as a slot name, so a bare "1" must still find
/// slot 1. Reading "1" as a path and giving up when no pool claims it would
/// break the form every agent uses.
#[test]
fn e2e_return_by_slot_name_still_resolves_the_name() {
    let (repo, home) = setup_without_remote();
    let bin = common::treehouse_bin();

    common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    let (_out, err, code) = common::run(&bin, &repo, &home, &[], &["return", "1"]);
    assert_eq!(code, 0, "return by slot name failed: {err}");

    let (out, _err, _code) = common::run(&bin, &repo, &home, &[], &["status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(
        v[0]["status"], "available",
        "slot 1 must be back in the pool: {out}"
    );
}

/// `return <slot-name>` works from inside a pooled worktree too.
#[test]
fn e2e_return_by_slot_name_works_from_inside_a_worktree() {
    let (repo, home) = setup_without_remote();
    let bin = common::treehouse_bin();

    common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    let (out, _err, _code) = common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    let wt = worktree_path(&out);

    let (_out, err, code) = common::run_from(&bin, &repo, &wt, &home, &[], &["return", "1"]);
    assert_eq!(code, 0, "return by slot name failed: {err}");
}
