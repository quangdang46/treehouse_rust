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
//! - A **missing** state file is a fresh, empty pool.
//! - A file that **exists but fails to parse** (empty or truncated) is
//!   *corrupt*, not missing — it triggers conservative recovery that marks
//!   every on-disk worktree as leased until a human verifies it.

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
}

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
}

/// The pool state file: an ordered list of managed worktrees.
///
/// `worktrees` is always present in the wire format (Go has no `omitempty`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct State {
    #[serde(default)]
    pub worktrees: Vec<WorktreeEntry>,
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
    /// A missing file is a fresh empty pool. A file that exists but fails to
    /// parse (empty or truncated) is corrupt: it is conservatively recovered
    /// by scanning the pool directory for worktrees still on disk and marking
    /// each `leased` (see [`recover_corrupt_state`]). If that scan cannot
    /// complete, the call fails closed rather than returning partial state.
    pub fn read_state(pool_dir: &Path) -> Result<State, StateError> {
        let path = Self::state_file_path(pool_dir);
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(StateError::Read(path, e)),
        };
        match serde_json::from_slice(&data) {
            Ok(s) => Ok(s),
            Err(parse_err) => recover_corrupt_state(pool_dir, path, parse_err),
        }
    }

    /// Reads pool state using the injected environment.
    pub fn read_state_with_env(
        pool_dir: &Path,
        env: &dyn crate::env::TreehouseEnv,
    ) -> Result<State, StateError> {
        let path = Self::state_file_path(pool_dir);
        let data = match env.read_bytes(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(StateError::Read(path, e)),
        };
        match serde_json::from_slice(&data) {
            Ok(s) => Ok(s),
            Err(parse_err) => recover_corrupt_state(pool_dir, path, parse_err),
        }
    }
}

/// Marker placed on entries reconstructed by [`recover_corrupt_state`] so
/// callers (status output, destroy) can explain why they are unexpectedly
/// leased. Mirrors Go's `recoveredLeaseHolder`.
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
    let slots =
        std::fs::read_dir(pool_dir).map_err(|e| StateError::RecoverScan(state_path.clone(), e))?;

    let mut recovered = Vec::new();
    for slot in slots {
        let slot = slot.map_err(|e| StateError::RecoverScan(state_path.clone(), e))?;
        if !slot
            .file_type()
            .map_err(|e| StateError::RecoverScan(state_path.clone(), e))?
            .is_dir()
        {
            continue;
        }
        let slot_dir = pool_dir.join(slot.file_name());
        let nested = std::fs::read_dir(&slot_dir)
            .map_err(|e| StateError::RecoverScan(state_path.clone(), e))?;
        for entry in nested {
            let entry = entry.map_err(|e| StateError::RecoverScan(state_path.clone(), e))?;
            if !entry
                .file_type()
                .map_err(|e| StateError::RecoverScan(state_path.clone(), e))?
                .is_dir()
            {
                continue;
            }
            let wt_path = slot_dir.join(entry.file_name());
            match std::fs::metadata(wt_path.join(".git")) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StateError::RecoverScan(state_path.clone(), e)),
            }
            let now = chrono::Utc::now();
            recovered.push(WorktreeEntry {
                name: slot.file_name().to_string_lossy().into_owned(),
                path: wt_path.to_string_lossy().into_owned(),
                created_at: now,
                leased: true,
                lease_holder: RECOVERED_LEASE_HOLDER.to_string(),
                leased_at: now,
                ..WorktreeEntry::default()
            });
        }
    }

    eprintln!(
        "treehouse: WARNING: state file {} is corrupt or truncated ({parse_err}); recovering from worktrees found on disk. They are marked leased until verified - see `treehouse status`, then `treehouse return` or `treehouse destroy --include-leased`.",
        state_path.display()
    );
    Ok(State {
        worktrees: recovered,
    })
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
    #[error("state file {0} is corrupt or truncated and recovery could not scan: {1}")]
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
            }],
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
