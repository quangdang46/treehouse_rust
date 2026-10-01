//! Filesystem-level fast paths (Go `internal/fileclone`, upstream #153).
//!
//! On APFS a freshly-created git worktree can share file *data* with the source
//! checkout instead of duplicating it: the bytes are stored once and the new
//! directory entry is copy-on-write. For a repo with a large tracked binary the
//! difference is instant versus minutes, and the space is not duplicated either.
//!
//! ## The sharp edge, and why it is safe here
//!
//! Sharing data is NOT sharing an inode. A write through the destination breaks
//! the copy-on-write link and leaves the source untouched, which is what makes
//! the fast path usable for a worktree a developer is about to edit. Go proves
//! that property end to end in `sharing_darwin_test.go` ("target edit changed
//! source" / "source edit/deletion affected clone"); [`share`] keeps the same
//! guarantee and this module's tests re-assert it on every run.
//!
//! The hard part is not the syscall (`fclonefileat`) but everything around it.
//! Publishing a clone replaces a file that already exists, so a mistake can
//! leak metadata (an xattr the source had and the destination must not), corrupt
//! content, or publish bytes that are not the ones we hashed. This module
//! therefore **refuses far more than it accepts**, and every refusal is ported
//! from Go's `attributes_darwin.go` / `metadata_darwin.go`:
//!
//! * no symlink in any path component, including either root — traversal and
//!   publication run on pinned directory descriptors, never on a path that can
//!   be re-pointed underneath us ([`darwin::open_directory`]);
//! * no ACL on the file, the source, or the *parent directory* (a directory ACL
//!   can make a clone inherit differently than a plain copy would);
//! * no `com.apple.decmpfs` or `com.apple.ResourceFork` on either side — their
//!   content hashes can match while the *storage representation* differs;
//! * no sparse or compressed files, same argument at the block level;
//! * no hardlinks, no `st_flags`, no setuid/setgid/sticky bits, no differing
//!   owner, no differing size;
//! * the clone is staged in a private directory and re-hashed **after** its
//!   metadata is restored, so a metadata step that rewrites content is caught;
//! * immediately before `renameat`, the destination file, its parent, and the
//!   destination root are all re-identified through descriptors opened earlier.
//!
//! ## What this cannot do
//!
//! Staging and the final identity checks narrow the window in which a hostile
//! or merely concurrent writer can interfere. They do not close it: no
//! filesystem check can cover the instant between the last check and
//! `renameat`. **The caller must exclusively own the destination directory for
//! the whole call** — see [`share`]. Go demands the same in its package
//! comment, which is why the lifecycle only ever passes a fresh, reserved slot.
//!
//! ## Opt-in
//!
//! Nothing here runs unless a caller asks for it. There is no config read and no
//! environment read *inside this module*: [`share`] is an ordinary function, and
//! the only caller is the pool's acquisition path, which calls it only when the
//! acquisition asked for it (`--apfs-sharing fresh` on the CLI,
//! `$TREEHOUSE_APFS_SHARING`, or `AcquireOptions::apfs_sharing`). Someone who
//! passes no such flag gets byte-identical behavior whether this module is
//! empty or full.
//!
//! Failure is never fatal to the caller. Almost every problem is a *skip*
//! recorded in [`Report::skipped`], and even the aborts in [`ShareError`] are
//! reported so the caller can fall back to a plain copy rather than fail the
//! acquisition. The pool honors that: the tracked files were already written by
//! `git worktree add` before this pass runs, so declining to share leaves a
//! correct worktree that merely duplicates its bytes.
//!
//! ## Where it plugs in
//!
//! Sharing is NOT part of seeding ([`crate::git::GitBackend::seed_worktree`]),
//! which copies the pool's ignored files. Sharing applies to the TRACKED
//! checkout git itself just wrote, and only on a slot the acquisition created
//! moments earlier — the exclusivity this module's contract requires is
//! something a recycled slot cannot promise.
//!
//! The path enumeration and the Git-freshness preflight that upstream keeps in
//! `internal/vcs` are NOT here: this module takes the relative paths and does the
//! sharing. The pool owns that preflight (it needs to check the source
//! repository and the destination worktree, and to notice hooks or an fsmonitor
//! that may have left a writer behind) and then calls [`share`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Smallest file worth sharing, in bytes (Go `fileclone.MinimumSize`).
///
/// Below this the per-file setup — four hashes, a `getattrlist`, a staged
/// directory, a `renameat` — costs more than the duplicated bytes save, and
/// small files are the overwhelming majority of a checkout.
pub const MINIMUM_SIZE: u64 = 64 * 1024;

/// Whether this platform has an APFS sharing implementation.
///
/// `false` everywhere except macOS. On those platforms [`share`] is a no-op
/// that reports a reason rather than an error, and does not even look at the
/// paths it is handed (Go `sharing_other.go`).
pub const SUPPORTED: bool = cfg!(target_os = "macos");

/// Keeps logical payload separate from measured APFS private data. Private
/// data is not volume free space: snapshots and delayed reclamation still
/// apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Files whose data is now shared rather than duplicated.
    pub cloned: u64,
    /// Bytes of payload moved to shared storage. This is the *logical* size of
    /// the clones, not the space reclaimed.
    pub logical_bytes: u64,
    /// Measured reduction in the destination's APFS private data. Reported
    /// separately from [`Self::logical_bytes`] and never a substitute for it.
    pub private_bytes_reduced: u64,
    /// Why each file was left alone, counted by reason (Go `Report.Skipped`).
    pub skipped: BTreeMap<&'static str, u64>,
    /// Set when the whole pass was skipped before looking at any file.
    pub reason: String,
    /// Up to three per-file errors, for the operator. Every error is also
    /// counted as a `kept_after_error` skip.
    pub errors: Vec<String>,
}

impl Report {
    /// Record one refusal. Reasons are static strings so a caller can match on
    /// them (`report.skipped.get("acl")`) instead of parsing prose.
    pub fn skip(&mut self, reason: &'static str) {
        *self.skipped.entry(reason).or_insert(0) += 1;
    }

    /// Whether this pass shared nothing: an unsupported platform, a preflight
    /// refusal, or every file skipped.
    pub fn is_noop(&self) -> bool {
        self.cloned == 0
    }
}

impl fmt::Display for Report {
    /// The one-line summary Go's `Report.String` produces. `BTreeMap` ordering
    /// makes it stable, so it diffs cleanly in logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.reason.is_empty() {
            return write!(f, "APFS sharing skipped: {}", self.reason);
        }
        write!(
            f,
            "APFS sharing: cloned={} logical_bytes={} private_data_reduced_bytes={}",
            self.cloned, self.logical_bytes, self.private_bytes_reduced
        )?;
        for (reason, count) in &self.skipped {
            write!(f, " {reason}={count}")?;
        }
        for error in &self.errors {
            write!(f, " error={error:?}")?;
        }
        Ok(())
    }
}

/// Why a sharing pass gave up instead of finishing.
///
/// Everything *refusable* is a [`Report`] skip, not an error. These variants are
/// the cases where continuing would be unsafe or meaningless, and they are
/// reported so the caller can fall back to a plain copy.
#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    /// The destination file, its parent, or the destination root changed
    /// identity between our first look and the publication step. Something
    /// outside this call is writing to a directory we were told we own.
    #[error("destination changed during APFS sharing: {0}")]
    DestinationChanged(String),

    /// A staging directory could not be fully removed, or removal found data we
    /// did not put there. We refuse to delete anything we did not create, so
    /// the caller is told rather than left guessing what is on disk.
    #[error("APFS sharing staging cleanup incomplete: {0}")]
    Cleanup(String),

    /// The pass was cancelled. Staging has already been torn down.
    #[error("APFS sharing cancelled")]
    Cancelled,

    /// A syscall failed. The affected file is left exactly as it was.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ShareError {
    /// Whether the pass must abort, as opposed to one file being kept as-is and
    /// recorded in [`Report::errors`].
    ///
    /// A cancelled or changed destination means the state we validated is gone;
    /// a cleanup failure means we cannot promise what is left on disk. Both need
    /// the caller to stop and think. An `Io` error is scoped to one file.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            ShareError::DestinationChanged(_) | ShareError::Cleanup(_) | ShareError::Cancelled
        )
    }
}

/// Cooperative cancellation for a sharing pass.
///
/// Go installs a `signal.NotifyContext` inside `Share`. A library must not take
/// over a process's signal disposition, so the token lives here instead: a
/// caller that wants SIGINT/SIGTERM to abort a pass wires its own handler to
/// [`Cancel::cancel`]. [`share`] uses a token that is never cancelled.
///
/// Cancellation is checked between files and between hash chunks, and always
/// leaves staging cleaned up — the same guarantee Go's SIGTERM test asserts.
#[derive(Debug, Default)]
pub struct Cancel {
    flag: AtomicBool,
}

impl Cancel {
    /// A handle that can be cancelled from another thread or a signal handler.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A token that is never cancelled.
    pub fn never() -> Cancel {
        Cancel::default()
    }

    /// Request cancellation. The current file finishes its current step; no
    /// further file is started and no staged clone is published.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

#[cfg(target_os = "macos")]
pub use darwin::{filesystem_reason, share, share_cancellable};

#[cfg(not(target_os = "macos"))]
pub use other::{filesystem_reason, share, share_cancellable};

/// Whether `path` is a relative path that stays inside the destination and is
/// already in its shortest form — the Rust form of Go's
/// `filepath.IsLocal(path) && filepath.Clean(path) == path` on a unix host.
///
/// Both halves reject the same shapes, so they fold into one walk: every
/// `/`-separated component must be non-empty and must not be `.` or `..`.
///
/// * a leading `/` is absolute, so the path could name anything;
/// * an empty component (`a//b`, `a/b/`) means `Clean` would rewrite it;
/// * a `.` component means the same;
/// * a `..` component is what `IsLocal` exists to reject, and even without it
///   `Clean` would rewrite the path, so the two checks cannot disagree.
///
/// A leading `:` is a legal unix filename, so unlike the Windows branch of
/// `IsLocal` it is not special-cased here.
#[cfg(any(target_os = "macos", test))]
fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') {
        return false;
    }
    path.split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
mod path_tests {
    use super::is_safe_relative_path;

    #[test]
    fn plain_relative_paths_are_accepted() {
        for path in [
            "asset",
            "sub/asset",
            "a/b/c.bin",
            "with space",
            "with:colon",
        ] {
            assert!(is_safe_relative_path(path), "{path} should be accepted");
        }
    }

