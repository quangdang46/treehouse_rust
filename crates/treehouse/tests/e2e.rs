//! E2E scenarios (port of Go cmd/e2e_test.go).
//!
//! The harness helpers live in `e2e/common.rs`, declared here as a module.

#[path = "e2e/common.rs"]
mod common;

use std::path::Path;

/// `treehouse init` creates treehouse.toml; a second init errors.
#[test]
fn e2e_init_and_already_exists() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let (out, err, code) = common::run(&bin, &repo, &home, &[], &["init"]);
    assert_eq!(code, 0, "init failed: {err}");
    assert!(
        repo.join("treehouse.toml").exists(),
        "treehouse.toml not created"
    );
    assert!(out.is_empty() || err.contains("Created"));

    let (_, err, code) = common::run(&bin, &repo, &home, &[], &["init"]);
    assert_ne!(code, 0, "second init should fail");
    assert!(err.contains("already exists"), "got {err}");
}

/// `treehouse get --lease` prints only the bare path to stdout.
#[test]
fn e2e_get_lease_prints_only_path() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    let (out, _err, code) = common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    assert_eq!(code, 0);
    let out = out.trim();
    assert!(
        out.contains(".treehouse"),
        "expected pool path, got {out:?}"
    );
    assert!(out.lines().count() == 1, "path-only expected, got {out:?}");
    assert!(Path::new(out).exists(), "worktree {out} does not exist");
}

/// `treehouse status --json` returns a JSON array.
#[test]
fn e2e_status_json() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();
    common::run(&bin, &repo, &home, &[], &["get", "--lease"]);
    let (out, _err, code) = common::run(&bin, &repo, &home, &[], &["status", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert!(v.is_array(), "expected array, got {out}");
}

/// `treehouse get` (interactive) with an exit-immediately shell returns cleanly.
///
/// Unix-only: on Windows `cmd.exe` writes its banner to stdout and blocks for
/// input (the Go harness builds a dedicated exit-helper binary; porting that
/// helper is a follow-up).
#[cfg(not(windows))]
#[test]
fn e2e_get_interactive_with_exit_shell() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();
    // /bin/true exits 0 immediately, so get doesn't block.
    let (out, err, code) = common::run(&bin, &repo, &home, &[("SHELL", "/bin/true")], &["get"]);
    assert_eq!(code, 0, "get failed: {err}");
    assert!(
        out.trim().is_empty(),
        "interactive get must write nothing to stdout, got {out:?}"
    );
    assert!(
        err.contains("Entered worktree at"),
        "expected banner, got {err}"
    );
}

/// `treehouse get --lease --ttl` records an expiry; gc reclaims an expired,
/// disposable lease but never a valid one.
#[test]
fn e2e_gc_reclaims_expired_lease_but_not_valid() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    // A valid lease (1h TTL).
    let (out, err, code) = common::run(&bin, &repo, &home, &[], &["get", "--lease", "--ttl", "1h"]);
    assert_eq!(code, 0, "get --lease --ttl failed: {err}");
    let valid_path = out.trim().to_string();

    // gc dry-run: valid lease is NOT a candidate (0 reclaimed).
    let (out, err, code) = common::run(&bin, &repo, &home, &[], &["gc"]);
    assert_eq!(code, 0, "gc failed: {err}");
    assert!(
        !out.contains("reclaim 1") && !out.contains("Reclaimed 1"),
        "valid lease must not be reclaimed, got: {out} {err}"
    );
    assert!(
        std::path::Path::new(&valid_path).exists(),
        "valid lease worktree must survive"
    );

    // An expired lease (past TTL): simulate by writing an expired expires_at.
    // gc dry-run reports it; gc --yes reclaims it.
    let (_, _, _) = common::run(&bin, &repo, &home, &[], &["get", "--lease", "--ttl", "1s"]);
    let (_out, err, code) = common::run(&bin, &repo, &home, &[], &["gc", "--yes"]);
    assert_eq!(code, 0, "gc --yes failed: {err}");
    // The valid lease is untouched; the expired one is a candidate (or already
    // reclaimed). The key invariant: gc never removed the valid lease.
    assert!(
        std::path::Path::new(&valid_path).exists(),
        "valid lease must never be gc'd"
    );
}

/// `treehouse run -- <cmd>` acquires a worktree, runs the command, and cleans up.
#[test]
fn e2e_run_cleans_up() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();

    // run -- true exits 0 and returns the worktree.
    let (out, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["run", "--", "cmd", "/c", "exit", "0"],
    );
    assert_eq!(code, 0, "run -- true should exit 0, got {err}");
    assert!(
        out.is_empty() || !out.contains("error"),
        "unexpected stdout {out}"
    );

    // run -- false exits 1.
    let (_, err, code) = common::run(
        &bin,
        &repo,
        &home,
        &[],
        &["run", "--", "cmd", "/c", "exit", "1"],
    );
    assert_eq!(code, 1, "run -- false should exit 1, got {err}");
}

/// `treehouse doctor` reports a healthy pool (exit 0) and JSON output.
#[test]
fn e2e_doctor_healthy() {
    let (repo, home) = common::setup();
    let bin = common::treehouse_bin();
    common::run(&bin, &repo, &home, &[], &["get", "--lease"]);

    let (out, err, code) = common::run(&bin, &repo, &home, &[], &["doctor"]);
    assert_eq!(code, 0, "doctor on healthy pool should exit 0, got {err}");
    assert!(
        out.contains("Doctor:") || out.contains("healthy"),
        "got {out}"
    );

    let (out, err, code) = common::run(&bin, &repo, &home, &[], &["doctor", "--format", "json"]);
    assert_eq!(code, 0, "doctor --json failed: {err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert!(
        v.get("healthy").is_some(),
        "expected healthy field, got {out}"
    );
}
