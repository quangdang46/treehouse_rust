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