    #[test]
    fn traversal_and_non_normalized_paths_are_refused() {
        for path in [
            "",
            "/asset",
            "../target/asset",
            "sub/../../escape",
            "a//b",
            "a/./b",
            "./a",
            "a/b/",
            ".",
            "..",
        ] {
            assert!(!is_safe_relative_path(path), "{path} should be refused");
        }
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    #[test]
    fn skip_counts_repeat_reasons() {
        let mut report = Report::default();
        report.skip("acl");
        report.skip("acl");
        report.skip("below_threshold");
        assert_eq!(report.skipped["acl"], 2);
        assert_eq!(report.skipped["below_threshold"], 1);
        assert!(report.is_noop());
    }

    #[test]
    fn reason_short_circuits_the_summary() {
        let report = Report {
            reason: "requires APFS on both paths".to_string(),
            ..Report::default()
        };
        assert_eq!(
            report.to_string(),
            "APFS sharing skipped: requires APFS on both paths"
        );
        assert!(report.is_noop());
    }

    /// The rendered form is a log line operators grep; the ordering and the
    /// field names are part of the contract.
    #[test]
    fn summary_is_ordered_and_includes_errors() {
        let mut report = Report {
            cloned: 2,
            logical_bytes: 4096,
            private_bytes_reduced: 1024,
            ..Report::default()
        };
        report.skip("sparse_or_compressed");
        report.skip("acl");
        report.errors.push("asset: boom".to_string());
        assert_eq!(
            report.to_string(),
            "APFS sharing: cloned=2 logical_bytes=4096 private_data_reduced_bytes=1024 \
             acl=1 sparse_or_compressed=1 error=\"asset: boom\""
        );
        assert!(!report.is_noop());
    }

    #[test]
    fn only_fatal_variants_abort_the_pass() {
        assert!(ShareError::DestinationChanged("asset".into()).is_fatal());
        assert!(ShareError::Cleanup("busy".into()).is_fatal());
        assert!(ShareError::Cancelled.is_fatal());
        assert!(!ShareError::Io(std::io::Error::other("boom")).is_fatal());
    }

    #[test]
    fn cancellation_token_round_trips() {
        let token = Cancel::never();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }
}

// ─── macOS ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod darwin {
    use super::{Cancel, MINIMUM_SIZE, Report, ShareError, is_safe_relative_path};
    use libc::{c_int, c_long, mode_t, off_t, time_t};
    use rand::Rng;
    use sha2::{Digest, Sha256};
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    // ── Darwin constants ──────────────────────────────────────────────────
    //
    // From `sys/attr.h` and `sys/attr_clang.h`, which libc does not re-export.
    // `getattrlist` packs returned attributes on four-byte boundaries, not at
    // Rust struct alignment, so responses are read at fixed byte offsets
    // (RESPONSE_*) rather than through a struct. The layout and every value
    // here were checked against a live `fgetattrlist` on APFS; the module's
    // tests re-check them on every run.
    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_CMN_ACCTIME: u32 = 0x0000_1000;
    const ATTR_CMN_EXTENDED_SECURITY: u32 = 0x0040_0000;
    const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
    const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
    /// `ATTR_CMN_FNDRINFO` with `ATTRFMT_INFO` — the allocated size, not the
    /// logical size.
    const ATTR_CMN_FNDRINFO_PRIVATE_SIZE: u32 = 0x0000_0008;
    const ATTR_CMN_CLONEID: u32 = 0x0000_0100;
    const ATTR_CMN_CLONEREFS: u32 = 0x0000_1000;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;
    const FSOPT_PACK_INVAL_ATTRS: u32 = 0x0000_0008;
    const KAUTH_FILESEC_MAGIC: u32 = 0x012c_c16d;
    /// The `kauth_filesec` entry-count value that means "no ACL". Anything
    /// else — including a count of zero for an *explicitly empty* ACL — is an
    /// ACL: inheritance can tell those apart, and cloning must not change that
    /// policy. An absent ACL never reaches this value; it arrives as a
    /// zero-length reference, which is handled separately.
    const KAUTH_FILESEC_NOACL: u32 = u32::MAX;

    // Byte offsets inside an `fgetattrlist` response: a u32 length, the
    // returned attrlist (bitmapcount and reserved are NOT repeated), then
    // packed attribute data.
    const RESPONSE_LENGTH: usize = 0;
    const RESPONSE_COMMONATTR: usize = 4;
    const RESPONSE_FORKATTR: usize = 20;
    /// An `attrreference_t` — { i32 offset, u32 length } — whose offset is
    /// relative to *itself*, not to the start of the buffer.
    const RESPONSE_SECURITY_OFFSET: usize = 24;
    const RESPONSE_SECURITY_LENGTH: usize = 28;
    const RESPONSE_PRIVATE_SIZE: usize = 32;
    const RESPONSE_CLONE_ID: usize = 40;
    /// Smallest legal response for a regular file: header, security reference,
    /// private size, clone id, and the empty clone-reference count.
    const FIXED_FILE_SIZE: usize = 52;
    /// Directories request no forkattr, so only the header plus the reference.
    const FIXED_DIRECTORY_SIZE: usize = 32;
    /// A `kauth_filesec` must be at least this long for its entry count to sit
    /// inside the buffer.
    const MINIMUM_SECURITY_ATTRIBUTE: usize = 44;
    const SECURITY_ENTRY_COUNT: usize = 36;
    /// Large enough for the maximum Darwin ACL, not just the count we read.
    const ATTRIBUTE_BUFFER: usize = 8192;
    /// Bound memory and skip metadata we could not safely reproduce anyway.
    const MAXIMUM_XATTR_BYTES: usize = 1024 * 1024;
    const MAXIMUM_REPORTED_ERRORS: usize = 3;
    const HASH_CHUNK: usize = 128 * 1024;
    const STAGING_PREFIX: &str = ".treehouse-sharing-";
    const DIRECTORY_FLAGS: c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    /// A cheap preflight so unsupported volumes do not pay for Git enumeration
    /// and status. [`share`] repeats these checks through no-follow handles; a
    /// pass here is a hint, not a promise.
    pub fn filesystem_reason(source: &Path, destination: &Path) -> String {
        let (Ok(source_fs), Ok(destination_fs)) = (statfs_path(source), statfs_path(destination))
        else {
            return "filesystem cannot be verified".to_string();
        };
        if !is_apfs(&source_fs) || !is_apfs(&destination_fs) {
            return "requires APFS on both paths".to_string();
        }
        // Same volume means same `st_dev` on macOS. libc keeps `fsid_t`'s two
        // halves private, and the authoritative check inside `share` compares
        // `st_dev` on the opened handles — so the preflight and the real check
        // share one predicate instead of two that could drift.
        match (metadata_path(source), metadata_path(destination)) {
            (Ok(source_meta), Ok(destination_meta))
                if source_meta.st_dev == destination_meta.st_dev =>
            {
                String::new()
            }
            _ => "source and destination are on different volumes".to_string(),
        }
    }

    /// Share identical file data between `source` and `destination` without
    /// sharing writable inodes.
    ///
    /// `paths` are relative, `Clean`-form paths interpreted under
    /// `destination`; anything else is counted as an `unsafe_path` skip. The
    /// source file is `source` joined with the same relative path.
    ///
    /// # Safety contract with the caller
    ///
    /// The caller must **exclusively own `destination` for the duration of this
    /// call**: no hooks, no smudge filters, no fsmonitor, no other process.
    /// Neither the identity checks nor the final atomic `renameat` can protect
    /// against a concurrent writer — they only narrow the window. Go states the
    /// same requirement in the package comment, and the lifecycle satisfies it
    /// by only ever passing a fresh, reserved slot.
    ///
    /// Failure is not fatal. A [`ShareError`] that
    /// [`ShareError::is_fatal`] rejects is one file left untouched and recorded
    /// in [`Report::errors`]; a fatal one means the caller must fall back to a
    /// plain copy rather than continue.
    ///
    /// A fatal error carries no [`Report`]. Go returns the partial one
    /// alongside, but after a changed destination, a failed cleanup, or a
    /// cancellation the destination is in a state the caller must not reason
    /// about — a partial count would read as a partial success.
    pub fn share(
        source: &Path,
        destination: &Path,
        paths: &[impl AsRef<str>],
    ) -> Result<Report, ShareError> {
        share_cancellable(source, destination, paths, &Cancel::never())
    }

    /// [`share`], aborting promptly when `cancel` is tripped. See [`Cancel`] for
    /// why the signal handling lives with the caller.
    pub fn share_cancellable(
        source: &Path,
        destination: &Path,
        paths: &[impl AsRef<str>],
        cancel: &Cancel,
    ) -> Result<Report, ShareError> {
        share_with(cancel, source, destination, paths, &production_ops())
    }

    // ── the pass ──────────────────────────────────────────────────────────

    /// The two operations the port keeps injectable. Behavioral failure tests
    /// override these rather than reaching into production state, so they can
    /// never change behavior for another pass or for a real caller.
    type CloneFn<'a> = &'a dyn Fn(RawFd, RawFd, &CStr) -> io::Result<()>;
    type MetadataFn<'a> = &'a dyn Fn(RawFd, &Metadata<'_>) -> io::Result<()>;

    struct Ops<'a> {
        clone: CloneFn<'a>,
        metadata: MetadataFn<'a>,
    }

    /// `fn` pointers in statics so the production operations can be handed out
    /// as `&'static dyn Fn` — the same shape a behavioral test overrides with a
    /// capturing closure.
    static PRODUCTION_CLONE: fn(RawFd, RawFd, &CStr) -> io::Result<()> = clone_file;
    static PRODUCTION_METADATA: fn(RawFd, &Metadata<'_>) -> io::Result<()> = restore_metadata;

    fn production_ops() -> Ops<'static> {
        Ops {
            clone: &PRODUCTION_CLONE,
            metadata: &PRODUCTION_METADATA,
        }
    }

    fn share_with(
        cancel: &Cancel,
        source: &Path,
        destination: &Path,
        paths: &[impl AsRef<str>],
        ops: &Ops<'_>,
    ) -> Result<Report, ShareError> {
        let mut report = Report::default();
        let source_dir = match open_directory(source) {
            Ok(dir) => dir,
            Err(error) => {
                report.reason = format!("source path unavailable or symlinked: {error}");
                return Ok(report);
            }
        };
        let destination_dir = match open_directory(destination) {
            Ok(dir) => dir,
            Err(error) => {
                report.reason = format!("destination path unavailable or symlinked: {error}");
                return Ok(report);
            }
        };

        // Everything below re-verifies the preflight through the pinned
        // handles: a path can be swapped between the check and the open.
        let source_fs = match fstatfs(source_dir.as_raw_fd()) {
            Ok(fs) => fs,
            Err(_) => return Ok(refuse(report, "source filesystem cannot be verified")),
        };
        let destination_fs = match fstatfs(destination_dir.as_raw_fd()) {
            Ok(fs) => fs,
            Err(_) => return Ok(refuse(report, "destination filesystem cannot be verified")),
        };
        if !is_apfs(&source_fs) || !is_apfs(&destination_fs) {
            return Ok(refuse(report, "requires APFS on both paths"));
        }
        let source_stat = match fstat(source_dir.as_raw_fd()) {
            Ok(stat) => stat,
            Err(_) => return Ok(refuse(report, "source identity cannot be verified")),
        };
        let destination_stat = match fstat(destination_dir.as_raw_fd()) {
            Ok(stat) => stat,
            Err(_) => return Ok(refuse(report, "destination identity cannot be verified")),
        };
        if source_stat.st_dev != destination_stat.st_dev {
            return Ok(refuse(
                report,
                "source and destination are on different volumes",
            ));
        }
        if source_stat.st_ino == destination_stat.st_ino {
            return Ok(refuse(
                report,
                "source and destination are the same directory",
            ));
        }

        for path in paths {
            let path = AsRef::<str>::as_ref(path);
            if cancel.is_cancelled() {
                return Err(ShareError::Cancelled);
            }
            if !is_safe_relative_path(path) {
                report.skip("unsafe_path");
                continue;
            }
            // The staging guard records teardown failures here; Rust `Drop`
            // cannot return one, and a cleanup failure must abort the pass.
            let cleanup = Cell::new(None);
            let outcome = share_file(
                cancel,
                &source_dir,
                &destination_dir,
                destination,
                path,
                &destination_stat,
                ops,
                &cleanup,
            );
            if let Some(message) = cleanup.take() {
                return Err(ShareError::Cleanup(message));
            }
            match outcome {
                Err(error) if error.is_fatal() => return Err(error),
                Err(error) => {
                    report.skip("kept_after_error");
                    if report.errors.len() < MAXIMUM_REPORTED_ERRORS {
                        report.errors.push(format!("{path}: {error}"));
                    }
                }
                Ok(Outcome {
                    reason: Some(reason),
                    ..
                }) => report.skip(reason),
                Ok(Outcome {
                    reason: None,
                    logical,
                    saved,
                }) => {
                    report.cloned += 1;
                    report.logical_bytes += logical;
                    report.private_bytes_reduced += saved;
                }
            }
        }
        Ok(report)
    }

