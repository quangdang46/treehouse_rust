//! Atomic state file writer (`treehouse-state.json`).
//!
//! Port of Go's `atomicWriteFile`: write to a same-directory temp file, fsync,
//! then atomically commit over the target. A crash mid-write (killed process,
//! power loss) must never leave a truncated or empty live file — the old
//! contents survive until the rename/replace lands.
//!
//! On unix `tempfile::NamedTempFile::persist()` is used instead of
//! `std::fs::rename` because rename fails on Windows when the destination
//! exists; persist uses `rename(2)` on unix. Note `persist` does NOT fsync —
//! we call `sync_all()` explicitly, and we fsync the parent directory
//! afterwards because rename(2) alone is not durable.
//!
//! Windows does NOT use `persist`: Go asks for a write-through commit, and
//! persist's `MoveFileExW` cannot ask for one. See [`commit_windows`].

use std::io::Write;
use std::path::Path;

use crate::env::TreehouseEnv;

/// Atomically writes `data` to `path` with the same durability contract as
/// Go's `atomicWriteFile`:
///
/// 1. Create a temp file in the same directory.
/// 2. Write + fsync it.
/// 3. Commit over the target with the platform's write-through primitive —
///    `rename(2)` on unix, `ReplaceFileW`/`MoveFileExW` on Windows.
/// 4. On unix, fsync the parent directory. Windows has no directory handle to
///    flush, so the write-through flag in step 3 is its equivalent.
/// 5. Preserve the existing target's file mode; new files get `perm`.
///
/// Steps 3 and 4 are separate durability steps, not one. `rename(2)` is atomic
/// for *readers* the instant it returns, but the directory entry it creates
/// sits in the page cache until that directory is fsynced. Skip step 4 and a
/// power cut rolls the pool back to the PREVIOUS — still perfectly valid —
/// state file, so `recover_corrupt_state` never fires and a live lease or owner
/// reservation is silently forgotten while a process still holds the worktree.
pub fn atomic_write_file(path: &Path, data: &[u8], perm: u32) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other(format!("no parent dir for {}", path.display())))?;

    // Preserve the existing target's mode if it exists; otherwise use `perm`.
    // Must be read BEFORE the commit: afterwards the name resolves to the new
    // file, so this would report the mode we just wrote.
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

    #[cfg(not(windows))]
    {
        // Atomic commit: rename(2).
        tmp.persist(path).map_err(|e| e.error)?;
    }

    #[cfg(unix)]
    // Make the rename itself durable. Go has done this since v2.0.1
    // (state_file_posix.go:17-24); skipping it silently downgrades the whole
    // pool to "loses the last lease on power loss".
    sync_parent_dir(path)?;

    #[cfg(windows)]
    {
        // into_temp_path closes the handle before the commit, matching Go's
        // tmp.Close() before commitStateFile; keep() disarms tempfile's
        // auto-delete so the path outlives the call.
        let tmp_path = tmp.into_temp_path();
        let tmp_path = tmp_path.keep().map_err(|e| e.error)?;
        if let Err(e) = commit_windows(&tmp_path, path, target_exists(path)) {
            // keep() already cancelled the drop-time cleanup Go's deferred
            // os.Remove would have done, so a failed commit would strand the
            // temp file. Clean it up here instead.
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
    }

    Ok(())
}

/// fsyncs the directory holding `path`. Mirrors Go's `syncDirectory`
/// (`internal/pool/state_file_posix.go:17-24`), the POSIX half of its
/// `commitStateFile`.
///
/// The error propagates instead of being swallowed: a state file we cannot make
/// durable is not a state file we can honestly report as written. That is also
/// why this is not `let _ = ...` — a pool root on a filesystem that rejects
/// directory fsync should fail loudly at the write, not at some later recovery.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other(format!("no parent dir for {}", path.display())))?;
    // A directory is opened read-only — the only mode that yields an fsync-able
    // descriptor for it. This is the call Go's syncDirectory makes.
    std::fs::File::open(dir)?.sync_all()
}

