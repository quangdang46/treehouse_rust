//! End-to-end proof that `treehouse update` is actually wired.
//!
//! `updater::apply_into` had a complete, tested download → verify → extract →
//! replace pipeline and NO production caller: `cmd_update` read a cache, printed
//! "Successfully updated treehouse X -> Y", and left the binary on disk
//! untouched. Every test here drives the real binary against a LOCAL
//! `file://` release fixture — never the network — and asserts on the BYTES of
//! the installed target, because a run that reports success while changing
//! nothing passes every assertion except that one.
//!
//! The fixture is pointed at by `TREEHOUSE_UPDATE_API_URL` and
//! `TREEHOUSE_UPDATE_TARGET` (see `update_api_endpoint` / `update_target` in
//! `main.rs`). Without the target override the test would replace the very
//! `target/debug/treehouse` every other test in this suite runs.

#![cfg(not(windows))] // the fixture archive is tar.gz; Windows ships .zip

#[path = "e2e/common.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

/// The payload the fixture release "contains". Distinctive so a target that was
/// never replaced cannot be confused with one that was.
const NEW_BINARY: &[u8] = b"#!/bin/sh\n# treehouse 9.9.9 fixture payload\n";

/// Builds a release fixture: the archive, its checksum sidecar, and the
/// GitHub-shaped `latest.json` pointing at both over `file://`.
struct ReleaseFixture {
    dir: PathBuf,
    latest_json: PathBuf,
}

impl ReleaseFixture {
    /// `tag` names the release; `archive_bytes` is what the archive contains.
    /// Pass a mismatching `checksum` to model a corrupted download.
    fn build(dir: &Path, tag: &str, archive_bytes: &[u8], checksum: Option<&str>) -> Self {
        let stage = dir.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("treehouse"), archive_bytes).unwrap();

        let asset = format!("treehouse-{tag}-{}-{}.tar.gz", os(), arch());
        let archive = dir.join(&asset);
        let out = Command::new("tar")
            .args(["-czf", archive.to_str().unwrap(), "-C"])
            .arg(&stage)
            .arg("treehouse")
            .output()
            .expect("tar must be installed");
        assert!(
            out.status.success(),
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let digest = checksum
            .map(str::to_string)
            .unwrap_or_else(|| sha256(&archive));
        let sidecar = dir.join(format!("{asset}.sha256"));
        std::fs::write(&sidecar, format!("{digest}  {asset}\n")).unwrap();

        let latest_json = dir.join("latest.json");
        let body = format!(
            r#"{{"tag_name":"{tag}","assets":[
                 {{"name":"{asset}","browser_download_url":"file://{archive}"}},
                 {{"name":"{asset}.sha256","browser_download_url":"file://{sidecar}"}}]}}"#,
            tag = tag,
            asset = asset,
            archive = archive.display(),
            sidecar = sidecar.display(),
        );
        std::fs::write(&latest_json, body).unwrap();
        Self {
            dir: dir.to_path_buf(),
            latest_json,
        }
    }

    fn url(&self) -> String {
        format!("file://{}", self.latest_json.display())
    }
}

/// Runs `treehouse update` with the fixture wired in and `target` as the binary
/// to replace. Returns (stdout, stderr, exit code).
fn run_update(fixture: &ReleaseFixture, home: &Path, target: &Path) -> (String, String, i32) {
    let env = [
        ("HOME", home.display().to_string()),
        ("TREEHOUSE_NO_UPDATE_CHECK", "1".to_string()),
        ("TREEHOUSE_UPDATE_API_URL", fixture.url()),
        ("TREEHOUSE_UPDATE_TARGET", target.display().to_string()),
    ];
    let out = Command::new(target)
        .arg("update")
        .envs(env)
        .output()
        .expect("failed to run treehouse update");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// A fresh copy of the built binary standing in for an installed release.
/// Leaked like `common::setup` does, so it outlives the assertion helpers.
fn install_target(name: &str) -> PathBuf {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let target = dir.path().join(name);
    std::fs::copy(common::treehouse_bin(), &target).unwrap();
    target
}

fn os() -> &'static str {
    std::env::consts::OS
}

fn arch() -> &'static str {
    std::env::consts::ARCH
}

