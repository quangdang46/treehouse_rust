//! Pool state: the on-disk `treehouse-state.json` wire format.
//!
//! This is the single most load-bearing file for Go compatibility. The Go
//! baseline (`internal/pool/state.go`) defines the exact wire format this
//! module must reproduce: snake_case field names, `omitempty`/`omitzero`
//! omission rules, and RFC3339Nano timestamps. Field names and *presence* are
//! a contract; byte-level encoding beyond that is verified against Go golden
//! output.
//!
//! Reading rules:
//! - A **missing** state file is a fresh, empty pool — unless worktree
//!   directories already exist under the pool directory, which are recovered
//!   as leased (see [`recover_missing_state_entries`]).
//! - A file that **exists but fails to parse** (empty or truncated) is
//!   *corrupt*, not missing — it triggers conservative recovery that marks
//!   every on-disk worktree as leased until a human verifies it.
//!
//! Both recovery paths resolve a worktree through the single
//! [`recover_one_worktree`] helper, so the same on-disk condition can never be
//! classified one way by one path and the other way by the other.

use std::path::Path;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use rand::Rng;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The zero sentinel for timestamps. Go's zero `time.Time` is year-1
/// `0001-01-01T00:00:00Z`; Rust uses the Unix epoch. Both serialize as
/// "absent" via `skip_serializing_if` and deserialize to the same sentinel.
pub const ZERO_TIME: DateTime<Utc> = DateTime::<Utc>::UNIX_EPOCH;

fn is_false(v: &bool) -> bool {
    !*v
}
fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_empty_str(s: &str) -> bool {
    s.is_empty()
}
fn is_zero_utc(t: &DateTime<Utc>) -> bool {
    *t == ZERO_TIME
}

/// RFC3339Nano serde shim matching Go's `time.Time` marshaling.
///
/// Go writes RFC3339Nano: `Z` for UTC, no fractional seconds when the value
/// has none, full nanosecond precision otherwise. It also accepts its own
/// year-1 zero time (`0001-01-01T00:00:00Z`) on read, which we map to the
/// [`ZERO_TIME`] sentinel so omission rules stay consistent.
pub mod rfc3339_nano {
    use super::*;

    pub fn serialize<S>(dt: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let fmt = if dt.timestamp_subsec_nanos() == 0 {
            SecondsFormat::Secs
        } else {
            SecondsFormat::Nanos
        };
        serializer.serialize_str(&dt.to_rfc3339_opts(fmt, true))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let dt = chrono::DateTime::parse_from_rfc3339(&s)
            .map_err(serde::de::Error::custom)?
            .with_timezone(&Utc);
        // Go's zero time ("0001-01-01T00:00:00Z") maps to our zero sentinel.
        Ok(if is_go_zero_time(dt) { ZERO_TIME } else { dt })
    }

    /// Whether `dt` is Go's zero `time.Time` (year 1, Jan 1, 00:00:00 UTC).
    fn is_go_zero_time(dt: DateTime<Utc>) -> bool {
        dt.time() == chrono::NaiveTime::MIN
            && dt.date_naive() == NaiveDate::from_ymd_opt(1, 1, 1).expect("year 1 is valid")
    }
}

/// A single managed worktree in a pool.
///
/// Field names, types, and omission rules mirror Go's `WorktreeEntry` exactly
/// (snake_case JSON, `omitempty`/`omitzero` semantics). Fields decode to their
/// zero value when absent, so pre-lease state files (no lease keys) load
/// correctly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub struct WorktreeEntry {
    pub name: String,
    pub path: String,
    #[serde(with = "rfc3339_nano")]
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub destroying: bool,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub owner_pid: i32,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub owner_started_at: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub leased: bool,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub lease_id: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub lease_holder: String,
    #[serde(default, skip_serializing_if = "is_zero_utc", with = "rfc3339_nano")]
    pub leased_at: DateTime<Utc>,
    /// P1 additive: lease expiry. Zero = permanent lease (pre-P1 state files
    /// load identically). Only serialized when nonzero.
    #[serde(default, skip_serializing_if = "is_zero_utc", with = "rfc3339_nano")]
    pub expires_at: DateTime<Utc>,
    /// Why this entry's VCS marker could not be resolved during a state
    /// recovery scan. Mirrors Go's `RecoveryError`: empty for a worktree whose
    /// marker read back normally, non-empty for one whose marker is present
    /// but unresolvable (dangling or self-referential symlink, a loop, a
    /// permission failure). The entry is otherwise an ordinary recovered
    /// entry — leased, so acquire and prune skip it and destroy removes it
    /// only via an explicit single-target `--include-leased` — but the reason
    /// makes a damaged slot visible instead of reading as an ordinary lease.
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub recovery_error: String,
    /// Fields this build does not model as struct fields, preserved verbatim.
    ///
    /// Go's entry carries acquisition metadata that has no dedicated field
    /// here (`base_branch`, `seeded_paths`, `seed_inventory_*`, `seed_backend`,
    /// `seed_auth_identity`, `recovery_reason`). Every state write is a
    /// read-modify-write of the whole file, so without this map a single
    /// read/write cycle silently deletes all of it — metadata loss on a wire
    /// format the module is contractually required to reproduce. Keeping
    /// unknown keys means a pool written by a newer treehouse can still be
    /// listed, returned and destroyed by this build without damage.
    ///
    /// Populated on read, emitted on write. Unmodelled keys are never
    /// interpreted. The seed-inventory and base-branch keys ARE interpreted,
    /// but only through the typed accessors above
    /// ([`Self::seed_inventory`], [`Self::set_seed_inventory`],
    /// [`Self::base_branch`], [`Self::set_base_branch`]), never by poking at
    /// this map from a caller: the accessors own Go's key names and `omitempty`
    /// shape, and a caller that hand-rolls either re-introduces the
    /// silent-deletion bug this map exists to prevent.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Go's five seed-inventory fields, read and written as one value.
///
/// Upstream splits `SeededPaths`, `SeedInventoryKnown`, `SeedInventoryDigest`,
/// `SeedBackend` and `SeedAuthIdentity` across a `WorktreeEntry` (state.go:52-58).
/// They are split on the wire because that is what JSON is, but they are a
/// single fact — "these ignored files exist because this pool copied them" — and
/// every one of them is only meaningful alongside `SeedInventoryKnown`. Reading
/// them independently is how a caller ends up treating "nobody recorded what was
/// copied" as the claim "nothing was copied", which authorizes deletions nobody
/// approved.
///
/// [`Self::authorized_paths`] is the one method callers should reach for: it is
/// the only thing standing between a state file and an arbitrary path deletion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedInventory {
    /// The paths this pool copied into the worktree.
    pub paths: Vec<String>,
    /// Whether `paths` is a verified record rather than lost bookkeeping.
    pub known: bool,
    /// The backend that performed the seeding (`git`, `jj`).
    pub backend: String,
    /// Go's seed-authentication identity. Empty unless a backend records one;
    /// this port has no jj seed authentication, so it stays empty for git.
    pub auth_identity: String,
}

impl SeedInventory {
    /// The paths a reset is authorized to delete, or `None` when nothing is.
    ///
    /// `None` covers both "no inventory was ever recorded" and "the recorded
    /// inventory is empty". Neither authorizes a deletion:
    /// [`crate::vcs::validate_seed_inventory`] refuses an empty list outright,
    /// and a missing record is a caller that lost its bookkeeping. Turning that
    /// loss into deletion would make a bookkeeping bug look like a feature.
    pub fn authorized_paths(&self) -> Option<&[String]> {
        if self.known && !self.paths.is_empty() {
            Some(&self.paths)
        } else {
            None
        }
    }
}

/// Wire keys for the seed-inventory fields (Go `WorktreeEntry` JSON tags).
const K_SEEDED_PATHS: &str = "seeded_paths";
const K_SEED_INVENTORY_KNOWN: &str = "seed_inventory_known";
const K_SEED_BACKEND: &str = "seed_backend";
const K_SEED_AUTH_IDENTITY: &str = "seed_auth_identity";
/// Go's `base_branch` (state.go:48).
const K_BASE_BRANCH: &str = "base_branch";