    fn refuse(mut report: Report, reason: &str) -> Report {
        report.reason = reason.to_string();
        report
    }

    struct Outcome {
        reason: Option<&'static str>,
        logical: u64,
        saved: u64,
    }

    impl Outcome {
        fn skip(reason: &'static str) -> Outcome {
            Outcome {
                reason: Some(reason),
                logical: 0,
                saved: 0,
            }
        }

        fn cloned(logical: u64, saved: u64) -> Outcome {
            Outcome {
                reason: None,
                logical,
                saved,
            }
        }
    }

    /// The metadata a published clone must carry: the *destination's* original
    /// stat, ACL verdict, and xattrs — never the source's.
    struct Metadata<'a> {
        stat: &'a libc::stat,
        attributes: Attributes,
        xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    }

    #[allow(clippy::too_many_arguments)]
    fn share_file(
        cancel: &Cancel,
        source: &OwnedFd,
        destination: &OwnedFd,
        destination_path: &Path,
        path: &str,
        root_stat: &libc::stat,
        ops: &Ops<'_>,
        cleanup: &Cell<Option<String>>,
    ) -> Result<Outcome, ShareError> {
        let (parent_path, leaf) = split_leaf(path);
        let leaf = cstr(leaf.as_bytes())?;

        // A parent we cannot open no-follow is a *skip*, not a failure: the
        // destination is simply not shaped like the source here.
        let Ok(parent) = open_relative_directory(destination.as_raw_fd(), parent_path) else {
            return Ok(Outcome::skip("destination_missing_or_symlink"));
        };
        let Ok((destination_file, old)) = open_regular(parent.as_raw_fd(), &leaf) else {
            return Ok(Outcome::skip("destination_not_regular_or_symlink"));
        };
        if old.st_size < MINIMUM_SIZE as off_t {
            return Ok(Outcome::skip("below_threshold"));
        }
        let Ok(source_parent) = open_relative_directory(source.as_raw_fd(), parent_path) else {
            return Ok(Outcome::skip("source_missing_or_symlink"));
        };
        let Ok((source_file, donor)) = open_regular(source_parent.as_raw_fd(), &leaf) else {
            return Ok(Outcome::skip("source_not_regular_or_symlink"));
        };

        if old.st_dev != donor.st_dev {
            return Ok(Outcome::skip("different_volume"));
        }
        if old.st_ino == donor.st_ino {
            return Ok(Outcome::skip("same_file"));
        }
        // A second link means someone else's name points at these blocks;
        // replacing the destination would change what they see.
        if old.st_nlink != 1 || donor.st_nlink != 1 {
            return Ok(Outcome::skip("hardlink"));
        }
        if old.st_uid != donor.st_uid || old.st_gid != donor.st_gid {
            return Ok(Outcome::skip("different_owner"));
        }
        if old.st_flags != 0
            || donor.st_flags != 0
            || old.st_mode & 0o7000 != 0
            || donor.st_mode & 0o7000 != 0
        {
            return Ok(Outcome::skip("unsupported_flags_or_mode"));
        }
        if old.st_size != donor.st_size {
            return Ok(Outcome::skip("different_content"));
        }
        // Conservatively exclude sparse and compressed representations. Their
        // hashes may match while the metadata changes how the bytes are
        // interpreted — and a clone would silently drop the sparseness.
        if allocated_bytes(&old) < old.st_size as u64
            || allocated_bytes(&donor) < donor.st_size as u64
        {
            return Ok(Outcome::skip("sparse_or_compressed"));
        }

        // Settle this newly checked-out file's allocation before measuring
        // private bytes; logical payload is reported separately and is never a
        // substitute.
        fsync(destination_file.as_raw_fd())
            .map_err(|error| context(error, "measuring allocation"))?;
        let destination_attributes = file_attributes(destination_file.as_raw_fd())
            .map_err(|error| context(error, "destination attributes"))?;
        let source_attributes = file_attributes(source_file.as_raw_fd())
            .map_err(|error| context(error, "source attributes"))?;
        let parent_attributes = file_attributes(parent.as_raw_fd())
            .map_err(|error| context(error, "parent attributes"))?;
        if destination_attributes.acl || source_attributes.acl || parent_attributes.acl {
            // A directory ACL can make the clone inherit differently than a
            // plain copy would, and we cannot reproduce the inheritance.
            return Ok(Outcome::skip("acl"));
        }
        let xattrs = read_xattrs(destination_file.as_raw_fd())
            .map_err(|error| context(error, "destination metadata"))?;
        let donor_xattrs = read_xattrs(source_file.as_raw_fd())
            .map_err(|error| context(error, "source metadata"))?;
        for set in [&xattrs, &donor_xattrs] {
            if set.contains_key(b"com.apple.decmpfs".as_slice())
                || set.contains_key(b"com.apple.ResourceFork".as_slice())
            {
                return Ok(Outcome::skip("representation_xattr"));
            }
        }
        let expected = hash(destination_file.as_raw_fd(), cancel)?;
        if hash(source_file.as_raw_fd(), cancel)? != expected {
            return Ok(Outcome::skip("different_content"));
        }
        if source_attributes.clone_id != 0
            && source_attributes.clone_id == destination_attributes.clone_id
        {
            // Already the same physical blocks. Cloning again would only churn
            // the directory entry, and would lose whatever made the first
            // clone worth keeping.
            return Ok(Outcome::skip("already_shared"));
        }

        // Sixteen unpredictable bytes: the staging name must not be guessable,
        // or a hostile process could pre-create the directory and have us
        // publish into its tree.
        let mut random = [0u8; 16];
        rand::rng().fill(&mut random);
        let name = CString::new(format!("{STAGING_PREFIX}{}", hex(&random)))
            .expect("a hex rendering cannot contain NUL");
        if let Err(error) = mkdirat(parent.as_raw_fd(), &name, 0o700) {
            return Err(error.into());
        }
        let staging_dir = match openat_owned(parent.as_raw_fd(), &name, DIRECTORY_FLAGS) {
            Ok(dir) => dir,
            Err(error) => {
                return Err(
                    if let Err(cleanup_error) = unlinkat_removedir(parent.as_raw_fd(), &name) {
                        ShareError::Cleanup(format!("{error}; staging directory: {cleanup_error}"))
                    } else {
                        error.into()
                    },
                );
            }
        };
        let staging = Staging {
            dir: staging_dir,
            parent: parent.as_raw_fd(),
            name,
            failure: cleanup,
        };

        if cancel.is_cancelled() {
            return Err(ShareError::Cancelled);
        }
        (ops.clone)(source_file.as_raw_fd(), staging.dir.as_raw_fd(), c"clone")
            .map_err(|error| context(error, "clonefile"))?;
        let (staged, _) = open_regular(staging.dir.as_raw_fd(), c"clone")?;
        if hash(staged.as_raw_fd(), cancel)? != expected {
            // The source changed between our hash and the clone. The staged
            // copy is discarded with the staging directory.
            return Ok(Outcome::skip("source_changed"));
        }
        let wanted = Metadata {
            stat: &old,
            attributes: destination_attributes,
            xattrs,
        };
        (ops.metadata)(staged.as_raw_fd(), &wanted)
            .map_err(|error| context(error, "preserving metadata"))?;
        if hash(staged.as_raw_fd(), cancel)? != expected {
            // Restoring metadata rewrote content. Publishing would be
            // publishing something we never verified.
            return Ok(Outcome::skip("metadata_changed_content"));
        }
        // Hashing can update atime; restore both timestamps after the last read.
        restore_times(staged.as_raw_fd(), &old)?;
        verify_metadata(staged.as_raw_fd(), &wanted)?;
        let final_attributes = file_attributes(staged.as_raw_fd())?;
        fsync(staged.as_raw_fd())?;
        if cancel.is_cancelled() {
            return Err(ShareError::Cancelled);
        }

        // Re-check the open file, its name, the destination root and the
        // parent. This still requires exclusive ownership: no filesystem check
        // can close the interval between here and the rename.
        let now = fstat(destination_file.as_raw_fd())?;
        let named = fstatat(parent.as_raw_fd(), &leaf, libc::AT_SYMLINK_NOFOLLOW)?;
        if !same_identity(&old, &now)
            || !same_identity(&old, &named)
            || !directory_unchanged(destination_path, root_stat)
            || !parent_unchanged(destination.as_raw_fd(), parent_path, &parent)
        {
            return Err(ShareError::DestinationChanged(path.to_string()));
        }
        renameat(staging.dir.as_raw_fd(), c"clone", parent.as_raw_fd(), &leaf)?;

        let saved = destination_attributes
            .private_bytes
            .saturating_sub(final_attributes.private_bytes);
        Ok(Outcome::cloned(old.st_size as u64, saved))
    }

    /// Removes the staged clone and the staging directory when a file is
    /// finished with — published, refused, or errored.
    ///
    /// Go joins a cleanup failure onto the in-flight error. `Drop` cannot return
    /// one, so a failure is stashed for the caller to promote: the distinction
    /// matters, because an incomplete teardown must abort the pass and must
    /// never be resolved by deleting data we did not create.
    struct Staging<'a> {
        dir: OwnedFd,
        parent: RawFd,
        name: CString,
        failure: &'a Cell<Option<String>>,
    }

    impl Staging<'_> {
        /// Keeps the first failure: later ones are consequences of it, and the
        /// first is the one that names the actual cause.
        fn record(&self, message: String) {
            if self.failure.take().is_none() {
                self.failure.set(Some(message));
            }
        }
    }

    impl Drop for Staging<'_> {
        fn drop(&mut self) {
            // Only the clone WE named. Anything else in the directory is not
            // ours to remove, and the rmdir below will report that. An already
            // absent clone means the rename published it, which is success.
            if let Err(error) = unlinkat_nodir(self.dir.as_raw_fd(), c"clone")
                && error.raw_os_error() != Some(libc::ENOENT)
            {
                self.record(format!("unlinking staged clone: {error}"));
            }
            // Re-identify the directory through BOTH the descriptor we hold and
            // its name in the parent, so a staging directory that was moved or
            // replaced is never removed by name alone.
            let identity = match (
                fstat(self.dir.as_raw_fd()),
                fstatat(self.parent, &self.name, libc::AT_SYMLINK_NOFOLLOW),
            ) {
                (Ok(opened), Ok(named)) => {
                    (opened.st_dev, opened.st_ino) == (named.st_dev, named.st_ino)
                }
                _ => false,
            };
            if !identity {
                // The name no longer resolves to the directory we created.
                // Something replaced or moved it; refuse to remove anything.
                self.record(format!(
                    "staging directory {} was replaced before removal",
                    self.name.to_string_lossy()
                ));
                return;
            }
            if let Err(error) = unlinkat_removedir(self.parent, &self.name) {
                self.record(format!("removing staging directory: {error}"));
            }
        }
    }

    // ── attributes ────────────────────────────────────────────────────────

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Attributes {
        /// APFS allocated bytes. Zero when the file's data is entirely shared
        /// with another clone.
        private_bytes: u64,
        /// The data-stream identity. Two files with the same non-zero clone id
        /// read the same physical blocks.
        clone_id: u64,
        /// Whether the file carries an explicit ACL. An *empty* explicit ACL
        /// counts: inheritance can distinguish it from "no ACL", and cloning
        /// must not change that policy.
        acl: bool,
    }

    fn file_attributes(fd: RawFd) -> io::Result<Attributes> {
        let mut request = attrlist(ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_EXTENDED_SECURITY, 0);
        let stat = fstat(fd)?;
        let mut fixed_size = FIXED_FILE_SIZE;
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            // Parents need the ACL check but have no data streams to report.
            fixed_size = FIXED_DIRECTORY_SIZE;
        } else {
            request.forkattr =
                ATTR_CMN_FNDRINFO_PRIVATE_SIZE | ATTR_CMN_CLONEID | ATTR_CMN_CLONEREFS;
        }
        let mut buffer = [0u8; ATTRIBUTE_BUFFER];
        // SAFETY: `request` and `buffer` are live locals; fgetattrlist writes at
        // most `buffer.len()` bytes and does not retain either pointer.
        let code = unsafe {
            libc::fgetattrlist(
                fd,
                (&raw mut request).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                FSOPT_ATTR_CMN_EXTENDED | FSOPT_PACK_INVAL_ATTRS,
            )
        };
        if code != 0 {
            return Err(io::Error::last_os_error());
        }
        let length = read_u32(&buffer, RESPONSE_LENGTH)? as usize;
        let common = read_u32(&buffer, RESPONSE_COMMONATTR)?;
        let fork = read_u32(&buffer, RESPONSE_FORKATTR)?;
        if length < fixed_size
            || length > buffer.len()
            || common & ATTR_CMN_RETURNED_ATTRS == 0
            || fork & request.forkattr != request.forkattr
        {
            return Err(invalid_data(format!(
                "APFS allocation/security attributes unavailable: \
                 length={length} common={common:#x} extended={fork:#x}"
            )));
        }
        let reference = read_i32(&buffer, RESPONSE_SECURITY_OFFSET)?;
        let security_size = read_u32(&buffer, RESPONSE_SECURITY_LENGTH)?;
        let acl = if security_size == 0 {
            false
        } else {
            if common & ATTR_CMN_EXTENDED_SECURITY == 0 {
                return Err(invalid_data("security attribute was not returned"));
            }
            let start = RESPONSE_SECURITY_OFFSET as i64 + i64::from(reference);
            if start < fixed_size as i64
                || security_size < MINIMUM_SECURITY_ATTRIBUTE as u32
                || start + i64::from(security_size) > length as i64
            {
                return Err(invalid_data("invalid Darwin security attribute"));
            }
            let start = start as usize;
            if read_u32(&buffer, start)? != KAUTH_FILESEC_MAGIC {
                return Err(invalid_data("unknown Darwin security attribute"));
            }
            read_u32(&buffer, start + SECURITY_ENTRY_COUNT)? != KAUTH_FILESEC_NOACL
        };
        Ok(Attributes {
            private_bytes: if request.forkattr == 0 {
                0
            } else {
                read_u64(&buffer, RESPONSE_PRIVATE_SIZE)?
            },
            clone_id: if request.forkattr == 0 {
                0
            } else {
                read_u64(&buffer, RESPONSE_CLONE_ID)?
            },
            acl,
        })
    }

    /// `fsetattrlist` preserves nanoseconds; `futimes` would truncate them to
    /// microseconds and silently change a file a caller is about to trust.
    fn restore_times(fd: RawFd, wanted: &libc::stat) -> io::Result<()> {
        let mut request = attrlist(ATTR_CMN_MODTIME | ATTR_CMN_ACCTIME, 0);
        // The attribute buffer is `struct timespec[2]` in
        // {modification, access} order.
        let times = [
            libc::timespec {
                tv_sec: wanted.st_mtime,
                tv_nsec: wanted.st_mtime_nsec,
            },
            libc::timespec {
                tv_sec: wanted.st_atime,
                tv_nsec: wanted.st_atime_nsec,
            },
        ];
        let size = size_of::<libc::timespec>() * times.len();
        // SAFETY: both structs are live locals and `size` is their combined
        // length; fsetattrlist copies the data and retains neither pointer.
        let code = unsafe {
            libc::fsetattrlist(
                fd,
                (&raw mut request).cast(),
                times.as_ptr().cast_mut().cast(),
                size,
                0,
            )
        };
        if code != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn attrlist(commonattr: u32, forkattr: u32) -> libc::attrlist {
        libc::attrlist {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr,
        }
    }

    // ── xattrs ────────────────────────────────────────────────────────────

    /// Reads the destination's own xattrs so they can be reproduced exactly.
    /// No source-only xattr — quarantine, provenance, anything — may leak onto
    /// the destination, which means the whole set has to be readable, not just
    /// the names we care about.
    fn read_xattrs(fd: RawFd) -> io::Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let mut result = BTreeMap::new();
        let size = list_xattr_size(fd)?;
        if size > MAXIMUM_XATTR_BYTES {
            return Err(invalid_data("extended attribute names exceed safety limit"));
        }
        if size == 0 {
            return Ok(result);
        }
        let mut names = vec![0u8; size];
        let written = list_xattrs(fd, &mut names)?;
        if written > names.len() {
            return Err(invalid_data("extended attribute list changed"));
        }
        let mut total = 0usize;
        for name in names[..written].split(|byte| *byte == 0) {
            if name.is_empty() {
                continue;
            }
            let size = xattr_size(fd, name)?;
            total += size;
            if total > MAXIMUM_XATTR_BYTES {
                return Err(invalid_data("extended attributes exceed safety limit"));
            }
            let mut value = vec![0u8; size];
            let read = get_xattr(fd, name, &mut value)?;
            if read > value.len() {
                return Err(invalid_data("extended attribute changed"));
            }
            value.truncate(read);
            result.insert(name.to_vec(), value);
        }
        Ok(result)
    }

    fn restore_metadata(fd: RawFd, wanted: &Metadata<'_>) -> io::Result<()> {
        let current = read_xattrs(fd)?;
        for name in current.keys() {
            if !wanted.xattrs.contains_key(name) {
                remove_xattr(fd, name)?;
            }
        }
        for (name, value) in &wanted.xattrs {
            if current.get(name) == Some(value) {
                continue;
            }
            set_xattr(fd, name, value)?;
        }
        // Mode and times last: a failure here must leave a clone that does not
        // look publishable, not one that does.
        fchmod(fd, wanted.stat.st_mode)?;
        restore_times(fd, wanted.stat)
    }

    fn verify_metadata(fd: RawFd, wanted: &Metadata<'_>) -> io::Result<()> {
        let stat = fstat(fd)?;
        if stat.st_mode != wanted.stat.st_mode
            || stat.st_uid != wanted.stat.st_uid
            || stat.st_gid != wanted.stat.st_gid
            || stat.st_size != wanted.stat.st_size
            || !same_time(
                stat.st_mtime,
                stat.st_mtime_nsec,
                wanted.stat.st_mtime,
                wanted.stat.st_mtime_nsec,
            )
            || !same_time(
                stat.st_atime,
                stat.st_atime_nsec,
                wanted.stat.st_atime,
                wanted.stat.st_atime_nsec,
            )
            || stat.st_flags != wanted.stat.st_flags
            || stat.st_nlink != 1
        {
            return Err(invalid_data(
                "cloning did not preserve destination metadata",
            ));
        }
        if file_attributes(fd)?.acl != wanted.attributes.acl {
            return Err(invalid_data("cloning changed ACL"));
        }
        if read_xattrs(fd)? != wanted.xattrs {
            return Err(invalid_data("cloning changed extended attributes"));
        }
        Ok(())
    }

    // ── paths and identities ──────────────────────────────────────────────

    /// The parent directory and the leaf of a relative path, both relative to a
    /// destination root. An empty parent means the root itself, which is what
    /// Go's `filepath.Dir` expresses as `"."`.
    fn split_leaf(path: &str) -> (&Path, &str) {
        match path.rsplit_once('/') {
            Some((parent, leaf)) => (Path::new(parent), leaf),
            None => (Path::new(""), path),
        }
    }

    /// Rejects a symlink in every component, including either root. Once
    /// returned, traversal and publication use the pinned descriptor, so a
    /// rename of an ancestor afterwards cannot redirect us.
    fn open_directory(path: &Path) -> io::Result<OwnedFd> {
        let absolute = std::path::absolute(path)?;
        let root = openat_owned(libc::AT_FDCWD, c"/", DIRECTORY_FLAGS)?;
        open_relative_directory(root.as_raw_fd(), &absolute)
    }

    /// `O_NOFOLLOW` on every step is what turns a symlinked component into an
    /// error instead of a redirection. `..` is opened like any other component,
    /// because a caller-supplied root may legitimately name one.
    fn open_relative_directory(root: RawFd, path: &Path) -> io::Result<OwnedFd> {
        let mut fd = openat_owned(root, c".", DIRECTORY_FLAGS)?;
        for component in path.components() {
            // The root is already pinned by the descriptor we were handed, and
            // "." is this directory by definition. Every other component —
            // including ".." — is opened, so the walk ends on exactly the
            // directory the caller named.
            let part = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::ParentDir => Path::new("..").as_os_str(),
                Component::Normal(part) => part,
                Component::Prefix(_) => return Err(invalid_data("unsupported path prefix")),
            };
            let part = cstr(part.as_bytes())?;
            let next = openat_owned(fd.as_raw_fd(), &part, DIRECTORY_FLAGS);
            // Replaced by `next` either way, so close the old descriptor.
            drop(fd);
            fd = next?;
        }
        Ok(fd)
    }

    /// Opens a regular file, refusing symlinks. `O_NONBLOCK` keeps a FIFO from
    /// blocking the pass forever; the `S_IFREG` check rejects it anyway.
    fn open_regular(parent: RawFd, name: &CStr) -> io::Result<(OwnedFd, libc::stat)> {
        let file = openat_owned(
            parent,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )?;
        let stat = fstat(file.as_raw_fd())?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(invalid_data("not a readable regular file"));
        }
        Ok((file, stat))
    }

    fn same_identity(a: &libc::stat, b: &libc::stat) -> bool {
        a.st_dev == b.st_dev
            && a.st_ino == b.st_ino
            && a.st_size == b.st_size
            && same_time(a.st_mtime, a.st_mtime_nsec, b.st_mtime, b.st_mtime_nsec)
            && same_time(a.st_ctime, a.st_ctime_nsec, b.st_ctime, b.st_ctime_nsec)
            && a.st_nlink == b.st_nlink
    }

    fn same_time(a_sec: time_t, a_nsec: c_long, b_sec: time_t, b_nsec: c_long) -> bool {
        a_sec == b_sec && a_nsec == b_nsec
    }

    /// Re-resolves the destination root through its original path. A root that
    /// was renamed away and replaced is no longer the directory we were given.
    fn directory_unchanged(path: &Path, before: &libc::stat) -> bool {
        open_directory(path)
            .and_then(|dir| fstat(dir.as_raw_fd()))
            .is_ok_and(|now| now.st_dev == before.st_dev && now.st_ino == before.st_ino)
    }

    fn parent_unchanged(root: RawFd, path: &Path, parent: &OwnedFd) -> bool {
        let Ok(fresh) = open_relative_directory(root, path) else {
            return false;
        };
        match (fstat(fresh.as_raw_fd()), fstat(parent.as_raw_fd())) {
            (Ok(a), Ok(b)) => a.st_dev == b.st_dev && a.st_ino == b.st_ino,
            _ => false,
        }
    }

    fn allocated_bytes(stat: &libc::stat) -> u64 {
        // A negative block count is not a thing; treating it as zero makes the
        // sparseness check refuse rather than proceed.
        u64::try_from(stat.st_blocks)
            .unwrap_or(0)
            .saturating_mul(512)
    }

    // ── content ───────────────────────────────────────────────────────────

    /// `pread` rather than seek-then-read: hashing must not disturb the
    /// descriptor's offset, and the file is only ever read through this handle.
    /// It does update atime, which is why [`restore_times`] runs after the last
    /// hash.
    fn hash(fd: RawFd, cancel: &Cancel) -> Result<[u8; 32], ShareError> {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; HASH_CHUNK];
        let mut offset: off_t = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(ShareError::Cancelled);
            }
            let read = pread(fd, &mut buffer, offset)?;
            if read == 0 {
                return Ok(hasher.finalize().into());
            }
            hasher.update(&buffer[..read]);
            offset += read as off_t;
        }
    }

    fn clone_file(source: RawFd, destination_dir: RawFd, name: &CStr) -> io::Result<()> {
        // SAFETY: `name` is a valid C string and both descriptors are live for
        // the call. fclonefileat takes ownership of neither.
        let code = unsafe { libc::fclonefileat(source, destination_dir, name.as_ptr(), 0) };
        if code != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    // ── syscall and buffer helpers ────────────────────────────────────────

    /// Attach Go's `%w`-style context to a syscall error without dropping the
    /// errno from the message.
    fn context(error: io::Error, message: &str) -> ShareError {
        ShareError::Io(io::Error::new(error.kind(), format!("{message}: {error}")))
    }

    fn invalid_data(message: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message.into())
    }

    fn cstr(value: &[u8]) -> io::Result<CString> {
        CString::new(value).map_err(|_| invalid_data("path contains a NUL byte"))
    }

    /// Lowercase hex. `LowerHex` is not implemented for arrays, and the
    /// staging name has to be exactly as unguessable as the bytes in it.
    fn hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            out.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble is a hex digit"));
            out.push(char::from_digit(u32::from(byte & 0xf), 16).expect("nibble is a hex digit"));
        }
        out
    }

    fn openat_owned(dirfd: RawFd, path: &CStr, flags: c_int) -> io::Result<OwnedFd> {
        // SAFETY: `path` is a valid C string for the duration of the call, and a
        // non-negative return is a fresh descriptor we now own.
        let fd = unsafe { libc::openat(dirfd, path.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is fresh and owned; nothing else will close it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn fstat(fd: RawFd) -> io::Result<libc::stat> {
        // SAFETY: `fd` is live and `stat` is a plain C struct we own.
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(fd, &raw mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    fn fstatat(dirfd: RawFd, path: &CStr, flags: c_int) -> io::Result<libc::stat> {
        // SAFETY: as `fstat`, plus a valid `path`.
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstatat(dirfd, path.as_ptr(), &raw mut stat, flags) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    fn fstatfs(fd: RawFd) -> io::Result<libc::statfs> {
        // SAFETY: as `fstat`, plus a correctly sized `statfs` buffer.
        let mut statfs = unsafe { std::mem::zeroed::<libc::statfs>() };
        if unsafe { libc::fstatfs(fd, &raw mut statfs) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(statfs)
    }

    fn statfs_path(path: &Path) -> io::Result<libc::statfs> {
        let name = cstr(path.as_os_str().as_bytes())?;
        // SAFETY: `name` is a valid C string and `statfs` is a plain C struct
        // we own.
        let mut statfs = unsafe { std::mem::zeroed::<libc::statfs>() };
        if unsafe { libc::statfs(name.as_ptr(), &raw mut statfs) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(statfs)
    }

    fn metadata_path(path: &Path) -> io::Result<libc::stat> {
        let name = cstr(path.as_os_str().as_bytes())?;
        // SAFETY: as `statfs_path`, for `stat`. Following a symlink is fine in
        // a preflight; the authoritative check runs on no-follow handles.
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::stat(name.as_ptr(), &raw mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    /// The filesystem name from a `statfs`, NUL-padded in a fixed 16-byte field.
    /// Only APFS supports the clone data streams this module reads, and only
    /// APFS gives a meaningful answer to the allocation and clone-id queries.
    fn is_apfs(statfs: &libc::statfs) -> bool {
        let name: Vec<u8> = statfs
            .f_fstypename
            .iter()
            .map(|byte| *byte as u8)
            .take_while(|byte| *byte != 0)
            .collect();
        name == b"apfs"
    }

    fn fsync(fd: RawFd) -> io::Result<()> {
        // SAFETY: `fd` is live for the call; fsync only takes a descriptor.
        if unsafe { libc::fsync(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn fchmod(fd: RawFd, mode: mode_t) -> io::Result<()> {
        // SAFETY: `fd` is live for the call.
        if unsafe { libc::fchmod(fd, mode) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn mkdirat(dirfd: RawFd, name: &CStr, mode: mode_t) -> io::Result<()> {
        // SAFETY: `dirfd` and `name` are valid for the call.
        if unsafe { libc::mkdirat(dirfd, name.as_ptr(), mode) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn unlinkat_nodir(dirfd: RawFd, name: &CStr) -> io::Result<()> {
        // SAFETY: `dirfd` and `name` are valid for the call.
        if unsafe { libc::unlinkat(dirfd, name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn unlinkat_removedir(dirfd: RawFd, name: &CStr) -> io::Result<()> {
        // SAFETY: `dirfd` and `name` are valid for the call.
        if unsafe { libc::unlinkat(dirfd, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn renameat(old_dirfd: RawFd, old: &CStr, new_dirfd: RawFd, new: &CStr) -> io::Result<()> {
        // SAFETY: both descriptors and both names are valid for the call.
        if unsafe { libc::renameat(old_dirfd, old.as_ptr(), new_dirfd, new.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn pread(fd: RawFd, buffer: &mut [u8], offset: off_t) -> io::Result<usize> {
        // SAFETY: `buffer` is writable for `len` bytes and `fd` is live; pread
        // writes at most `len` bytes and does not move the file offset.
        let read = unsafe { libc::pread(fd, buffer.as_mut_ptr().cast(), buffer.len(), offset) };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }

    fn list_xattr_size(fd: RawFd) -> io::Result<usize> {
        // SAFETY: a null buffer with size 0 is the documented way to ask for the
        // required length; `fd` is live.
        let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0, 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(size as usize)
    }

    fn list_xattrs(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buffer` is writable for `buffer.len()` bytes, which is the
        // length we pass, and `fd` is live.
        let read = unsafe { libc::flistxattr(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }

    fn xattr_size(fd: RawFd, name: &[u8]) -> io::Result<usize> {
        let name = cstr(name)?;
        // SAFETY: as `list_xattr_size`, for a single named attribute.
        let size = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(size as usize)
    }

    fn get_xattr(fd: RawFd, name: &[u8], value: &mut [u8]) -> io::Result<usize> {
        let name = cstr(name)?;
        // SAFETY: `value` is writable for `value.len()` bytes, which is the
        // length we pass, and `fd` is live.
        let read = unsafe {
            libc::fgetxattr(
                fd,
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }

    fn set_xattr(fd: RawFd, name: &[u8], value: &[u8]) -> io::Result<()> {
        let name = cstr(name)?;
        // SAFETY: both buffers are valid for the lengths we pass.
        let code =
            unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) };
        if code != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn remove_xattr(fd: RawFd, name: &[u8]) -> io::Result<()> {
        let name = cstr(name)?;
        // SAFETY: `name` is a valid C string and `fd` is live.
        if unsafe { libc::fremovexattr(fd, name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn read_u32(buffer: &[u8], offset: usize) -> io::Result<u32> {
        let bytes: [u8; 4] = buffer
            .get(offset..offset + 4)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| invalid_data(format!("response truncated at offset {offset}")))?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_i32(buffer: &[u8], offset: usize) -> io::Result<i32> {
        let bytes: [u8; 4] = buffer
            .get(offset..offset + 4)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| invalid_data(format!("response truncated at offset {offset}")))?;
        Ok(i32::from_le_bytes(bytes))
    }

    fn read_u64(buffer: &[u8], offset: usize) -> io::Result<u64> {
        let bytes: [u8; 8] = buffer
            .get(offset..offset + 8)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| invalid_data(format!("response truncated at offset {offset}")))?;
        Ok(u64::from_le_bytes(bytes))
    }

    // ─── tests ────────────────────────────────────────────────────────────
    //
    // Ported from Go's `sharing_darwin_test.go`, `safety_darwin_test.go`,
    // `metadata_safety_darwin_test.go` and `attributes_darwin_test.go`, plus
    // one end-to-end pass over a real git worktree. Go's `volumes_darwin_test.go`
    // is deliberately NOT ported: it mounts disposable disk images, which is not
    // available in every macOS sandbox, and Go already gates it behind
    // `TREEHOUSE_TEST_VOLUMES=1`. The volume refusals it covers are still
    // reachable here through `filesystem_reason` and the same-directory check.

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{MetadataExt, symlink};
        use std::path::PathBuf;

        /// A temporary directory on APFS, in its symlink-free form.
        ///
        /// The raw `TempDir` path can sit behind `/var` → `/private/var`, and
        /// the no-follow walk refuses a symlinked component by design. Go's
        /// `cloneFixture` calls `filepath.EvalSymlinks` for the same reason;
        /// every directory handed to `share` in these tests must come from
        /// [`Self::path`], never from the `TempDir` directly.
        struct ApfsDir {
            _root: tempfile::TempDir,
            path: PathBuf,
        }

        impl ApfsDir {
            /// `None` when the temporary directory is not on APFS; the caller
            /// skips, exactly as Go's `cloneFixture` does.
            fn new() -> Option<ApfsDir> {
                let root = tempfile::tempdir().ok()?;
                let path = root.path().canonicalize().ok()?;
                let on_apfs = statfs_path(&path).ok().is_some_and(|fs| is_apfs(&fs));
                on_apfs.then_some(ApfsDir { _root: root, path })
            }

            fn path(&self) -> &Path {
                &self.path
            }
        }

        /// A source/destination pair holding one identical file on APFS. The
        /// Rust form of Go's `cloneFixture`.
        struct Fixture {
            /// Kept alive so the directories outlive every path handed to
            /// `share`.
            _root: ApfsDir,
            source: PathBuf,
            destination: PathBuf,
            data: Vec<u8>,
        }

        impl Fixture {
            fn new() -> Option<Fixture> {
                let root = ApfsDir::new()?;
                let source = root.path().join("source");
                let destination = root.path().join("target");
                let data = b"independent file data\n".repeat(8192);
                for dir in [&source, &destination] {
                    std::fs::create_dir(dir).ok()?;
                    write_test_file(&dir.join("asset"), &data);
                }
                Some(Fixture {
                    _root: root,
                    source,
                    destination,
                    data,
                })
            }
        }

        /// Skips the calling test, loudly, when there is no APFS volume to work
        /// on. A silent pass would be indistinguishable from a real one.
        macro_rules! fixture {
            () => {
                match Fixture::new() {
                    Some(fixture) => fixture,
                    None => {
                        eprintln!("skipping: the temporary directory is not on APFS");
                        eprintln!("  (set TMPDIR to an APFS volume to exercise this path)");
                        return;
                    }
                }
            };
        }

        /// Writes and settles the file, so its APFS allocation is real before
        /// anything measures it.
        fn write_test_file(path: &Path, data: &[u8]) {
            use std::io::Write;
            let mut file = std::fs::File::create(path).expect("create test file");
            file.write_all(data).expect("write test file");
            file.sync_all().expect("settle test file allocation");
        }

        fn edit_in_place(path: &Path, data: &[u8]) {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("open for edit");
            file.seek(SeekFrom::Start(0)).expect("seek");
            file.write_all(data).expect("edit");
        }

        fn attributes_of(path: &Path) -> (Attributes, libc::stat) {
            let file = std::fs::File::open(path).expect("open for attributes");
            let fd = file.as_raw_fd();
            let attributes = file_attributes(fd).expect("read attributes");
            (attributes, fstat(fd).expect("fstat"))
        }

        fn xattrs_of(path: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
            let file = std::fs::File::open(path).expect("open for xattrs");
            read_xattrs(file.as_raw_fd()).expect("read xattrs")
        }

        fn set_path_xattr(path: &Path, name: &str, value: &[u8]) {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("open for xattr");
            set_xattr(file.as_raw_fd(), name.as_bytes(), value).expect("set xattr");
        }

        fn set_mode(path: &Path, mode: mode_t) {
            let file = std::fs::File::open(path).expect("open for chmod");
            fchmod(file.as_raw_fd(), mode).expect("chmod");
        }

        /// `utimensat` takes {access, modification}, the reverse of the
        /// `fsetattrlist` order used to restore them.
        fn set_times(path: &Path, atime: libc::timespec, mtime: libc::timespec) {
            let name = cstr(path.as_os_str().as_bytes()).expect("path");
            let times = [atime, mtime];
            // SAFETY: `name` and `times` are valid for the lengths we pass, and
            // neither is retained.
            let code = unsafe { libc::utimensat(libc::AT_FDCWD, name.as_ptr(), times.as_ptr(), 0) };
            assert_eq!(code, 0, "utimensat: {}", io::Error::last_os_error());
        }

        fn add_acl(path: &Path) {
            let output = std::process::Command::new("chmod")
                .args(["+a", "everyone allow read"])
                .arg(path)
                .output()
                .expect("chmod must be available");
            assert!(
                output.status.success(),
                "chmod +a {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        /// Any staging directory left anywhere under `root` is a bug: the guard
        /// tears it down on every path out of a file.
        fn assert_no_staging(root: &Path) {
            let Ok(entries) = std::fs::read_dir(root) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                assert!(
                    !name.to_string_lossy().starts_with(STAGING_PREFIX),
                    "staging survived: {}",
                    path.display()
                );
                if path.is_dir() {
                    assert_no_staging(&path);
                }
            }
        }

        /// The staging directories currently present directly under `root`.
        fn staging_paths(root: &Path) -> Vec<PathBuf> {
            let Ok(entries) = std::fs::read_dir(root) else {
                return Vec::new();
            };
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX))
                })
                .collect()
        }

        fn skipped(report: &Report, reason: &str) -> u64 {
            report.skipped.get(reason).copied().unwrap_or(0)
        }

        // ── the guarantee that makes the whole thing worth doing ──────────

        /// Go `TestSharePreservesMetadataAndIndependentFiles`. The clone must
        /// be copy-on-write (not a shared writable inode) and must carry the
        /// DESTINATION's metadata, with no source-only xattr leaking across.
        #[test]
        fn share_preserves_metadata_and_keeps_the_files_independent() {
            let fixture = fixture!();
            let source_file = fixture.source.join("asset");
            let destination_file = fixture.destination.join("asset");

            // Make the destination's own metadata distinctive in every
            // dimension the port promises to preserve.
            set_mode(&destination_file, 0o751);
            let stamp = libc::timespec {
                tv_sec: 1_234_567_890,
                tv_nsec: 123_456_789,
            };
            set_times(&destination_file, stamp, stamp);
            set_path_xattr(&source_file, "com.treehouse.source-only", b"donor");
            set_path_xattr(&destination_file, "com.treehouse.destination", b"original");

            let (before, old) = attributes_of(&destination_file);
            let report = share(&fixture.source, &fixture.destination, &["asset"]).expect("pass");
            assert_eq!(report.cloned, 1, "{report}");
            assert_eq!(report.logical_bytes, fixture.data.len() as u64, "{report}");
            assert_eq!(
                report.private_bytes_reduced, before.private_bytes,
                "{report}"
            );

            let (source_attributes, source_stat) = attributes_of(&source_file);
            let (after, stat) = attributes_of(&destination_file);
            assert_ne!(
                source_attributes.clone_id, 0,
                "a shared file has a data stream"
            );
            assert_eq!(
                source_attributes.clone_id, after.clone_id,
                "the data is not shared"
            );
            assert_ne!(source_stat.st_ino, stat.st_ino, "the clone shares an inode");
            assert_ne!(stat.st_ino, old.st_ino, "the destination inode was reused");
            assert_eq!(
                after.private_bytes, 0,
                "the clone still allocates its own data"
            );
            assert_eq!(stat.st_mode, old.st_mode, "mode changed");
            assert_eq!(
                (stat.st_mtime, stat.st_mtime_nsec),
                (old.st_mtime, old.st_mtime_nsec),
                "mtime changed"
            );
            assert_eq!(
                (stat.st_atime, stat.st_atime_nsec),
                (old.st_atime, old.st_atime_nsec),
                "atime changed"
            );

            let xattrs = xattrs_of(&destination_file);
            assert_eq!(
                xattrs
                    .get(b"com.treehouse.destination".as_slice())
                    .map(Vec::as_slice),
                Some(b"original".as_slice()),
                "the destination's own xattr was not preserved"
            );
            assert!(
                !xattrs.contains_key(b"com.treehouse.source-only".as_slice()),
                "a source-only xattr leaked onto the destination"
            );

            // A second pass has nothing left to do.
            let report = share(&fixture.source, &fixture.destination, &["asset"]).expect("pass");
            assert_eq!(skipped(&report, "already_shared"), 1, "{report}");

            // The whole point: copy-on-write, not a shared writable inode.
            edit_in_place(&destination_file, b"target edit");
            assert_eq!(
                std::fs::read(&source_file).expect("read source"),
                fixture.data,
                "a target edit changed the source"
            );
            write_test_file(&source_file, b"source edit");
            std::fs::remove_file(&source_file).expect("remove source");
            let published = std::fs::read(&destination_file).expect("read target");
            assert!(published.starts_with(b"target edit"));
            assert_eq!(
                &published[11..],
                &fixture.data[11..],
                "a source edit or deletion affected the clone"
            );
            assert_no_staging(&fixture.destination);
        }

        // ── refusals ─────────────────────────────────────────────────────

        /// Go `TestShareSameSizeDifferentBytes`.
        #[test]
        fn share_refuses_same_size_different_bytes() {
            let fixture = fixture!();
            write_test_file(
                &fixture.destination.join("asset"),
                &vec![b'x'; fixture.data.len()],
            );
            let report = share(&fixture.source, &fixture.destination, &["asset"]).expect("pass");
            assert_eq!(skipped(&report, "different_content"), 1, "{report}");
            assert_eq!(report.cloned, 0, "{report}");
        }

        /// Go `TestShareSymlinkParentsAndUnsafePaths`.
        #[test]
        fn share_refuses_unsafe_and_symlinked_paths() {
            let fixture = fixture!();
            let destination_file = fixture.destination.join("asset");
            symlink(&fixture.destination, fixture.destination.join("link")).expect("symlink");

            let report = share(
                &fixture.source,
                &fixture.destination,
                &["link/asset", "../target/asset"],
            )
            .expect("pass");
            assert_eq!(
                skipped(&report, "destination_missing_or_symlink"),
                1,
                "{report}"
            );
            assert_eq!(skipped(&report, "unsafe_path"), 1, "{report}");

            // A symlinked ROOT is refused outright, and its target is untouched.
            let alias = fixture
                .destination
                .parent()
                .expect("a fixture root")
                .join("alias");
            symlink(&fixture.destination, &alias).expect("symlink");
            let report = share(&fixture.source, &alias, &["asset"]).expect("pass");
            assert!(report.reason.contains("symlink"), "{report}");
            assert_eq!(
                std::fs::read(&destination_file).expect("read"),
                fixture.data,
                "the symlink target changed"
            );
        }

        /// Go `TestShareMinimumSizeBoundary`. One byte either side of the
        /// threshold decides whether the setup cost is worth paying.
        #[test]
        fn share_honors_the_minimum_size_boundary() {
            for size in [MINIMUM_SIZE - 1, MINIMUM_SIZE] {
                let fixture = fixture!();
                let data = vec![b'a'; size as usize];
                write_test_file(&fixture.source.join("asset"), &data);
                write_test_file(&fixture.destination.join("asset"), &data);
                let report =
                    share(&fixture.source, &fixture.destination, &["asset"]).expect("pass");
                assert_eq!(
                    report.cloned,
                    u64::from(size == MINIMUM_SIZE),
                    "size {size}: {report}"
                );
            }
        }

        /// Go `TestShareSafeSkips`. Each case must leave the destination
        /// exactly as it found it.
        #[test]
        fn share_refuses_cases_it_cannot_reproduce() {
            struct Case {
                name: &'static str,
                reason: &'static str,
                change: fn(&Fixture),
            }
            let cases = [
                Case {
                    name: "different content",
                    reason: "different_content",
                    change: |fixture| {
                        write_test_file(&fixture.destination.join("asset"), &vec![b'x'; 172_032])
                    },
                },
                Case {
                    name: "below threshold",
                    reason: "below_threshold",
                    change: |fixture| write_test_file(&fixture.destination.join("asset"), b"small"),
                },
                Case {
                    name: "hardlink",
                    reason: "hardlink",
                    change: |fixture| {
                        std::fs::hard_link(
                            fixture.destination.join("asset"),
                            fixture.destination.join("alias"),
                        )
                        .expect("hard link");
                    },
                },
                Case {
                    name: "symlink",
                    reason: "destination_not_regular_or_symlink",
                    change: |fixture| {
                        std::fs::remove_file(fixture.destination.join("asset")).expect("remove");
                        symlink(
                            fixture.source.join("asset"),
                            fixture.destination.join("asset"),
                        )
                        .expect("symlink");
                    },
                },
                Case {
                    name: "file ACL",
                    reason: "acl",
                    change: |fixture| add_acl(&fixture.destination.join("asset")),
                },
                Case {
                    name: "parent ACL",
                    reason: "acl",
                    change: |fixture| add_acl(&fixture.destination),
                },
            ];
            for case in cases {
                let Some(fixture) = Fixture::new() else {
                    eprintln!(
                        "skipping {}: the temporary directory is not on APFS",
                        case.name
                    );
                    continue;
                };
                (case.change)(&fixture);
                let before = std::fs::symlink_metadata(fixture.destination.join("asset"))
                    .expect("lstat before");
                let report =
                    share(&fixture.source, &fixture.destination, &["asset"]).expect("pass");
                assert_eq!(report.cloned, 0, "{}: {report}", case.name);
                assert_eq!(skipped(&report, case.reason), 1, "{}: {report}", case.name);
                let after = std::fs::symlink_metadata(fixture.destination.join("asset"))
                    .expect("lstat after");
                assert_eq!(
                    (before.dev(), before.ino(), before.mtime()),
                    (after.dev(), after.ino(), after.mtime()),
                    "{}: the skip replaced the destination",
                    case.name
                );
                assert_no_staging(&fixture.destination);
            }
        }

        /// Go `TestSharingUnsupportedRepresentations`. These are the cases
        /// where a matching hash does NOT mean the same file.
        #[test]
        fn share_refuses_unsupported_representations() {
            struct Case {
                name: &'static str,
                reason: &'static str,
                change: fn(&Fixture),
            }
            let cases = [
                Case {
                    name: "flags",
                    reason: "unsupported_flags_or_mode",
                    change: |fixture| {
                        let target = cstr(fixture.destination.join("asset").as_os_str().as_bytes())
                            .expect("path");
                        // SAFETY: `target` is a valid C string and the flag is a
                        // documented `UF_*` value.
                        let code = unsafe { libc::chflags(target.as_ptr(), libc::UF_HIDDEN) };
                        assert_eq!(code, 0, "chflags: {}", io::Error::last_os_error());
                    },
                },
                Case {
                    name: "resource fork",
                    reason: "representation_xattr",
                    change: |fixture| {
                        set_path_xattr(
                            &fixture.destination.join("asset"),
                            "com.apple.ResourceFork",
                            b"representation metadata",
                        )
                    },
                },
                Case {
                    name: "sparse",
                    reason: "sparse_or_compressed",
                    change: |fixture| {
                        for root in [&fixture.source, &fixture.destination] {
                            let file = std::fs::OpenOptions::new()
                                .write(true)
                                .open(root.join("asset"))
                                .expect("open for truncate");
                            file.set_len(8 * 1024 * 1024).expect("truncate");
                        }
                    },
                },
            ];
            for case in cases {
                let Some(fixture) = Fixture::new() else {
                    eprintln!(
                        "skipping {}: the temporary directory is not on APFS",
                        case.name
                    );
                    continue;
                };
                (case.change)(&fixture);
                let target = fixture.destination.join("asset");
                let before = std::fs::read(&target).expect("read before");
                let report =
                    share(&fixture.source, &fixture.destination, &["asset"]).expect("pass");
                assert_eq!(report.cloned, 0, "{}: {report}", case.name);
                assert_eq!(skipped(&report, case.reason), 1, "{}: {report}", case.name);
                let after = std::fs::read(&target).expect("read after");
                assert_eq!(
                    before, after,
                    "{}: an unsupported representation changed",
                    case.name
                );
                assert_no_staging(&fixture.destination);
            }
        }

        /// Both sides missing, for different reasons, must not be conflated:
        /// only the destination's identity is ever at risk.
        #[test]
        fn missing_side_reasons_are_distinguished() {
            let fixture = fixture!();
            std::fs::remove_file(fixture.source.join("asset")).expect("remove source");
            let report = share(&fixture.source, &fixture.destination, &["asset"]).expect("pass");
            assert_eq!(
                skipped(&report, "source_not_regular_or_symlink"),
                1,
                "{report}"
            );

            let fixture = fixture!();
            std::fs::remove_file(fixture.destination.join("asset")).expect("remove destination");
            let report = share(&fixture.source, &fixture.destination, &["asset"]).expect("pass");
            assert_eq!(
                skipped(&report, "destination_not_regular_or_symlink"),
                1,
                "{report}"
            );
        }

        // ── failure handling ─────────────────────────────────────────────

        /// Go `TestShareFailureKeepsOriginal`. A failed `clonefileat` is a skip,
        /// never a lost file: the caller falls back to a plain copy.
        #[test]
        fn share_keeps_the_original_when_cloning_fails() {
            for (name, code) in [("ENOSPC", libc::ENOSPC), ("ENOTSUP", libc::ENOTSUP)] {
                let fixture = fixture!();
                let ops = Ops {
                    clone: &|_, _, _| Err(io::Error::from_raw_os_error(code)),
                    metadata: &PRODUCTION_METADATA,
                };
                let report = share_with(
                    &Cancel::never(),
                    &fixture.source,
                    &fixture.destination,
                    &["asset"],
                    &ops,
                )
                .expect("a per-file failure is not fatal");
                assert_eq!(skipped(&report, "kept_after_error"), 1, "{name}: {report}");
                assert_eq!(report.errors.len(), 1, "{name}: {report}");
                assert_eq!(
                    std::fs::read(fixture.destination.join("asset")).expect("read"),
                    fixture.data,
                    "{name}: the failure changed the original"
                );
                assert_no_staging(&fixture.destination);
            }
        }

        /// Go `TestShareSourceMutationAndMetadataFailure`, metadata branch.
        #[test]
        fn share_keeps_the_original_when_metadata_cannot_be_restored() {
            let fixture = fixture!();
            let ops = Ops {
                clone: &PRODUCTION_CLONE,
                metadata: &|_, _| Err(io::Error::from_raw_os_error(libc::EPERM)),
            };
            let report = share_with(
                &Cancel::never(),
                &fixture.source,
                &fixture.destination,
                &["asset"],
                &ops,
            )
            .expect("a per-file failure is not fatal");
            assert_eq!(report.cloned, 0, "{report}");
            assert_eq!(skipped(&report, "kept_after_error"), 1, "{report}");
            assert_eq!(
                std::fs::read(fixture.destination.join("asset")).expect("read"),
                fixture.data,
                "the failure published something"
            );
            assert_no_staging(&fixture.destination);
        }

        /// Go `TestShareSourceMutationAndMetadataFailure`, source branch: the
        /// donor changed between our hash and the clone, so the clone is not the
        /// bytes we verified.
        #[test]
        fn share_discards_a_clone_of_a_source_that_changed() {
            let fixture = fixture!();
            let source_file = fixture.source.join("asset");
            let length = fixture.data.len();
            let ops = Ops {
                clone: &|source, dir, name| {
                    // The donor changes BEFORE the clone, so the staged copy is
                    // not the bytes we hashed.
                    write_test_file(&source_file, &vec![b'x'; length]);
                    clone_file(source, dir, name)
                },
                metadata: &PRODUCTION_METADATA,
            };
            let report = share_with(
                &Cancel::never(),
                &fixture.source,
                &fixture.destination,
                &["asset"],
                &ops,
            )
            .expect("pass");
            assert_eq!(report.cloned, 0, "{report}");
            assert_eq!(skipped(&report, "source_changed"), 1, "{report}");
            assert_eq!(
                std::fs::read(fixture.destination.join("asset")).expect("read"),
                fixture.data,
                "unverified content was published"
            );
            assert_no_staging(&fixture.destination);
        }

        /// Go `TestShareRejectsMetadataContentCorruption`. Restoring metadata
        /// must not be able to rewrite content and still pass.
        #[test]
        fn share_discards_a_clone_whose_metadata_changed_its_content() {
            let fixture = fixture!();
            let length = fixture.data.len();
            let ops = Ops {
                clone: &PRODUCTION_CLONE,
                metadata: &|fd, wanted| {
                    let staged = staging_paths(&fixture.destination)
                        .into_iter()
                        .next()
                        .expect("one staged clone");
                    write_test_file(&staged.join("clone"), &vec![b'x'; length]);
                    PRODUCTION_METADATA(fd, wanted)
                },
            };
            let report = share_with(
                &Cancel::never(),
                &fixture.source,
                &fixture.destination,
                &["asset"],
                &ops,
            )
            .expect("pass");
            assert_eq!(report.cloned, 0, "{report}");
            assert_eq!(skipped(&report, "metadata_changed_content"), 1, "{report}");
            assert_eq!(
                std::fs::read(fixture.destination.join("asset")).expect("read"),
                fixture.data,
                "corrupted content was published"
            );
            assert_no_staging(&fixture.destination);
        }

        /// Go `TestShareDetectsDestinationWriterAndCancellation`, writer branch:
        /// a concurrent writer to the destination aborts the pass and its bytes
        /// survive.
        #[test]
        fn share_refuses_to_publish_over_a_concurrent_write() {
            let fixture = fixture!();
            let destination_file = fixture.destination.join("asset");
            let ops = Ops {
                clone: &|source, dir, name| {
                    clone_file(source, dir, name)?;
                    write_test_file(&destination_file, b"new edit");
                    Ok(())
                },
                metadata: &PRODUCTION_METADATA,
            };
            let error = share_with(
                &Cancel::never(),
                &fixture.source,
                &fixture.destination,
                &["asset"],
                &ops,
            )
            .expect_err("a concurrent writer must abort the pass");
            assert!(
                matches!(error, ShareError::DestinationChanged(_)),
                "{error}"
            );
            assert_eq!(
                std::fs::read(&destination_file).expect("read"),
                b"new edit",
                "the concurrent write was lost"
            );
            assert_no_staging(&fixture.destination);
        }

        /// Go `TestShareDetectsDestinationWriterAndCancellation`, cancel branch.
        /// Cancellation must not publish and must not leave staging behind.
        #[test]
        fn share_aborts_on_cancellation_and_keeps_the_original() {
            let fixture = fixture!();
            let token = Cancel::new();
            let ops = Ops {
                clone: &|source, dir, name| {
                    clone_file(source, dir, name)?;
                    token.cancel();
                    Ok(())
                },
                metadata: &PRODUCTION_METADATA,
            };
            let error = share_with(
                &token,
                &fixture.source,
                &fixture.destination,
                &["asset"],
                &ops,
            )
            .expect_err("cancellation must abort the pass");
            assert!(matches!(error, ShareError::Cancelled), "{error}");
            assert_eq!(
                std::fs::read(fixture.destination.join("asset")).expect("read"),
                fixture.data,
                "a cancelled pass published something"
            );
            assert_no_staging(&fixture.destination);
        }

        /// A pass cancelled before it starts shares nothing at all.
        #[test]
        fn a_pass_cancelled_before_it_starts_shares_nothing() {
            let fixture = fixture!();
            let token = Cancel::new();
            token.cancel();
            let error =
                share_cancellable(&fixture.source, &fixture.destination, &["asset"], &token)
                    .expect_err("cancellation must abort the pass");
            assert!(matches!(error, ShareError::Cancelled), "{error}");
            assert_no_staging(&fixture.destination);
        }

        /// Go `TestShareRejectsDestinationParentReplacement`. The parent is
        /// re-resolved through the descriptor we pinned, so a directory swapped
        /// underneath us is caught — and the new parent's file is not
        /// overwritten by a clone staged in the old one.
        #[test]
        fn share_refuses_to_publish_into_a_replaced_destination_parent() {
            let fixture = fixture!();
            for root in [&fixture.source, &fixture.destination] {
                std::fs::create_dir(root.join("sub")).expect("mkdir");
                std::fs::rename(root.join("asset"), root.join("sub/asset")).expect("move");
            }
            let sub = fixture.destination.join("sub");
            let ops = Ops {
                clone: &|source, dir, name| {
                    clone_file(source, dir, name)?;
                    std::fs::rename(&sub, fixture.destination.join("saved")).expect("rename away");
                    std::fs::create_dir(&sub).expect("mkdir");
                    write_test_file(&sub.join("asset"), b"replacement");
                    Ok(())
                },
                metadata: &PRODUCTION_METADATA,
            };
            let error = share_with(
                &Cancel::never(),
                &fixture.source,
                &fixture.destination,
                &["sub/asset"],
                &ops,
            )
            .expect_err("a replaced parent must abort the pass");
            assert!(
                matches!(error, ShareError::DestinationChanged(_)),
                "{error}"
            );
            assert_eq!(
                std::fs::read(sub.join("asset")).expect("read"),
                b"replacement",
                "the new parent was overwritten"
            );
            assert_eq!(
                std::fs::read(fixture.destination.join("saved/asset")).expect("read"),
                fixture.data,
                "the old parent was overwritten"
            );
            assert_no_staging(&fixture.destination);
        }

        /// Go `TestSharingIncompleteCleanupFailsWithoutDeletingUnknownData`.
        /// A staging directory we cannot empty is reported, and the data in it
        /// is left for the operator instead of being deleted recursively.
        #[test]
        fn an_incomplete_cleanup_aborts_without_deleting_unknown_data() {
            let fixture = fixture!();
            let ops = Ops {
                clone: &PRODUCTION_CLONE,
                metadata: &|fd, wanted| {
                    let staged = staging_paths(&fixture.destination)
                        .into_iter()
                        .next()
                        .expect("one staging directory");
                    write_test_file(&staged.join("unknown"), b"preserve this");
                    PRODUCTION_METADATA(fd, wanted)
                },
            };
            let error = share_with(
                &Cancel::never(),
                &fixture.source,
                &fixture.destination,
                &["asset"],
                &ops,
            )
            .expect_err("an incomplete cleanup must abort the pass");
            assert!(matches!(error, ShareError::Cleanup(_)), "{error}");
            let survived = staging_paths(&fixture.destination);
            assert_eq!(survived.len(), 1, "{error}");
            assert_eq!(
                std::fs::read(survived[0].join("unknown")).expect("read"),
                b"preserve this",
                "cleanup recursively deleted unknown data"
            );
            assert_eq!(
                std::fs::read(fixture.destination.join("asset")).expect("read"),
                fixture.data,
                "the cleanup error changed bytes"
            );
        }

        // ── preflight and attributes ─────────────────────────────────────

        /// Go `TestAllocationAndACLAttributes`, which is also the live check
        /// that the `getattrlist` response offsets in this module are right:
        /// every one of these assertions depends on them.
        #[test]
        fn attributes_report_allocation_and_detect_acls() {
            let Some(root) = ApfsDir::new() else {
                eprintln!("skipping: the temporary directory is not on APFS");
                return;
            };
            let source = root.path().join("source");
            let target = root.path().join("target");
            write_test_file(&source, &vec![b'a'; 128 * 1024]);

            let (before, _) = attributes_of(&source);
            assert!(!before.acl, "an ordinary file has no ACL: {before:?}");
            assert!(
                before.private_bytes >= 128 * 1024,
                "an ordinary file allocates its own data: {before:?}"
            );

            if let Err(error) = clonefile_path(&source, &target) {
                eprintln!("skipping: APFS clone unavailable: {error}");
                return;
            }
            let (from, _) = attributes_of(&source);
            let (into, _) = attributes_of(&target);
            assert_ne!(from.clone_id, 0, "a clone has a data stream");
            assert_eq!(from.clone_id, into.clone_id, "clonefile did not share data");
            assert_eq!(
                into.private_bytes, 0,
                "the clone still allocates its own data"
            );

            add_acl(&target);
            assert!(attributes_of(&target).0.acl, "ACL undetected");
        }

        fn clonefile_path(source: &Path, target: &Path) -> io::Result<()> {
            let source = cstr(source.as_os_str().as_bytes())?;
            let target = cstr(target.as_os_str().as_bytes())?;
            // SAFETY: both are valid C strings for the duration of the call.
            let code = unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) };
            if code != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// The preflight is a hint, but it must never disagree with the check
        /// the pass itself makes.
        #[test]
        fn the_preflight_agrees_with_the_pass() {
            let fixture = fixture!();
            assert_eq!(
                filesystem_reason(&fixture.source, &fixture.destination),
                "",
                "one APFS volume must preflight clean"
            );
            assert_eq!(
                filesystem_reason(Path::new("/does/not/exist"), &fixture.destination),
                "filesystem cannot be verified"
            );
            let report = share(&fixture.source, &fixture.source, &["asset"]).expect("pass");
            assert!(report.reason.contains("same directory"), "{report}");
            assert_no_staging(&fixture.destination);
        }

        // ── end to end ───────────────────────────────────────────────────

        /// The real path: a git repository, a real linked worktree, and a large
        /// tracked file shared between them. Everything above works on
        /// synthetic fixtures; this proves the module composes with the
        /// checkout it will be pointed at.
        #[test]
        fn shares_a_large_tracked_file_into_a_real_git_worktree() {
            let Some(root) = ApfsDir::new() else {
                eprintln!("skipping: the temporary directory is not on APFS");
                return;
            };
            if std::process::Command::new("git")
                .arg("--version")
                .output()
                .is_err()
            {
                eprintln!("skipping: git is not installed");
                return;
            }
            let repo = root.path().join("repo");
            let worktree = root.path().join("slot");
            let git = |dir: &Path, args: &[&str]| {
                let output = std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .output()
                    .expect("git must be installed");
                assert!(
                    output.status.success(),
                    "git {args:?} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            let git_bytes = |dir: &Path, args: &[&str]| {
                let output = std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .output()
                    .expect("git must be installed");
                assert!(
                    output.status.success(),
                    "git {args:?} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                output.stdout
            };

            git(
                root.path(),
                &["init", "--initial-branch=main", &repo.to_string_lossy()],
            );
            git(&repo, &["config", "user.email", "t@t.com"]);
            git(&repo, &["config", "user.name", "T"]);
            let payload: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
            write_test_file(&repo.join("payload.bin"), &payload);
            git(&repo, &["add", "payload.bin"]);
            git(&repo, &["commit", "-m", "add payload"]);
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    &worktree.to_string_lossy(),
                    "HEAD",
                ],
            );

            assert_eq!(
                filesystem_reason(&repo, &worktree),
                "",
                "the repo and the pool slot must be on one APFS volume"
            );
            let report = share(&repo, &worktree, &["payload.bin"]).expect("pass");
            assert_eq!(report.cloned, 1, "{report}");
            assert_eq!(report.logical_bytes, payload.len() as u64, "{report}");
            assert!(
                report.private_bytes_reduced > 0,
                "the checkout did not stop allocating its own data: {report}"
            );

            // The checkout is still byte-for-byte what git wrote, and git still
            // considers the worktree clean.
            assert_eq!(
                std::fs::read(worktree.join("payload.bin")).expect("read"),
                payload,
                "sharing corrupted the checkout"
            );
            let status = git_bytes(
                &worktree,
                &["-c", "core.fsmonitor=false", "status", "--porcelain=v1"],
            );
            assert!(
                status.is_empty(),
                "git status after sharing: {}",
                String::from_utf8_lossy(&status)
            );

            // The source kept its own allocation, so a write to the worktree
            // has to be free of side effects.
            assert_eq!(
                std::fs::read(repo.join("payload.bin")).expect("read"),
                payload,
                "the source changed"
            );
            let (source_attributes, _) = attributes_of(&repo.join("payload.bin"));
            let (worktree_attributes, _) = attributes_of(&worktree.join("payload.bin"));
            assert_ne!(source_attributes.clone_id, 0);
            assert_eq!(
                source_attributes.clone_id, worktree_attributes.clone_id,
                "the worktree copy is not sharing data with the repository"
            );
            // APFS reports zero private bytes at BOTH ends of a shared data
            // stream, so only the clone's own figure proves the saving landed.
            assert_eq!(
                worktree_attributes.private_bytes, 0,
                "the worktree copy still allocates its own data"
            );
            assert_no_staging(&worktree);

            git(
                &repo,
                &["worktree", "remove", "--force", &worktree.to_string_lossy()],
            );
        }
    }
}

// ─── everything else ─────────────────────────────────────────────────────────

/// Non-macOS builds. Fourteen lines, like Go's `sharing_other.go`: the reason
/// is already decided, and touching the caller's filesystem would be behavior
/// this module has no way to make safe.
#[cfg(not(target_os = "macos"))]
mod other {
    use super::{Cancel, Report, ShareError};
    use std::path::Path;

    pub fn filesystem_reason(_source: &Path, _destination: &Path) -> String {
        "requires macOS APFS".to_string()
    }

    /// Does not inspect the paths it is handed.
    pub fn share(
        _source: &Path,
        _destination: &Path,
        _paths: &[impl AsRef<str>],
    ) -> Result<Report, ShareError> {
        unsupported()
    }

    pub fn share_cancellable(
        source: &Path,
        destination: &Path,
        paths: &[impl AsRef<str>],
        _cancel: &Cancel,
    ) -> Result<Report, ShareError> {
        share(source, destination, paths)
    }

    fn unsupported() -> Result<Report, ShareError> {
        Ok(Report {
            reason: "requires macOS APFS".to_string(),
            ..Report::default()
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Go `sharing_other_test.go`: the unsupported platform must not
        /// inspect the paths it is handed, and must report rather than fail.
        #[test]
        fn unsupported_platform_does_not_inspect_paths() {
            let report = share(
                Path::new("nonexistent-source"),
                Path::new("nonexistent-destination"),
                &["asset"],
            )
            .expect("unsupported platforms never fail the pass");
            // `SUPPORTED` is covered by `supported_tests`; this module only
            // compiles when it is false, so asserting it here would be a
            // tautology.
            assert_eq!(report.cloned, 0);
            assert!(report.reason.contains("requires macOS"), "{report}");
            assert!(report.skipped.is_empty());
        }

        #[test]
        fn unsupported_platform_preflight_always_refuses() {
            assert_eq!(
                filesystem_reason(Path::new("/"), Path::new("/")),
                "requires macOS APFS"
            );
        }
    }
}

#[cfg(test)]
mod supported_tests {
    #[test]
    fn support_matches_the_platform() {
        assert_eq!(super::SUPPORTED, cfg!(target_os = "macos"));
    }

    #[test]
    fn minimum_size_is_the_go_constant() {
        assert_eq!(super::MINIMUM_SIZE, 64 * 1024);
    }
}
