//! Pure destroy/prune/gc classification.
//!
//! This is the shared safety engine. Go's destroy and prune reuse the same
//! primitives (`ownerAlive`, `FindProcessesInWorktree`,
//! `backingRepositoryMissing`, `git.IsDirty`, `git.IsHeadMergedIntoRef`) so
//! they agree on "disposable" by construction. This module is **pure**: no
//! disk, no git, no processes — all inputs are passed in so the layer is
//! unit-testable and destroy/prune/gc share one classifier.
//!
//! A target can accumulate MULTIPLE classes (e.g. `leased+dirty`), each gated
//! by its own `--include-*` flag. Anything that cannot be verified (proc-scan
//! failure, unresolvable default ref) classifies `UNVERIFIED` — fail closed,
//! never disposable.

use crate::process::ProcessInfo;

/// A managed worktree reference (name + path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRef {
    pub name: String,
    pub path: String,
}

/// A destroy/prune risk class (Go `DestroyClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DestroyClass {
    Disposable,
    Leased,
    InUse,
    Dirty,
    Unmerged,
    Unverified,
}

impl std::fmt::Display for DestroyClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DestroyClass::Disposable => "disposable",
            DestroyClass::Leased => "leased",
            DestroyClass::InUse => "in-use",
            DestroyClass::Dirty => "dirty",
            DestroyClass::Unmerged => "unmerged",
            DestroyClass::Unverified => "unverified",
        };
        f.write_str(s)
    }
}

/// A bitmask of [`DestroyClass`] values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClassSet(u32);

impl ClassSet {
    pub const DISPOSABLE: u32 = 1 << 0;
    pub const LEASED: u32 = 1 << 1;
    pub const IN_USE: u32 = 1 << 2;
    pub const DIRTY: u32 = 1 << 3;
    pub const UNMERGED: u32 = 1 << 4;
    pub const UNVERIFIED: u32 = 1 << 5;

    pub fn add(&mut self, class: DestroyClass) {
        self.0 |= Self::bit(class);
    }

    pub fn contains(&self, class: DestroyClass) -> bool {
        self.0 & Self::bit(class) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub fn has_unlanded(&self) -> bool {
        self.contains(DestroyClass::Dirty)
            || self.contains(DestroyClass::Unmerged)
            || self.contains(DestroyClass::Unverified)
    }

    fn bit(class: DestroyClass) -> u32 {
        match class {
            DestroyClass::Disposable => Self::DISPOSABLE,
            DestroyClass::Leased => Self::LEASED,
            DestroyClass::InUse => Self::IN_USE,
            DestroyClass::Dirty => Self::DIRTY,
            DestroyClass::Unmerged => Self::UNMERGED,
            DestroyClass::Unverified => Self::UNVERIFIED,
        }
    }
}

/// Destroy options (which risk classes are opted into).
#[derive(Debug, Clone, Copy, Default)]
pub struct DestroyOptions {
    pub allow_leased: bool,
    pub include_leased: bool,
    pub include_in_use: bool,
    pub include_unlanded: bool,
}

impl DestroyOptions {
    /// The `--include-*` flags missing for a target of these classes.
    /// Multiple classes -> every flag needed.
    pub fn missing_flags(&self, classes: &ClassSet) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if classes.contains(DestroyClass::Leased) {
            // Leased is removable only via a single named target with
            // --include-leased (never bulk --all => allow_leased false).
            if !(self.allow_leased && self.include_leased) {
                missing.push("--include-leased");
            }
        }
        if classes.contains(DestroyClass::InUse) && !self.include_in_use {
            missing.push("--include-in-use");
        }
        if classes.has_unlanded() && !self.include_unlanded {
            missing.push("--include-unlanded");
        }
        missing
    }
}