impl WorktreeEntry {
    /// A lease that has passed its TTL. Permanent leases (zero `expires_at`)
    /// are never stale.
    pub fn is_stale_lease(&self, now: DateTime<Utc>) -> bool {
        self.leased && self.expires_at != ZERO_TIME && now >= self.expires_at
    }

    /// A lease that is either permanent or not yet expired.
    pub fn is_valid_lease(&self, now: DateTime<Utc>) -> bool {
        self.leased && (self.expires_at == ZERO_TIME || now < self.expires_at)
    }

    /// The seed inventory recorded for this slot (Go's five seed fields).
    ///
    /// Total by construction: every key that is absent, `null`, or of the wrong
    /// JSON type reads as its zero value rather than failing the whole state
    /// read. A state file written by another build is data to be preserved, not
    /// a parse error — and one malformed field must not make an entire pool
    /// unreadable, which would turn a recoverable metadata problem into an
    /// acquisition failure.
    pub fn seed_inventory(&self) -> SeedInventory {
        SeedInventory {
            paths: string_list(self.extra.get(K_SEEDED_PATHS)),
            known: self
                .extra
                .get(K_SEED_INVENTORY_KNOWN)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            backend: string_field(self.extra.get(K_SEED_BACKEND)),
            auth_identity: string_field(self.extra.get(K_SEED_AUTH_IDENTITY)),
        }
    }

    /// Records `inventory` as this slot's trusted seed inventory.
    ///
    /// Written through [`Self::extra`] under Go's own JSON keys, and written
    /// with Go's `omitempty` shape: a field at its zero value is REMOVED rather
    /// than written as an empty value. Removing (not overwriting) is what stops
    /// a cleared inventory from leaving a stale list behind for the next reader
    /// — the difference between "seed nothing" and "seed whatever that was".
    ///
    /// Storage is the flatten map rather than a dedicated struct field because
    /// that map is the module's load-bearing wire contract: it is what keeps
    /// every field a newer treehouse wrote from being silently deleted by the
    /// read-modify-write this type performs on each acquisition. Adding a
    /// derived serde field named `seeded_paths` would move that key out of
    /// `extra` and into a struct field — which reads as typed access but
    /// silently narrows what round-trips.
    pub fn set_seed_inventory(&mut self, inventory: SeedInventory) {
        if inventory.paths.is_empty() {
            self.extra.remove(K_SEEDED_PATHS);
        } else {
            self.extra.insert(
                K_SEEDED_PATHS.to_string(),
                serde_json::to_value(&inventory.paths).unwrap_or(serde_json::Value::Null),
            );
        }
        set_omitempty(
            &mut self.extra,
            K_SEED_INVENTORY_KNOWN,
            inventory.known.then_some(serde_json::Value::Bool(true)),
        );
        set_omitempty(
            &mut self.extra,
            K_SEED_BACKEND,
            nonempty_value(&inventory.backend),
        );
        set_omitempty(
            &mut self.extra,
            K_SEED_AUTH_IDENTITY,
            nonempty_value(&inventory.auth_identity),
        );
    }

    /// Drops this slot's seed inventory, leaving an empty, *known* one.
    ///
    /// Mirrors Go's `setSeedInventory(wt, nil, true)`: a VERIFIED empty
    /// inventory, not a lost one. The distinction is the difference between
    /// "this slot has nothing in it that we put there" and "we no longer know",
    /// and only the first is a claim cleanup may act on.
    pub fn clear_seed_inventory(&mut self) {
        self.set_seed_inventory(SeedInventory {
            known: true,
            ..Default::default()
        });
    }

    /// The EXPLICIT base this slot was last cut from (Go `BaseBranch`).
    ///
    /// `None` when absent or non-string — distinct from `Some("")`, which Go
    /// uses for "cut from the inferred default" and which therefore means
    /// something a later acquisition may weigh.
    pub fn base_branch(&self) -> Option<&str> {
        self.extra
            .get(K_BASE_BRANCH)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    }

    /// Records the base this slot was cut from (Go `BaseBranch`).
    pub fn set_base_branch(&mut self, branch: &str) {
        set_omitempty(
            &mut self.extra,
            K_BASE_BRANCH,
            nonempty_value(branch),
        );
    }
}

/// Writes `value` under `key`, or removes `key` when there is no value.
///
/// Go's `omitempty` in one place, so "clear this field" and "this field has no
/// value" cannot drift into two different wire shapes.
fn set_omitempty(
    extra: &mut std::collections::BTreeMap<String, serde_json::Value>,
    key: &str,
    value: Option<serde_json::Value>,
) {
    match value {
        Some(v) => {
            extra.insert(key.to_string(), v);
        }
        None => {
            extra.remove(key);
        }
    }
}

fn nonempty_value(s: &str) -> Option<serde_json::Value> {
    if s.is_empty() {
        None
    } else {
        Some(serde_json::Value::String(s.to_string()))
    }
}

/// Reads a JSON string field, treating absent, `null` and non-string as empty.
fn string_field(v: Option<&serde_json::Value>) -> String {
    v.and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Reads a JSON string list, keeping only the entries that are strings.
///
/// A list with a non-string entry is not a claim about paths, so it degrades to
/// "nothing was seeded" rather than to a partially-trusted list that a reset
/// would act on.
fn string_list(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The pool state file: an ordered list of managed worktrees.
///
/// `worktrees` is always present in the wire format (Go has no `omitempty`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct State {
    #[serde(default)]
    pub worktrees: Vec<WorktreeEntry>,
    /// Top-level fields this build does not understand, preserved verbatim.
    ///
    /// The same read-modify-write argument as [`WorktreeEntry::extra`], one
    /// level up. Go's `State` carries `version` (state.go:82, currently 4) and
    /// a Go v3 pool also carries a seed-auth key reference this port does not
    /// model. Because *every* state write rewrites the whole file from a
    /// parsed `State`, a top-level flatten map is the only thing standing
    /// between reading a pool written by another build and silently deleting
    /// its `version` on the first `get`, `return`, or `prune`.
    ///
    /// Not modelled, only carried: reading a `version` this build does not
    /// understand is Go's quarantine decision (state.go:135-136), and quietly
    /// acting on an unknown schema would be worse than preserving it.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

impl State {
    /// Returns the state file path for a pool directory.
    pub fn state_file_path(pool_dir: &Path) -> std::path::PathBuf {
        pool_dir.join("treehouse-state.json")
    }

    /// Returns the lock file path for a pool directory.
    pub fn lock_file_path(pool_dir: &Path) -> std::path::PathBuf {
        pool_dir.join("treehouse-state.lock")
    }

    /// Loads pool state.
    ///
    /// A file that exists but fails to parse (empty or truncated) is corrupt:
    /// it is conservatively recovered by scanning the pool directory for
    /// worktrees still on disk and marking each `leased` (see
    /// [`recover_corrupt_state`]).
    ///
    /// A missing file is a fresh, empty pool — unless worktree directories
    /// already exist, in which case [`recover_missing_state_entries`] adopts
    /// them as leased. That scan also runs after a *successful* parse: the
    /// window where `git worktree add` committed but the state write did not
    /// leaves a real worktree absent from an otherwise-valid file, and
    /// `next_name` would otherwise reissue its slot number.
    ///
    /// If the pool directory itself cannot be enumerated the call fails closed
    /// rather than returning partial state. A single unreadable marker *inside*
    /// a worktree is a per-slot problem and is recovered, not fatal.
    pub fn read_state(pool_dir: &Path) -> Result<State, StateError> {
        let path = Self::state_file_path(pool_dir);
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return recover_missing_state_entries(pool_dir, State::default(), &path);
            }
            Err(e) => return Err(StateError::Read(path, e)),
        };
        match serde_json::from_slice(&data) {
            Ok(s) => recover_missing_state_entries(pool_dir, s, &path),
            Err(parse_err) => recover_corrupt_state(pool_dir, path, parse_err),
        }
    }

    /// Reads pool state using the injected environment.
    ///
    /// The state *file* is read through `env`, but both recovery scans walk the
    /// real filesystem: a recovery is about what is on disk, so an injected
    /// environment that keeps state in memory simply has nothing to adopt and
    /// the scan is a no-op.
    pub fn read_state_with_env(
        pool_dir: &Path,
        env: &dyn crate::env::TreehouseEnv,
    ) -> Result<State, StateError> {
        let path = Self::state_file_path(pool_dir);
        let data = match env.read_bytes(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return recover_missing_state_entries(pool_dir, State::default(), &path);
            }
            Err(e) => return Err(StateError::Read(path, e)),
        };
        match serde_json::from_slice(&data) {
            Ok(s) => recover_missing_state_entries(pool_dir, s, &path),
            Err(parse_err) => recover_corrupt_state(pool_dir, path, parse_err),
        }
    }
}