fn sha256(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256", path.to_str().unwrap()])
        .output()
        .expect("shasum must be installed");
    assert!(
        out.status.success(),
        "shasum failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("shasum prints a digest")
        .to_string()
}

/// THE regression test for M-027: `treehouse update` replaces the binary.
///
/// Delete the `apply_into` call from `cmd_update` and this fails on the byte
/// comparison — the old command printed the same success line while leaving the
/// installed file exactly as it found it.
#[test]
fn e2e_update_replaces_the_installed_binary() {
    let home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let fixture_dir = home.path().join("release");
    std::fs::create_dir_all(&fixture_dir).unwrap();
    let fixture = ReleaseFixture::build(&fixture_dir, "v9.9.9", NEW_BINARY, None);

    let target = install_target("treehouse");
    let (out, err, code) = run_update(&fixture, home.path(), &target);

    assert_eq!(code, 0, "update failed: {err}");
    assert!(
        out.contains("Successfully updated treehouse"),
        "expected the success line, got stdout {out:?} stderr {err:?}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        NEW_BINARY,
        "the installed binary was NOT replaced — `update` reported success over an \
         untouched file, which is exactly the M-027 defect"
    );
}

/// The success line is printed from what `apply_into` reports, so it cannot
/// describe a version that was never installed.
#[test]
fn e2e_update_reports_the_versions_it_actually_installed() {
    let home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let fixture_dir = home.path().join("release");
    std::fs::create_dir_all(&fixture_dir).unwrap();
    let fixture = ReleaseFixture::build(&fixture_dir, "v9.9.9", NEW_BINARY, None);

    let target = install_target("treehouse");
    let (out, _err, code) = run_update(&fixture, home.path(), &target);
    assert_eq!(code, 0);
    assert!(
        out.contains("0.1.2 -> v9.9.9"),
        "the success line must name both versions, got {out:?}"
    );
}

/// A release at or below the running version installs nothing and says so.
#[test]
fn e2e_update_reports_up_to_date_without_touching_the_binary() {
    let home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let fixture_dir = home.path().join("release");
    std::fs::create_dir_all(&fixture_dir).unwrap();
    // 0.0.1 is below the 0.1.2 this suite builds, so there is nothing to do.
    let fixture = ReleaseFixture::build(&fixture_dir, "v0.0.1", NEW_BINARY, None);

    let target = install_target("treehouse");
    let before = std::fs::read(&target).unwrap();
    let (out, _err, code) = run_update(&fixture, home.path(), &target);

    assert_eq!(code, 0);
    assert!(
        out.contains("up to date"),
        "expected the up-to-date line, got {out:?}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "an up-to-date check must not rewrite the binary"
    );
}

/// A failed check is an `Err`, not a silent `Ok(0)`.
///
/// The old command printed "Could not check for updates (network unavailable)"
/// and returned success, so every script gating on the exit status read a dead
/// network as a finished update.
#[test]
fn e2e_update_fails_loudly_when_the_release_cannot_be_reached() {
    let home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let target = install_target("treehouse");
    let before = std::fs::read(&target).unwrap();

    let env = [
        ("HOME", home.path().display().to_string()),
        ("TREEHOUSE_NO_UPDATE_CHECK", "1".to_string()),
        (
            "TREEHOUSE_UPDATE_API_URL",
            format!("file://{}/no-such-release.json", home.path().display()),
        ),
        ("TREEHOUSE_UPDATE_TARGET", target.display().to_string()),
    ];
    let out = Command::new(&target)
        .arg("update")
        .envs(env)
        .output()
        .expect("failed to run treehouse update");

    assert_ne!(
        out.status.code(),
        Some(0),
        "an unreachable release must exit non-zero, not report success"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("checking for updates"),
        "the failure must say which step failed, got {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(&target).unwrap(), before);
}

/// A checksum mismatch aborts the update and leaves the installed binary alone.
///
/// Without this, a corrupted or substituted download replaces a working binary
/// with something that merely claims to be treehouse.
#[test]
fn e2e_update_refuses_a_release_whose_checksum_does_not_match() {
    let home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let fixture_dir = home.path().join("release");
    std::fs::create_dir_all(&fixture_dir).unwrap();
    let fixture = ReleaseFixture::build(
        &fixture_dir,
        "v9.9.9",
        NEW_BINARY,
        Some("0000000000000000000000000000000000000000000000000000000000000000"),
    );

    let target = install_target("treehouse");
    let before = std::fs::read(&target).unwrap();
    let (_out, err, code) = run_update(&fixture, home.path(), &target);

    assert_ne!(code, 0, "a checksum mismatch must not exit 0");
    assert!(
        err.contains("checksum"),
        "expected a checksum diagnosis, got {err:?}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "a failed verification must never touch the installed binary"
    );
}

/// The fixture directory is reachable through the fixture struct only — this
/// keeps the unused-field warning honest if the struct ever loses a field.
#[test]
fn fixture_dir_is_the_release_directory() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = ReleaseFixture::build(dir.path(), "v9.9.9", NEW_BINARY, None);
    assert!(fixture.dir.join("latest.json").exists());
    assert!(fixture.url().starts_with("file://"));
}