/// Inputs needed to classify a worktree (all pre-computed by the caller).
#[derive(Debug, Clone, Default)]
pub struct ClassCheckResults {
    /// Processes running inside the worktree (empty if none / scan succeeded).
    pub processes: Vec<ProcessInfo>,
    /// `Some(true)` when the backing repository's git metadata is missing.
    pub backing_repo_missing: Option<bool>,
    /// `Some(true)` when the worktree has uncommitted/untracked changes.
    pub dirty: Option<bool>,
    /// `Some(true)` when HEAD is merged into the default ref.
    pub merged: Option<bool>,
    /// The default merge ref; empty string means it couldn't be resolved.
    pub default_ref: String,
    /// True when the process scan failed (fails closed to IN_USE).
    pub proc_scan_error: bool,
}

/// A classified worktree target: its classes plus the primary class + detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedTarget {
    pub name: String,
    pub path: String,
    pub classes: ClassSet,
    pub class: DestroyClass,
    pub detail: String,
    /// Processes found inside the worktree (for status/display).
    pub processes: Vec<ProcessInfo>,
}

/// Classifies a worktree for destroy/prune/gc following Go's
/// `classifyForDestroy` order exactly.
///
/// `owner_alive` is supplied by the caller (it needs process lookup). The
/// result is **pure** — no I/O happens here.
pub fn classify_for_destroy(
    name: &str,
    path: &str,
    leased: bool,
    lease_holder: &str,
    owner_alive: bool,
    checks: &ClassCheckResults,
) -> ClassifiedTarget {
    let mut target = ClassifiedTarget {
        name: name.to_string(),
        path: path.to_string(),
        classes: ClassSet::default(),
        class: DestroyClass::Disposable,
        detail: String::new(),
        processes: checks.processes.clone(),
    };

    // 1. Leased (process-independent) => LEASED.
    if leased {
        let detail = if lease_holder.is_empty() {
            String::new()
        } else {
            format!("held by {lease_holder}")
        };
        target.add_class(DestroyClass::Leased, detail);
    }

    // 2. In-use (owner_alive OR running process). A proc-scan error fails
    //    closed to IN_USE.
    let proc_err = checks.proc_scan_error;
    if owner_alive || !checks.processes.is_empty() {
        target.add_class(DestroyClass::InUse, "");
    }
    if proc_err {
        target.add_class(DestroyClass::InUse, "cannot check processes");
    }

    // 3. Backing repo missing => UNVERIFIED, stop.
    if checks.backing_repo_missing == Some(true) {
        target.add_class(DestroyClass::Unverified, "backing repository missing");
        return target;
    }

    // 4. Dirty.
    match checks.dirty {
        Some(true) => target.add_class(DestroyClass::Dirty, "uncommitted changes"),
        Some(false) => {}
        None => {
            target.add_class(DestroyClass::Unverified, "cannot check status");
            return target;
        }
    }

    // 5. Merge verification against the default ref.
    if checks.default_ref.is_empty() {
        target.add_class(
            DestroyClass::Unverified,
            "cannot verify HEAD is merged into the default branch",
        );
        return target;
    }
    match checks.merged {
        Some(true) => {}
        Some(false) => target.add_class(
            DestroyClass::Unmerged,
            format!("HEAD not merged into {}", checks.default_ref),
        ),
        None => {
            target.add_class(
                DestroyClass::Unverified,
                format!("cannot verify merge into {}", checks.default_ref),
            );
            return target;
        }
    }

    target
}

impl ClassifiedTarget {
    fn add_class(&mut self, class: DestroyClass, detail: impl Into<String>) {
        if self.classes.contains(class) {
            return;
        }
        self.classes.add(class);
        if self.classes.0 == ClassSet::bit(class) {
            self.class = class;
            self.detail = detail.into();
        }
    }