/// Marker placed on every entry either recovery scan reconstructs —
/// [`recover_corrupt_state`] and [`recover_missing_state_entries`] alike — so
/// callers (status output, destroy) can explain why they are unexpectedly
/// leased. The wording is kept byte-identical to Go's `RecoveredLeaseHolder`,
/// which also does not distinguish the two recovery paths. Mirrors Go's
/// `recoveredLeaseHolder`.
pub const RECOVERED_LEASE_HOLDER: &str =
    "recovered: state file was corrupt or truncated; verify before reuse";

/// Rebuilds a `State` from the worktree directories that exist under
/// `pool_dir` when the on-disk state file could not be parsed.
///
/// The original reservation state is gone, so disk evidence alone cannot tell
/// an idle spare from a live, process-independent lease. Every recovered entry
/// is therefore marked `leased`: acquire and prune skip it, and destroy only
/// removes it via an explicit, single-target `--include-leased`. This mirrors
/// Go's `recoverCorruptState`, including the loud stderr warning.
fn recover_corrupt_state(
    pool_dir: &Path,
    state_path: std::path::PathBuf,
    parse_err: serde_json::Error,
) -> Result<State, StateError> {
    let mut recovered = Vec::new();
    for (slot_name, wt_path) in scan_worktree_dirs(pool_dir, &state_path)? {
        if let Some(wt) = recover_one_worktree(&slot_name, &wt_path) {
            recovered.push(wt);
        }
    }

    eprintln!(
        "treehouse: WARNING: state file {} is corrupt or truncated ({parse_err}); recovering from worktrees found on disk. They are marked leased until verified - see `treehouse status`, then `treehouse return` or `treehouse destroy --include-leased`.",
        state_path.display()
    );
    Ok(State {
        worktrees: recovered,
        ..Default::default()
    })
}

/// Adopts worktree directories that exist on disk but are absent from an
/// otherwise-usable state file (Go `recoverMissingStateEntries`).
///
/// This covers the narrow window where `git worktree add` committed but
/// persisting its entry did not. Left unhandled, such a worktree is invisible
/// to the pool: `status` and `gc` under-report it, `destroy` cannot name it,
/// and `next_name` — which computes `max(name) + 1` over the state list — hands
/// the same slot number back and tries to create a worktree where one already
/// exists. That last failure is loud rather than destructive (git refuses to
/// add into an occupied directory), but it never resolves on its own, so the
/// pool stays wedged until the directory is removed by hand.
///
/// Adopted entries are quarantined exactly like a corrupt-state recovery, so
/// the same "leased until a human looks at it" contract holds on this path
/// too. Entries already named by the state file are left untouched: this
/// function only ever adds.
fn recover_missing_state_entries(
    pool_dir: &Path,
    mut state: State,
    state_path: &Path,
) -> Result<State, StateError> {
    // Go stats the pool directory before scanning: a pool that does not exist
    // has no worktrees to adopt and is a genuinely fresh, empty pool. A
    // directory we cannot even *stat* is a different case and stays an error —
    // we could not tell "nothing there" from "cannot see".
    match std::fs::metadata(pool_dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(StateError::RecoverScan(
                state_path.to_path_buf(),
                std::io::Error::other(format!("{} is not a directory", pool_dir.display())),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(state),
        Err(e) => return Err(StateError::RecoverScan(state_path.to_path_buf(), e)),
    }

    // Compare on cleaned paths so `/pool/1/repo`, `/pool/./1/repo` and
    // `/pool/1/repo/` are recognized as the one worktree they are, and a
    // normal read-modify-write does not append a duplicate entry for it.
    let known: std::collections::HashSet<std::path::PathBuf> = state
        .worktrees
        .iter()
        .map(|wt| clean_path(Path::new(&wt.path)))
        .collect();

    let mut adopted = 0usize;
    for (slot_name, wt_path) in scan_worktree_dirs(pool_dir, state_path)? {
        if known.contains(&clean_path(&wt_path)) {
            continue;
        }
        if let Some(wt) = recover_one_worktree(&slot_name, &wt_path) {
            state.worktrees.push(wt);
            adopted += 1;
        }
    }

    // Go stays quiet here because its recovery entry is self-describing. We
    // say so anyway: entries appearing in a pool whose state file does not
    // exist is not a state a user can explain from the outside.
    if adopted > 0 {
        eprintln!(
            "treehouse: WARNING: no usable state file at {}; adopted {adopted} worktree(s) found on disk as leased until verified - see `treehouse status`.",
            state_path.display()
        );
    }
    Ok(state)
}

/// Lists every `<pool_dir>/<slot>/<worktree>` directory in the pool layout.
///
/// A `read_dir` failure stays the caller's error: it means the pool — or one
/// slot — cannot be enumerated at all, so we do not know which healthy
/// worktrees exist and must not hand back a partial pool. This is the line
/// `docs/rust-port-plan.md` §2.3 draws with "fail closed if the scan can't
/// complete", and it is read as being about the *scan*, not about every
/// per-slot condition it walks over: an unreadable VCS marker inside one
/// worktree hides nothing about the others, so it is handled per-slot by
/// [`recover_one_worktree`] rather than bricking the whole pool.
fn scan_worktree_dirs(
    pool_dir: &Path,
    state_path: &Path,
) -> Result<Vec<(String, std::path::PathBuf)>, StateError> {
    let err = |e| StateError::RecoverScan(state_path.to_path_buf(), e);
    let mut out = Vec::new();

    let slots = std::fs::read_dir(pool_dir).map_err(err)?;
    for slot in slots {
        let slot = slot.map_err(err)?;
        if !slot.file_type().map_err(err)?.is_dir() {
            continue;
        }
        let slot_os = slot.file_name();
        let slot_dir = pool_dir.join(&slot_os);
        let nested = std::fs::read_dir(&slot_dir).map_err(err)?;
        for entry in nested {
            let entry = entry.map_err(err)?;
            if !entry.file_type().map_err(err)?.is_dir() {
                continue;
            }
            out.push((
                slot_os.to_string_lossy().into_owned(),
                slot_dir.join(entry.file_name()),
            ));
        }
    }
    Ok(out)
}

/// Resolves one on-disk worktree directory into a quarantine entry, or reports
/// that the directory is not a pooled worktree at all (Go `recoverOneWorktree`).
///
/// This is the single place both recovery scans decide what an on-disk
/// condition means, so the missing-state and corrupt-state paths can never
/// classify the same slot differently.
///
/// Three outcomes:
/// - no VCS marker: not a pooled worktree, skip it (`None`).
/// - marker readable: recovered as a leased, quarantined entry.
/// - marker present but unreadable (dangling or self-referential symlink, a
///   loop, a permission failure): recovered too, as a leased entry carrying
///   the read error in [`WorktreeEntry::recovery_error`]. It stays unusable
///   exactly like every other recovered entry, but it is *visible* — acquire
///   and prune skip it, `status` reports it as damaged with the reason, and a
///   warning is printed so the failure is not silent. Failing the whole scan
///   here instead would let one stray symlink inside one slot brick every
///   command on the pool.
fn recover_one_worktree(slot_name: &str, wt_path: &Path) -> Option<WorktreeEntry> {
    match marker_backend(wt_path) {
        Ok(true) => Some(quarantine_entry(slot_name, wt_path, "")),
        Ok(false) => None,
        Err(e) => {
            eprintln!(
                "treehouse: WARNING: cannot read the VCS marker for {} ({e}); recovered as leased and reported damaged - see `treehouse status`",
                wt_path.display()
            );
            Some(quarantine_entry(slot_name, wt_path, &e.to_string()))
        }
    }
}

/// The conservative entry both recovery scans write for a worktree whose
/// reservation state was lost (Go `quarantineEntry`). It is leased under
/// [`RECOVERED_LEASE_HOLDER`], so acquire and prune skip it and destroy removes
/// it only via an explicit single-target `--include-leased`.
///
/// `recovery_error` is empty for a worktree whose marker resolved normally and
/// carries the read error for one whose marker exists but could not be read.
/// There is deliberately no `expires_at`: a recovered entry is never
/// automatically reclaimable, because recovery has no trustworthy inventory of
/// what was in the worktree.
fn quarantine_entry(slot_name: &str, wt_path: &Path, recovery_error: &str) -> WorktreeEntry {
    let now = chrono::Utc::now();
    WorktreeEntry {
        name: slot_name.to_string(),
        path: wt_path.to_string_lossy().into_owned(),
        created_at: now,
        leased: true,
        lease_holder: RECOVERED_LEASE_HOLDER.to_string(),
        leased_at: now,
        recovery_error: recovery_error.to_string(),
        ..WorktreeEntry::default()
    }
}

/// Reports whether `<wt_path>/.git` marks a pooled worktree, or fails with the
/// reason it could not be resolved (Go `WorktreeBackendNameChecked`).
///
/// The two-step lstat-then-stat is what makes a broken marker reportable.
/// `symlink_metadata` does not follow the link, so a dangling or
/// self-referential `.git` symlink — the exact shape a crashed `worktree add`
/// or a hand-edited pool leaves behind — is correctly *present*; the following
/// `metadata` then fails and the caller quarantines the slot as damaged. A
/// plain `stat` would report `ENOENT` and silently skip the worktree, hiding a
/// slot that is still occupying its name. Only a genuinely absent entry is
/// `Ok(false)`.
fn marker_backend(wt_path: &Path) -> std::io::Result<bool> {
    let git = wt_path.join(".git");
    match std::fs::symlink_metadata(&git) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
        Ok(_) => {
            std::fs::metadata(&git).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("resolving .git marker in {}: {e}", wt_path.display()),
                )
            })?;
            Ok(true)
        }
    }
}

