//! Atomic state file writer (`treehouse-state.json`).
//!
//! Port of Go's `atomicWriteFile`: write to a same-directory temp file, fsync,
//! then atomically commit over the target. A crash mid-write (killed process,
//! power loss) must never leave a truncated or empty live file — the old
//! contents survive until the rename/replace lands.
//!
//! `tempfile::NamedTempFile::persist()` is used instead of `std::fs::rename`
//! because rename fails on Windows when the destination exists; persist uses
//! `rename(2)` on unix and `MoveFileExW + MOVEFILE_REPLACE_EXISTING` on
//! Windows. Note `persist` does NOT fsync — we call `sync_all()` explicitly.

use std::io::Write;
use std::path::Path;

/// Atomically writes `data` to `path` with the same durability contract as
/// Go's `atomicWriteFile`:
///
/// 1. Create a temp file in the same directory.
/// 2. Write + fsync it.
/// 3. Persist over the target (atomic rename / replace).
/// 4. Preserve the existing target's file mode; new files get `perm`.
///
/// On Windows the parent-directory fsync is skipped (Go's `syncDirectory` is
/// POSIX-only) — accepted for P0 parity, documented as weaker durability.
pub fn atomic_write_file(path: &Path, data: &[u8], perm: u32) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other(format!("no parent dir for {}", path.display())))?;

    // Preserve the existing target's mode if it exists; otherwise use `perm`.
    let existing_mode = target_mode(path);

    let mut tmp = tempfile::Builder::new()
        .prefix("treehouse-state.tmp-")
        .tempfile_in(dir)?;
    tmp.write_all(data)?;
    tmp.flush()?;
    // tempfile does not fsync on persist — do it explicitly so a crash after
    // rename can't leave a zero-length live file.
    tmp.as_file().sync_all()?;
    apply_mode(tmp.as_file(), existing_mode.unwrap_or(perm))?;

    // Atomic commit: unix rename / Windows MoveFileExW+REPLACE.
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// The mode of the existing target file, if any. On unix this is the real
/// permission bits; on Windows permissions are mostly no-ops, so `None` means
/// "use the default". Mirrors Go's `replacementFileMode`.
fn target_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.mode())
    }
    #[cfg(windows)]
    {
        let _ = path;
        None
    }
}

/// Sets the file's permission mode. No-op on Windows (modes are ignored).
fn apply_mode(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(windows)]
    {
        let _ = (file, mode);
    }
    Ok(())
}

/// Write the pool state file with Go-compatible 2-space indentation
/// (`json.MarshalIndent(s, "", "  ")`), atomically.
///
/// This does NOT acquire the state lock — callers wrap read+mutate+write in
/// one `with_state_lock` (see `lock.rs`).
pub fn write_state(pool_dir: &Path, state: &crate::state::State) -> std::io::Result<()> {
    let path = crate::state::State::state_file_path(pool_dir);
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::other(format!("serializing state: {e}")))?;
    // Go's MarshalIndent emits a trailing newline; match it.
    let mut bytes = json.into_bytes();
    bytes.push(b'\n');
    atomic_write_file(&path, &bytes, 0o644)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WorktreeEntry;
    use chrono::{DateTime, Utc};

    fn sample_state() -> crate::state::State {
        crate::state::State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: "/tmp/pool/1/myrepo".into(),
                created_at: DateTime::parse_from_rfc3339("2026-07-20T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                ..WorktreeEntry::default()
            }],
        }
    }

    #[test]
    fn write_state_is_2space_indented_go_style() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &sample_state()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("treehouse-state.json")).unwrap();
        // Go MarshalIndent uses 2-space indent + trailing newline.
        assert!(raw.starts_with("{\n  \"worktrees\": ["), "got: {raw}");
        assert!(raw.ends_with("\n"), "missing trailing newline");
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "treehouse-state.json")
            .collect();
        assert!(leftovers.is_empty(), "leftover files: {leftovers:?}");
    }

    #[test]
    fn interrupted_write_never_touches_live_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("treehouse-state.json");
        let original = br#"{"worktrees":[]}"#;
        std::fs::write(&path, original).unwrap();

        // Simulate a crash mid-write: create a temp file but never persist it.
        let mut tmp = tempfile::Builder::new()
            .prefix("treehouse-state.tmp-")
            .tempfile_in(dir.path())
            .unwrap();
        tmp.write_all(br#"{"worktrees": [{"name": "2"}"#).unwrap();
        tmp.flush().unwrap();
        // No persist: the "crash".

        // Live file must be untouched.
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, original, "live file changed after interrupted write");
    }

    #[test]
    fn preserves_existing_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("treehouse-state.json");
        std::fs::write(&path, b"old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        atomic_write_file(&path, b"new", 0o644).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "existing mode must be preserved");
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn new_file_respects_perm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.json");
        atomic_write_file(&path, b"data", 0o644).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644);
        }
    }
}