    /// Whether a target is disposable: no risk classes present.
    pub fn is_disposable(&self) -> bool {
        self.classes.contains(DestroyClass::Disposable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(allow_leased: bool, leased: bool, in_use: bool, unlanded: bool) -> DestroyOptions {
        DestroyOptions {
            allow_leased,
            include_leased: leased,
            include_in_use: in_use,
            include_unlanded: unlanded,
        }
    }

    fn classify(
        leased: bool,
        owner_alive: bool,
        procs: usize,
        backing_missing: Option<bool>,
        dirty: Option<bool>,
        merged: Option<bool>,
        default_ref: &str,
    ) -> ClassifiedTarget {
        classify_for_destroy(
            "1",
            "/pool/1/myrepo",
            leased,
            if leased { "holder" } else { "" },
            owner_alive,
            &ClassCheckResults {
                processes: vec![
                    ProcessInfo {
                        pid: 1,
                        name: "x".into()
                    };
                    procs
                ],
                backing_repo_missing: backing_missing,
                dirty,
                merged,
                default_ref: default_ref.to_string(),
                proc_scan_error: false,
            },
        )
    }

    #[test]
    fn disposable_when_clean_idle_unleased_merged() {
        let t = classify(
            false,
            false,
            0,
            Some(false),
            Some(false),
            Some(true),
            "refs/heads/main",
        );
        assert!(!t.classes.has_unlanded());
        assert!(
            t.classes.is_empty(),
            "no classes => finalize adds DISPOSABLE"
        );
        // The pure classifier leaves the target empty; the caller adds
        // DISPOSABLE when classes are empty (Go finalizeDestroyTarget).
        assert!(t.classes.contains(DestroyClass::Disposable) || t.classes.is_empty());
    }

    #[test]
    fn leased_dirty_requires_both_flags() {
        let t = classify(
            true,
            false,
            0,
            Some(false),
            Some(true),
            Some(false),
            "refs/heads/main",
        );
        assert!(t.classes.contains(DestroyClass::Leased));
        assert!(t.classes.contains(DestroyClass::Dirty));
        // Needs BOTH --include-leased AND --include-unlanded.
        let missing = opts(true, false, false, false).missing_flags(&t.classes);
        assert!(missing.contains(&"--include-leased"));
        assert!(missing.contains(&"--include-unlanded"));
        // allow_leased=false => --include-leased always required.
        let missing2 = opts(false, true, false, true).missing_flags(&t.classes);
        assert!(missing2.contains(&"--include-leased"));
    }

    #[test]
    fn in_use_requires_include_in_use() {
        let t = classify(
            false,
            true,
            1,
            Some(false),
            Some(false),
            Some(true),
            "refs/heads/main",
        );
        assert!(t.classes.contains(DestroyClass::InUse));
        let missing = opts(false, false, false, false).missing_flags(&t.classes);
        assert!(missing.contains(&"--include-in-use"));
        // include_in_use alone clears it.
        let missing2 = opts(false, false, true, false).missing_flags(&t.classes);
        assert!(!missing2.contains(&"--include-in-use"));
    }

    #[test]
    fn backing_missing_is_unverified_fail_closed() {
        let t = classify(false, false, 0, Some(true), None, None, "");
        assert!(t.classes.contains(DestroyClass::Unverified));
        assert!(!t.classes.contains(DestroyClass::Disposable));
    }

    #[test]
    fn proc_scan_failure_fails_closed_to_in_use() {
        let t = classify_for_destroy(
            "1",
            "/pool/1/myrepo",
            false,
            "",
            false,
            &ClassCheckResults {
                processes: vec![],
                backing_repo_missing: Some(false),
                dirty: Some(false),
                merged: Some(true),
                default_ref: "refs/heads/main".into(),
                proc_scan_error: true,
            },
        );
        assert!(t.classes.contains(DestroyClass::InUse));
        assert_eq!(t.detail, "cannot check processes");
    }

    #[test]
    fn unresolvable_default_ref_is_unverified() {
        let t = classify(false, false, 0, Some(false), Some(false), None, "");
        assert!(t.classes.contains(DestroyClass::Unverified));
        assert!(!t.classes.contains(DestroyClass::Disposable));
    }

    #[test]
    fn unmerged_is_unlanded() {
        let t = classify(
            false,
            false,
            0,
            Some(false),
            Some(false),
            Some(false),
            "refs/heads/main",
        );
        assert!(t.classes.contains(DestroyClass::Unmerged));
        assert!(t.classes.has_unlanded());
        assert!(!t.classes.contains(DestroyClass::Disposable));
    }
}