/// Lexical path cleanup, the Rust equivalent of Go's `filepath.Clean`, for the
/// single job recovery needs it for: deciding whether two spellings of a path
/// name the same worktree. `Path::components` already drops `.` and collapses
/// repeated separators, so only `..` is left to fold. Purely lexical — it never
/// touches the filesystem and so never resolves symlinks, matching Go.
fn clean_path(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        std::path::PathBuf::from(".")
    } else {
        out
    }
}

/// Self-heals pool state in place (Go `healState`).
///
/// - Clears dead owner reservations: when `owner_pid != 0` but the owner
///   process no longer matches `owner_started_at`, zero the owner fields and
///   `destroying`.
/// - Drops entries whose path no longer exists on disk.
/// - **Never touches any lease field** (valid or stale) — leases are
///   process-independent and only `return` / an explicit destroy clears them.
///
/// `process_started_at` resolves a pid to its epoch-**millis** start time (or
/// `None` if the process no longer exists / can't be determined). It is a
/// parameter so `state.rs` stays decoupled from the process module; the pool
/// wires it to the real process table.
pub fn heal_state(state: &mut State, process_started_at: impl Fn(i32) -> Option<i64>) {
    let mut healed = Vec::with_capacity(state.worktrees.len());
    for mut wt in std::mem::take(&mut state.worktrees) {
        if std::path::Path::new(&wt.path).exists() {
            if wt.owner_pid != 0 && !owner_alive(&wt, &process_started_at) {
                wt.owner_pid = 0;
                wt.owner_started_at = 0;
                wt.destroying = false;
            }
            healed.push(wt);
        }
        // Path gone => entry dropped entirely (mirrors Go healState).
    }
    state.worktrees = healed;
}

/// Whether a worktree's owner reservation is held by a live, matching process
/// (Go `ownerAlive`). Requires BOTH a nonzero pid and start time, and that the
/// process's actual start time equals the recorded one (PID-reuse safe).
pub fn owner_alive(wt: &WorktreeEntry, process_started_at: &impl Fn(i32) -> Option<i64>) -> bool {
    if wt.owner_pid == 0 || wt.owner_started_at == 0 {
        return false;
    }
    match process_started_at(wt.owner_pid) {
        Some(started_at) => started_at == wt.owner_started_at,
        None => false,
    }
}

/// Clears any durable lease from a worktree entry (Go `clearLease`).
pub fn clear_lease(wt: &mut WorktreeEntry) {
    wt.leased = false;
    wt.lease_id.clear();
    wt.lease_holder.clear();
    wt.leased_at = ZERO_TIME;
    wt.expires_at = ZERO_TIME;
}

/// Generates a fresh 128-bit random lease identity as 32 lowercase hex chars
/// (Go `newLeaseID`).
pub fn new_lease_id() -> String {
    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..16).map(|_| rng.random()).collect();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------- errors ----------

