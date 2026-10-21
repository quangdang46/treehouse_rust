//! E2E coverage for the CLI contracts that were silently broken.
//!
//! Each test here fails if the behaviour it names is removed:
//! * the `enter` tests read stderr, so an `enter` that prints nothing — or
//!   prints only the exit banner — fails them;
//! * the sweep tests name a pool root, so a sweep that ignores `--root` and
//!   reaches for the user-level root instead fails them;
//! * the git-error test reads the message, so a label that names the program
//!   twice fails it.

#[path = "e2e/common.rs"]
mod common;

use std::path::Path;

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