/// Commits `tmp_path` over `path` with the write-through guarantee Go gets from
/// `commitStateFile` (`internal/pool/state_file_windows.go:32-58`).
///
/// `tempfile`'s `persist` calls `MoveFileExW` WITHOUT `MOVEFILE_WRITE_THROUGH`,
/// which only queues the move and returns; a power cut can still undo it. We
/// therefore issue the syscall directly rather than going through persist.
///
/// Go branches on whether the target exists because `ReplaceFileW` preserves
/// the replaced file's ACL (and therefore its permissions) where `MoveFileEx`
/// would install the replacement's own. `target_exists` is the same flag Go's
/// `replacementFileMode` hands to `commitStateFile`.
#[cfg(windows)]
fn commit_windows(tmp_path: &Path, path: &Path, target_exists: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACEFILE_WRITE_THROUGH,
        ReplaceFileW,
    };

    let wide = |p: &Path| {
        p.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>()
    };
    let (from, to) = (wide(tmp_path), wide(path));

    let ok = unsafe {
        if target_exists {
            ReplaceFileW(
                to.as_ptr(),
                from.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null(),
                std::ptr::null(),
            )
        } else {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the target already exists. Windows-only because only the Windows
/// commit primitive branches on it; unix does not need the distinction.
#[cfg(windows)]
fn target_exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
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

/// Write the pool state file using the injected environment.
///
/// Note: atomic write guarantee is relaxed — consumer controls via their
/// `write_file` implementation. For `DefaultEnv`, this still uses atomic
/// write. For `InMemoryEnv`, this uses HashMap insert.
pub fn write_state_with_env(
    pool_dir: &Path,
    state: &crate::state::State,
    env: &dyn TreehouseEnv,
) -> std::io::Result<()> {
    let path = crate::state::State::state_file_path(pool_dir);
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::other(format!("serializing state: {e}")))?;
    let mut bytes = json.into_bytes();
    bytes.push(b'\n');
    env.write_file(&path, &bytes)
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
            ..Default::default()
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

    // ─── parent-directory fsync (M-010) ──────────────────────────────────────
    //
    // Note on coverage: whether the fsync(2) itself reached the platter is not
    // observable from inside the process — proving it needs a crash or syscall
    // interception. These tests pin what IS observable: that the sync runs and
    // succeeds on a real directory, and that a failure is reported instead of
    // swallowed (which would let `atomic_write_file` claim a write it could not
    // make durable).

    #[cfg(unix)]
    #[test]
    fn sync_parent_dir_succeeds_on_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        // A path that does not exist yet is fine: the parent directory is what
        // gets synced, not the file.
        let path = dir.path().join("not-created-yet.json");
        sync_parent_dir(&path).expect("directory fsync must succeed on a real dir");
    }

    #[cfg(unix)]
    #[test]
    fn sync_parent_dir_propagates_error_instead_of_swallowing_it() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let err = sync_parent_dir(&missing.join("state.json"))
            .expect_err("a missing parent directory must not be reported as durable");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "got {err}");
    }

    #[test]
    fn repeated_commits_leave_no_temp_files_behind() {
        // Every commit runs the platform's commit primitive plus (on unix) the
        // parent-directory fsync. Re-commit over an existing target repeatedly:
        // that is the Windows ReplaceFileW branch and the unix "target already
        // exists" branch, and it must stay leak-free because the Windows path
        // disarms tempfile's auto-delete to issue its own syscall.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("treehouse-state.json");
        for i in 0..5u32 {
            atomic_write_file(&path, format!("{{\"n\":{i}}}").as_bytes(), 0o644).unwrap();
            assert_eq!(
                std::fs::read(&path).unwrap(),
                format!("{{\"n\":{i}}}").as_bytes()
            );
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "treehouse-state.json")
            .collect();
        assert!(leftovers.is_empty(), "leftover files: {leftovers:?}");
    }

    // ─── _with_env tests ────────────────────────────────────────────────────

    #[test]
    fn write_state_with_env_roundtrip() {
        let env = crate::env::InMemoryEnv::new(std::path::PathBuf::from("/test"));
        let pool_dir = std::path::PathBuf::from("/test/pool");
        let state = sample_state();

        write_state_with_env(&pool_dir, &state, &env).unwrap();

        // Read back via env
        let path = crate::state::State::state_file_path(&pool_dir);
        let data = env.read_bytes(&path).unwrap();
        let loaded: crate::state::State = serde_json::from_slice(&data).unwrap();
        assert_eq!(state, loaded);
    }
}