/// Errors from reading / recovering pool state.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("failed to read state file {0}: {1}")]
    Read(std::path::PathBuf, std::io::Error),
    /// The pool directory could not be enumerated during a recovery scan.
    /// Deliberately fatal: returning the worktrees found so far would be a
    /// partial pool, and a pool missing entries reads as "these slots are
    /// free" to `next_name`.
    #[error("could not scan the pool directory while recovering state for {0}: {1}")]
    RecoverScan(std::path::PathBuf, std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn serializes_go_style_minimal_entry() {
        let s = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: "/pool/1/myrepo".into(),
                created_at: dt("2026-07-20T12:00:00Z"),
                ..WorktreeEntry::default()
            }],
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&s).unwrap();
        // No lease / owner keys present; created_at keeps Go's Z form.
        assert!(!json.contains("owner_pid"), "unexpected owner_pid: {json}");
        assert!(!json.contains("lease_id"), "unexpected lease_id: {json}");
        assert!(
            json.contains(r#""created_at": "2026-07-20T12:00:00Z""#),
            "{json}"
        );
    }

    #[test]
    fn round_trips_full_entry() {
        let s = State {
            worktrees: vec![WorktreeEntry {
                name: "4".into(),
                path: "/pool/4/myrepo".into(),
                created_at: dt("2026-08-14T12:34:56.123456789Z"),
                destroying: true,
                owner_pid: 1234,
                owner_started_at: 1_725_000_000_000,
                leased: true,
                lease_id: "9f2c1e04a7b3d5c8e6f10293a4b5c6d7".into(),
                lease_holder: "automation-A".into(),
                leased_at: dt("2026-08-14T12:34:56.123456789Z"),
                expires_at: dt("2026-08-14T13:04:56.123456789Z"),
                recovery_error: "resolving .git marker: too many levels of symbolic links".into(),
                // A field this build does not model, to prove it survives the
                // write half of the round trip along with every modelled one.
                extra: [("base_branch".to_string(), serde_json::json!("main"))]
                    .into_iter()
                    .collect(),
            }],
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert!(
            json.contains(r#""leased_at": "2026-08-14T12:34:56.123456789Z""#),
            "{json}"
        );
    }

    #[test]
    fn pre_lease_file_loads_unleased() {
        // A state file written before leases existed has no lease keys.
        let json = r#"{
  "worktrees": [{
    "name": "1",
    "path": "legacy-worktree",
    "created_at": "2026-07-20T12:00:00Z",
    "leased": true,
    "lease_holder": "legacy-automation",
    "leased_at": "2026-07-20T12:01:00Z"
  }]
}"#;
        let s: State = serde_json::from_str(json).unwrap();
        let wt = &s.worktrees[0];
        assert!(wt.leased);
        assert_eq!(wt.lease_id, ""); // pre-identity lease has no ID
        assert_eq!(wt.lease_holder, "legacy-automation");
        assert_eq!(wt.leased_at, dt("2026-07-20T12:01:00Z"));
    }

    #[test]
    fn missing_keys_decode_to_zero() {
        let json =
            r#"{"worktrees":[{"name":"1","path":"/p","created_at":"2026-07-20T12:00:00Z"}]}"#;
        let s: State = serde_json::from_str(json).unwrap();
        let wt = &s.worktrees[0];
        assert!(!wt.destroying);
        assert_eq!(wt.owner_pid, 0);
        assert_eq!(wt.owner_started_at, 0);
        assert!(!wt.leased);
        assert_eq!(wt.lease_id, "");
        assert_eq!(wt.leased_at, ZERO_TIME);
        assert_eq!(wt.expires_at, ZERO_TIME);
    }

    #[test]
    fn go_zero_time_deserializes_to_sentinel() {
        // Go's zero time is year-1; map to ZERO_TIME so omitzero behaves.
        let json = r#"{"worktrees":[{"name":"1","path":"/p","created_at":"2026-07-20T12:00:00Z","leased_at":"0001-01-01T00:00:00Z"}]}"#;
        let s: State = serde_json::from_str(json).unwrap();
        assert_eq!(s.worktrees[0].leased_at, ZERO_TIME);
    }

    #[test]
    fn shim_directly_maps_go_zero_time() {
        // Call the shim's deserialize directly on the raw Go zero-time string.
        let mut de = serde_json::Deserializer::from_str("\"0001-01-01T00:00:00Z\"");
        let r: DateTime<Utc> = rfc3339_nano::deserialize(&mut de).unwrap();
        assert_eq!(
            r, ZERO_TIME,
            "shim should map Go zero time to sentinel, got {r}"
        );
    }

    #[test]
    fn stale_lease_boundary() {
        let base = dt("2026-08-14T12:00:00Z");
        let mut wt = WorktreeEntry {
            leased: true,
            expires_at: dt("2026-08-14T12:30:00Z"),
            ..WorktreeEntry::default()
        };
        assert!(wt.is_valid_lease(base));
        assert!(!wt.is_stale_lease(base));
        // Exactly at expiry: stale (now >= expires_at).
        assert!(wt.is_stale_lease(dt("2026-08-14T12:30:00Z")));
        assert!(!wt.is_valid_lease(dt("2026-08-14T12:31:00Z")));
        // Permanent lease: never stale.
        wt.expires_at = ZERO_TIME;
        assert!(wt.is_valid_lease(base));
        assert!(!wt.is_stale_lease(base));
    }

    #[test]
    fn heal_clears_dead_owner_and_drops_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let live_path = dir.path().join("1/live").to_string_lossy().into_owned();
        std::fs::create_dir_all(std::path::Path::new(&live_path)).unwrap();
        let dead_path = dir.path().join("2/dead").to_string_lossy().into_owned();
        std::fs::create_dir_all(std::path::Path::new(&dead_path)).unwrap();
        let gone_path = dir.path().join("3/gone").to_string_lossy().into_owned();

        let mut state = State {
            worktrees: vec![
                // Live owner (matches process) => untouched.
                WorktreeEntry {
                    name: "1".into(),
                    path: live_path.clone(),
                    owner_pid: 100,
                    owner_started_at: 111,
                    ..WorktreeEntry::default()
                },
                // Dead owner (process gone) => owner cleared, destroying reset.
                WorktreeEntry {
                    name: "2".into(),
                    path: dead_path.clone(),
                    destroying: true,
                    owner_pid: 999,
                    owner_started_at: 222,
                    ..WorktreeEntry::default()
                },
                // Path gone => dropped entirely.
                WorktreeEntry {
                    name: "3".into(),
                    path: gone_path,
                    ..WorktreeEntry::default()
                },
            ],
            ..Default::default()
        };

        let resolver = |pid: i32| -> Option<i64> { if pid == 100 { Some(111) } else { None } };
        heal_state(&mut state, resolver);

        assert_eq!(state.worktrees.len(), 2, "path-gone entry must be dropped");
        let one = state.worktrees.iter().find(|w| w.name == "1").unwrap();
        assert_eq!(one.owner_pid, 100);
        let two = state.worktrees.iter().find(|w| w.name == "2").unwrap();
        assert_eq!(two.owner_pid, 0);
        assert_eq!(two.owner_started_at, 0);
        assert!(!two.destroying);
    }

    #[test]
    fn heal_never_touches_leases() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("1/live").to_string_lossy().into_owned();
        std::fs::create_dir_all(std::path::Path::new(&p)).unwrap();
        let mut state = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: p,
                leased: true,
                lease_id: "abc".into(),
                lease_holder: "holder".into(),
                leased_at: dt("2026-08-14T12:00:00Z"),
                expires_at: dt("2026-08-14T12:30:00Z"),
                ..WorktreeEntry::default()
            }],
            ..Default::default()
        };
        heal_state(&mut state, |_| None);
        let wt = &state.worktrees[0];
        assert!(wt.leased, "heal must not clear a lease");
        assert_eq!(wt.lease_id, "abc");
        assert_eq!(wt.lease_holder, "holder");
        assert_eq!(wt.leased_at, dt("2026-08-14T12:00:00Z"));
        assert_eq!(wt.expires_at, dt("2026-08-14T12:30:00Z"));
    }

    #[test]
    fn clear_lease_resets_everything() {
        let mut wt = WorktreeEntry {
            leased: true,
            lease_id: "abc".into(),
            lease_holder: "h".into(),
            leased_at: dt("2026-08-14T12:00:00Z"),
            expires_at: dt("2026-08-14T13:00:00Z"),
            ..WorktreeEntry::default()
        };
        clear_lease(&mut wt);
        assert!(!wt.leased);
        assert_eq!(wt.lease_id, "");
        assert_eq!(wt.lease_holder, "");
        assert_eq!(wt.leased_at, ZERO_TIME);
        assert_eq!(wt.expires_at, ZERO_TIME);
    }

    #[test]
    fn new_lease_id_is_32_lowercase_hex() {
        let a = new_lease_id();
        let b = new_lease_id();
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lease id must be lowercase hex, got {a}"
        );
    }

    #[test]
    fn read_state_missing_is_empty_pool() {
        let dir = tempfile::tempdir().unwrap();
        let s = State::read_state(dir.path()).unwrap();
        assert!(s.worktrees.is_empty());
    }

    #[test]
    fn read_state_empty_file_is_corrupt_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        // Fake worktree dirs on disk.
        for slot in ["1", "2"] {
            let wt = dir.path().join(slot).join("myrepo");
            std::fs::create_dir_all(&wt).unwrap();
            std::fs::write(wt.join(".git"), "gitdir: ../../fake.git\n").unwrap();
        }
        // 0-byte state file => CORRUPT (not missing => not fresh).
        std::fs::write(State::state_file_path(dir.path()), b"").unwrap();
        let s = State::read_state(dir.path()).unwrap();
        assert_eq!(s.worktrees.len(), 2, "recovered 2 worktrees");
        for wt in &s.worktrees {
            assert!(wt.leased, "recovered entry must be leased");
            assert_eq!(wt.lease_holder, RECOVERED_LEASE_HOLDER);
        }
    }

    #[test]
    fn recovered_entries_are_permanent_leases_without_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("1/myrepo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../../fake.git\n").unwrap();
        std::fs::write(State::state_file_path(dir.path()), b"").unwrap();

        let s = State::read_state(dir.path()).unwrap();
        let e = &s.worktrees[0];
        // Deliberately conservative: no lease_id (so --if-lease-id can't match),
        // no expires_at (so gc can never auto-reclaim a recovered entry).
        assert!(e.leased);
        assert_eq!(e.lease_holder, RECOVERED_LEASE_HOLDER);
        assert_eq!(e.lease_id, "");
        assert_eq!(e.expires_at, ZERO_TIME);
        assert!(
            !e.is_stale_lease(chrono::Utc::now()),
            "recovered entries must never auto-expire"
        );
    }

    // ─── M-015: adopting worktrees the state file does not know about ───────

    /// Creates `<pool>/<slot>/<repo>` with a readable `.git` marker, the
    /// on-disk shape of a real pooled worktree.
    fn seed_worktree(pool: &Path, slot: &str) -> std::path::PathBuf {
        let wt = pool.join(slot).join("myrepo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../../fake.git\n").unwrap();
        wt
    }

    #[test]
    fn missing_state_file_adopts_on_disk_worktrees_as_leased() {
        let dir = tempfile::tempdir().unwrap();
        seed_worktree(dir.path(), "1");
        // No state file at all: a crash between `worktree add` and the state
        // write, or a deleted state file. Either way the worktree is real and
        // must not stay invisible to the pool.
        let s = State::read_state(dir.path()).unwrap();
        assert_eq!(s.worktrees.len(), 1, "orphan worktree must be adopted");
        let e = &s.worktrees[0];
        assert_eq!(e.name, "1");
        assert!(e.leased, "adopted entry must be quarantined as leased");
        assert_eq!(e.lease_holder, RECOVERED_LEASE_HOLDER);
        assert_eq!(e.recovery_error, "");
        assert!(e.path.ends_with("1/myrepo"), "got {}", e.path);
    }

    #[test]
    fn valid_state_adopts_worktree_missing_from_it() {
        // The successful-parse half: the state file is valid but the crash
        // window left a second worktree unrecorded. Without the scan,
        // `next_name` would compute 2 for a slot that already exists.
        let dir = tempfile::tempdir().unwrap();
        seed_worktree(dir.path(), "1");
        let orphan = seed_worktree(dir.path(), "2");
        let known = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: dir.path().join("1/myrepo").to_string_lossy().into_owned(),
                created_at: dt("2026-08-14T12:00:00Z"),
                ..WorktreeEntry::default()
            }],
            ..Default::default()
        };
        std::fs::write(
            State::state_file_path(dir.path()),
            serde_json::to_vec_pretty(&known).unwrap(),
        )
        .unwrap();

        let s = State::read_state(dir.path()).unwrap();
        assert_eq!(s.worktrees.len(), 2, "orphan must be adopted alongside");
        let adopted = s
            .worktrees
            .iter()
            .find(|w| w.name == "2")
            .expect("slot 2 adopted");
        assert!(adopted.leased);
        assert_eq!(adopted.lease_holder, RECOVERED_LEASE_HOLDER);
        assert_eq!(adopted.path, orphan.to_string_lossy());
        // The recorded entry is untouched, not re-quarantined.
        let kept = s.worktrees.iter().find(|w| w.name == "1").unwrap();
        assert!(!kept.leased, "an entry already in state must be left alone");
    }

    #[test]
    fn adoption_never_duplicates_an_entry_already_in_state() {
        // The common path: every command re-reads state through this scan, so
        // a worktree the file already names must not be adopted a second time
        // on each read.
        let dir = tempfile::tempdir().unwrap();
        let wt = seed_worktree(dir.path(), "1");
        let state = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: wt.to_string_lossy().into_owned(),
                created_at: dt("2026-08-14T12:00:00Z"),
                ..WorktreeEntry::default()
            }],
            ..Default::default()
        };
        std::fs::write(
            State::state_file_path(dir.path()),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let s = State::read_state(dir.path()).unwrap();
        assert_eq!(s.worktrees.len(), 1, "no duplicate entry");
        assert_eq!(s, state, "adoption must be a no-op here");
    }

    #[test]
    fn adoption_matches_paths_lexically_not_byte_for_byte() {
        // `/pool/./1/myrepo/` and `/pool/1/myrepo` are one worktree. Comparing
        // the strings would append a phantom duplicate on every read, and the
        // pool would grow without bound.
        let dir = tempfile::tempdir().unwrap();
        seed_worktree(dir.path(), "1");
        let spelled = format!("{}/./1/myrepo/", dir.path().to_string_lossy());
        let json = format!(
            r#"{{"worktrees":[{{"name":"1","path":"{spelled}","created_at":"2026-08-14T12:00:00Z"}}]}}"#
        );
        std::fs::write(State::state_file_path(dir.path()), json).unwrap();

        let s = State::read_state(dir.path()).unwrap();
        assert_eq!(
            s.worktrees.len(),
            1,
            "a differently-spelled path is the same slot"
        );
        assert!(
            !s.worktrees[0].leased,
            "a recorded entry is not re-quarantined"
        );
    }

    #[test]
    fn dir_without_a_marker_is_not_adopted() {
        // A slot directory that is not a worktree at all (a leftover mkdir, a
        // half-finished create) must not become a phantom leased entry.
        let dir = tempfile::tempdir().unwrap();
        seed_worktree(dir.path(), "1");
        std::fs::create_dir_all(dir.path().join("7/notaworktree")).unwrap();

        let s = State::read_state(dir.path()).unwrap();
        assert_eq!(s.worktrees.len(), 1);
        assert_eq!(s.worktrees[0].name, "1");
    }

    #[test]
    fn existing_but_empty_pool_dir_with_no_state_file_is_still_empty() {
        // The pool directory exists (the lock file is enough to create it) but
        // holds no worktrees. Adopting nothing here is what keeps a fresh pool
        // from printing recovery noise.
        let dir = tempfile::tempdir().unwrap();
        let s = State::read_state(dir.path()).unwrap();
        assert!(s.worktrees.is_empty());
    }

    #[test]
    fn absent_pool_dir_is_a_fresh_empty_pool() {
        // `read_state` runs on pools that were never created. Nothing to scan
        // is not an error.
        let dir = tempfile::tempdir().unwrap();
        let s = State::read_state(&dir.path().join("never-created")).unwrap();
        assert!(s.worktrees.is_empty());
    }

    // ─── M-022: one broken slot must not abort the whole scan ──────────────

    #[cfg(unix)]
    fn seed_broken_marker_worktree(pool: &Path, slot: &str, target: &str) -> std::path::PathBuf {
        use std::os::unix::fs::symlink;
        let wt = pool.join(slot).join("myrepo");
        std::fs::create_dir_all(&wt).unwrap();
        symlink(target, wt.join(".git")).unwrap();
        wt
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_marker_slot_does_not_abort_the_whole_pool() {
        // The exact reproduction from the audit: one healthy slot plus one
        // self-referential `.git` symlink. Before the fix the whole pool
        // returned RecoverScan and every pool command was bricked.
        let dir = tempfile::tempdir().unwrap();
        seed_worktree(dir.path(), "1");
        seed_broken_marker_worktree(dir.path(), "2", "self");

        let s = State::read_state(dir.path()).unwrap();
        assert_eq!(s.worktrees.len(), 2, "healthy slot must still be recovered");
        let healthy = s.worktrees.iter().find(|w| w.name == "1").unwrap();
        assert!(healthy.leased);
        assert_eq!(healthy.recovery_error, "");
        // The broken slot is quarantined as damaged, not dropped and not fatal.
        let damaged = s.worktrees.iter().find(|w| w.name == "2").unwrap();
        assert!(damaged.leased, "damaged slot stays unusable");
        assert_eq!(damaged.lease_holder, RECOVERED_LEASE_HOLDER);
        assert!(
            !damaged.recovery_error.is_empty(),
            "the reason must be recorded so status can report it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_marker_symlink_is_damaged_not_skipped() {
        // `lstat` says present, `stat` cannot resolve it. Treating ENOENT as
        // "no marker" would silently hide a slot that still owns its number.
        let dir = tempfile::tempdir().unwrap();
        seed_worktree(dir.path(), "1");
        seed_broken_marker_worktree(dir.path(), "2", "../gone/gitdir");

        let s = State::read_state(dir.path()).unwrap();
        assert_eq!(s.worktrees.len(), 2);
        let damaged = s.worktrees.iter().find(|w| w.name == "2").unwrap();
        assert!(damaged.leased);
        assert!(
            !damaged.recovery_error.is_empty(),
            "a dangling marker is a reported fault, not a missing worktree"
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_and_corrupt_state_agree_on_a_broken_slot() {
        // Go pins this with recovery_symmetry_test.go: the two recovery paths
        // must never classify the same on-disk condition differently. They share
        // recover_one_worktree, so they cannot drift — this test keeps it true.
        let missing = tempfile::tempdir().unwrap();
        seed_worktree(missing.path(), "1");
        seed_broken_marker_worktree(missing.path(), "2", "self");
        let by_missing = State::read_state(missing.path()).unwrap();

        let corrupt = tempfile::tempdir().unwrap();
        seed_worktree(corrupt.path(), "1");
        seed_broken_marker_worktree(corrupt.path(), "2", "self");
        std::fs::write(State::state_file_path(corrupt.path()), b"").unwrap();
        let by_corrupt = State::read_state(corrupt.path()).unwrap();

        assert_eq!(by_missing.worktrees.len(), by_corrupt.worktrees.len());
        // The recorded reason embeds the absolute path, which differs between
        // the two tempdirs; compare the parts that must actually agree.
        let strip = |s: &str, dir: &Path| s.replace(&dir.to_string_lossy().to_string(), "<pool>");
        for (a, b) in by_missing.worktrees.iter().zip(&by_corrupt.worktrees) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.leased, b.leased);
            assert_eq!(a.lease_holder, b.lease_holder);
            assert_eq!(
                strip(&a.recovery_error, missing.path()),
                strip(&b.recovery_error, corrupt.path()),
                "the two recovery paths must classify the same slot identically"
            );
        }
    }

    #[test]
    fn unreadable_pool_slot_directory_still_fails_closed() {
        // The other half of the line: a slot directory we cannot *enumerate*
        // means we do not know which healthy worktrees exist, and a pool
        // missing entries reads as "these slots are free" to next_name. That
        // stays an error rather than becoming a partial pool.
        let dir = tempfile::tempdir().unwrap();
        seed_worktree(dir.path(), "1");
        let blocked = dir.path().join("2");
        std::fs::create_dir_all(&blocked).unwrap();
        let mut perms = std::fs::metadata(&blocked).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o000);
        }
        #[cfg(not(unix))]
        perms.set_readonly(true);
        std::fs::set_permissions(&blocked, perms).unwrap();

        let precondition_held = std::fs::read_dir(&blocked).is_err();
        let scanned = State::read_state(dir.path());

        // Restore afterwards so the tempdir can clean itself up.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755));
        }

        if !precondition_held {
            // Running as root, or a filesystem that ignores mode bits: the
            // condition this test needs was never created.
            return;
        }
        match scanned {
            Err(StateError::RecoverScan(_, _)) => {}
            other => panic!("expected a hard RecoverScan error, got {other:?}"),
        }
    }

    // ─── Wire format: unknown Go fields must survive a read-modify-write ────

    // ─── the typed seed-inventory layer ──────────────────────────────────────

    /// A Go-written entry must read back through the typed accessors with the
    /// exact values Go wrote, and write back in Go's shape.
    ///
    /// This is the round trip that matters: a pool written by an upstream build
    /// is opened by this one, seeded-path removal has to act on what Go
    /// recorded, and every write must leave a file the next reader — of either
    /// build — still understands.
    #[test]
    fn a_go_written_seed_inventory_reads_and_writes_back_identically() {
        let json = r#"{
  "worktrees": [{
    "name": "1",
    "path": "/pool/1/repo",
    "created_at": "2026-08-14T12:00:00Z",
    "leased": false,
    "base_branch": "main",
    "seeded_paths": [".env", "vendor/"],
    "seed_inventory_known": true,
    "seed_backend": "git",
    "seed_auth_identity": "auth-abc"
  }]
}"#;
        let s: State = serde_json::from_str(json).unwrap();
        let inventory = s.worktrees[0].seed_inventory();
        assert!(inventory.known);
        assert_eq!(inventory.paths, vec![".env", "vendor/"]);
        assert_eq!(inventory.backend, "git");
        assert_eq!(inventory.auth_identity, "auth-abc");
        assert_eq!(s.worktrees[0].base_branch(), Some("main"));
        assert_eq!(
            inventory.authorized_paths(),
            Some([".env".to_string(), "vendor/".to_string()].as_slice())
        );

        // And back out, with Go's `omitempty` shape preserved exactly.
        let out = serde_json::to_string(&s).unwrap();
        for key in [
            "base_branch",
            "seeded_paths",
            "seed_inventory_known",
            "seed_backend",
            "seed_auth_identity",
        ] {
            assert!(out.contains(&format!("\"{key}\"")), "{key} lost: {out}");
        }
    }

    /// A malformed or absent key must degrade to its zero value, never fail the
    /// whole state read.
    ///
    /// One bad field must not make an entire pool unreadable: that would turn a
    /// recoverable metadata problem into an acquisition failure for every slot
    /// in it, which is a far worse outcome than ignoring the field.
    #[test]
    fn an_absent_or_malformed_seed_key_reads_as_zero_not_as_an_error() {
        let json = r#"{
  "worktrees": [
    {"name":"1","path":"/p/1","created_at":"2026-08-14T12:00:00Z"},
    {"name":"2","path":"/p/2","created_at":"2026-08-14T12:00:00Z",
     "seeded_paths": ["ok.env", 42, null],
     "seed_inventory_known": "yes",
     "seed_backend": 7}
  ]
}"#;
        let s: State = serde_json::from_str(json).expect("a bad field must not fail the read");

        let absent = s.worktrees[0].seed_inventory();
        assert!(!absent.known);
        assert!(absent.paths.is_empty());
        assert!(
            absent.authorized_paths().is_none(),
            "no record is not the same claim as 'nothing was seeded'"
        );

        let malformed = s.worktrees[1].seed_inventory();
        assert!(
            !malformed.known,
            "a non-boolean seed_inventory_known cannot authorize anything"
        );
        assert_eq!(
            malformed.paths,
            vec!["ok.env".to_string()],
            "non-string entries are dropped rather than trusted"
        );
        assert!(malformed.backend.is_empty());
        assert!(
            malformed.authorized_paths().is_none(),
            "an unknown inventory authorizes no deletion however long the list is"
        );
    }

    /// `known == true` with nothing in it is a real, different claim from
    /// `known == false`. Collapsing them is how a cleared inventory turns into a
    /// stale one.
    #[test]
    fn a_known_empty_inventory_differs_from_a_lost_one() {
        let mut cleared = WorktreeEntry::default();
        cleared.clear_seed_inventory();
        assert!(cleared.seed_inventory().known);
        assert!(cleared.seed_inventory().authorized_paths().is_none());

        let never = WorktreeEntry::default();
        assert!(!never.seed_inventory().known);

        // Both authorize no deletion, but only one of them is a statement about
        // the slot rather than an absence of one.
        let cleared_json = serde_json::to_string(&cleared).unwrap();
        assert!(
            cleared_json.contains(r#""seed_inventory_known":true"#),
            "a verified empty inventory must record that it is verified: {cleared_json}"
        );
        assert!(
            !cleared_json.contains("seeded_paths"),
            "an empty list must be omitted, not written as []: {cleared_json}"
        );
    }

    /// Setting then clearing must REMOVE the keys, not overwrite them with
    /// empty values — otherwise a later reader resurrects a stale list.
    #[test]
    fn clearing_removes_the_keys_rather_than_blanking_them() {
        let mut wt = WorktreeEntry::default();
        wt.set_seed_inventory(SeedInventory {
            paths: vec![".env".into()],
            known: true,
            backend: "git".into(),
            auth_identity: String::new(),
        });
        wt.set_base_branch("main");
        assert!(wt.seed_inventory().authorized_paths().is_some());

        wt.clear_seed_inventory();
        wt.set_base_branch("");
        assert!(wt.base_branch().is_none());
        let json = serde_json::to_string(&wt).unwrap();
        assert!(!json.contains("seeded_paths"), "{json}");
        assert!(!json.contains("seed_backend"), "{json}");
        assert!(!json.contains("base_branch"), "{json}");
    }

    #[test]
    fn unknown_go_fields_round_trip() {
        // Every state write is a read-modify-write of the whole file. Without
        // the flatten map, opening a pool written by a Go build (or a newer
        // Rust build) and saving it would delete every field the port has not
        // implemented yet — silent, permanent metadata loss.
        let json = r#"{
  "worktrees": [
    {
      "name": "1",
      "path": "/pool/1/repo",
      "created_at": "2026-08-14T12:00:00Z",
      "base_branch": "main",
      "seeded_paths": ["node_modules", ".env"],
      "seed_inventory_known": true,
      "seed_inventory_digest": "a1b2c3",
      "seed_backend": "git",
      "seed_auth_identity": "key-1",
      "recovery_reason": "quarantined"
    }
  ]
}"#;
        let s: State = serde_json::from_str(json).unwrap();
        let out = serde_json::to_string(&s).unwrap();
        for (key, expected) in [
            ("base_branch", "\"main\""),
            ("seeded_paths", r#"["node_modules",".env"]"#),
            ("seed_inventory_known", "true"),
            ("seed_inventory_digest", "\"a1b2c3\""),
            ("seed_backend", "\"git\""),
            ("seed_auth_identity", "\"key-1\""),
            ("recovery_reason", "\"quarantined\""),
        ] {
            assert!(
                out.contains(&format!(r#""{key}":{expected}"#)),
                "field {key} was dropped on write: {out}"
            );
        }
        // And the values are reachable as data, not just as text.
        assert_eq!(s.worktrees[0].extra["base_branch"], "main");
        assert_eq!(
            s.worktrees[0].extra["seeded_paths"][1].as_str(),
            Some(".env")
        );
    }

    #[test]
    fn top_level_unknown_fields_round_trip_through_a_real_write() {
        // The per-entry flatten map is not enough on its own: Go's `State`
        // carries `version` at the TOP level (state.go:82, currently 4), and
        // every write path is a read-modify-write of the whole file through
        // `State`. Without a top-level catch-all, the very first Rust command
        // to touch a Go-written pool (`get`, `return`, `prune`) would rewrite
        // the file with no `version` at all — silent, permanent loss of the
        // one field Go uses to decide whether it trusts the file.
        //
        // This walks the ACTUAL cycle (read_state -> write_state -> read_state)
        // rather than serde in isolation, because serde round-tripping is not
        // the failure mode: the failure is a field being absent from the
        // struct, which only shows up once a real command rewrites the file.
        let dir = tempfile::tempdir().unwrap();
        let go_v3_state = r#"{
  "version": 4,
  "worktrees": [
    {
      "name": "1",
      "path": "/pool/1/repo",
      "created_at": "2026-08-14T12:00:00Z",
      "base_branch": "main",
      "seed_inventory_known": true,
      "seed_inventory_digest": "a1b2c3",
      "seed_backend": "git",
      "seed_auth_identity": "key-1"
    },
    {
      "name": "2",
      "path": "/pool/2/repo",
      "created_at": "2026-08-14T13:00:00Z",
      "leased": true,
      "lease_id": "9f2c1e04a7b3d5c8e6f10293a4b5c6d7",
      "lease_holder": "agent-42",
      "leased_at": "2026-08-14T13:00:01Z"
    }
  ],
  "pool_note": "written by a newer treehouse"
}"#;
        std::fs::write(State::state_file_path(dir.path()), go_v3_state).unwrap();

        // Read 1: the file as another build left it.
        let read1 = State::read_state(dir.path()).unwrap();
        assert_eq!(read1.worktrees.len(), 2);

        // The write half: exactly what every mutating command does.
        crate::state_file::write_state(dir.path(), &read1).unwrap();

        // Assert on the BYTES ON DISK, not on the parsed struct. A struct that
        // parsed cleanly is not evidence of anything: serde ignores unknown
        // fields by default, so a `State` without a catch-all reads this file
        // perfectly and then writes it back with `version` silently deleted.
        // Only the bytes distinguish "preserved" from "quietly dropped".
        let raw = std::fs::read_to_string(State::state_file_path(dir.path())).unwrap();
        for key in [
            "version",
            "pool_note",
            "base_branch",
            "seed_inventory_known",
            "seed_inventory_digest",
            "seed_backend",
            "seed_auth_identity",
        ] {
            assert!(
                raw.contains(&format!("\"{key}\"")),
                "field {key} was dropped by the write: {raw}"
            );
        }
        assert!(
            raw.contains(r#""version": 4"#),
            "the version VALUE must survive, not just the key: {raw}"
        );

        // Read 2: the second cycle is lossless too, so a file that has already
        // been through one Rust command stays stable.
        let read2 = State::read_state(dir.path()).unwrap();
        assert_eq!(read2, read1, "read -> write -> read must be lossless");
        assert_eq!(read2.worktrees[0].extra["base_branch"], "main");
        // Modelled fields are untouched by the whole exercise.
        assert!(read2.worktrees[1].leased);
        assert_eq!(read2.worktrees[1].lease_holder, "agent-42");
    }

    #[test]
    fn state_extra_stays_empty_off_the_wire_when_nothing_is_unknown() {
        // The mirror of the round trip: a Rust-written file must not grow a
        // `version` (or any other key) that Go would never emit, so two
        // implementations agree byte-for-byte on a pool neither has annotated.
        let json = serde_json::to_string(&State::default()).unwrap();
        assert_eq!(json, r#"{"worktrees":[]}"#, "unexpected wire shape: {json}");
    }

    #[test]
    fn known_and_unknown_fields_survive_together() {
        let mut wt = WorktreeEntry {
            name: "3".into(),
            path: "/pool/3/repo".into(),
            created_at: dt("2026-08-14T12:00:00Z"),
            leased: true,
            recovery_error: "resolving .git marker: permission denied".into(),
            ..WorktreeEntry::default()
        };
        wt.extra
            .insert("base_branch".into(), serde_json::json!("release"));

        let json = serde_json::to_string(&wt).unwrap();
        let back: WorktreeEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wt, "round trip must be lossless");
        assert!(json.contains(r#""recovery_error":"#), "{json}");
        assert!(json.contains(r#""base_branch":"release""#), "{json}");
    }

    #[test]
    fn empty_extra_and_recovery_error_stay_off_the_wire() {
        // Go omits both when empty; a Rust-written state file must not grow
        // keys the baseline never emits.
        let json = serde_json::to_string(&WorktreeEntry {
            name: "1".into(),
            path: "/p".into(),
            created_at: dt("2026-08-14T12:00:00Z"),
            ..WorktreeEntry::default()
        })
        .unwrap();
        assert!(!json.contains("recovery_error"), "{json}");
        assert!(!json.contains("base_branch"), "{json}");
    }

    #[test]
    fn recovery_error_serializes_omitzero_like_go() {
        let json = serde_json::to_string(&WorktreeEntry {
            name: "1".into(),
            path: "/p".into(),
            created_at: dt("2026-08-14T12:00:00Z"),
            recovery_error: "loop".into(),
            ..WorktreeEntry::default()
        })
        .unwrap();
        assert!(json.contains(r#""recovery_error":"loop""#), "{json}");
    }

    // ─── _with_env tests ────────────────────────────────────────────────────

    #[test]
    fn read_state_with_env_missing_returns_empty() {
        let env = crate::env::InMemoryEnv::new(std::path::PathBuf::from("/test"));
        let state =
            State::read_state_with_env(&std::path::PathBuf::from("/test/empty"), &env).unwrap();
        assert!(state.worktrees.is_empty());
    }

    #[test]
    fn read_state_with_env_valid_state() {
        let env = crate::env::InMemoryEnv::new(std::path::PathBuf::from("/test"));
        let pool_dir = std::path::PathBuf::from("/test/pool");
        let state = State {
            worktrees: vec![WorktreeEntry {
                name: "1".into(),
                path: "/test/pool/1/repo".into(),
                created_at: dt("2026-07-20T12:00:00Z"),
                ..Default::default()
            }],
            ..Default::default()
        };
        // Seed state file via env
        let json = serde_json::to_string_pretty(&state).unwrap();
        env.seed_file(&State::state_file_path(&pool_dir), json.as_bytes());

        let loaded = State::read_state_with_env(&pool_dir, &env).unwrap();
        assert_eq!(state, loaded);
    }

    #[test]
    fn read_state_with_env_corrupt_triggers_recovery() {
        // Recovery uses std::fs::read_dir internally, so we need real dirs.
        // Use DefaultEnv with a tempdir for this test.
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path();
        // Seed corrupt state file
        std::fs::write(State::state_file_path(pool_dir), b"corrupt").unwrap();
        // Seed a worktree dir for recovery
        let wt = pool_dir.join("1/myrepo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), b"gitdir: ../../fake.git\n").unwrap();

        let env = crate::env::DefaultEnv;
        let state = State::read_state_with_env(pool_dir, &env).unwrap();
        assert_eq!(state.worktrees.len(), 1);
        assert!(state.worktrees[0].leased); // recovered entries are leased
    }
}
