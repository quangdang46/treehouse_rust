//! Durable leases: a process-independent reservation of a worktree.
//!
//! Go's durable lease (`leased` + `lease_id` + `lease_holder` + `leased_at`)
//! is deliberately NOT process-derived. A lease survives with zero processes
//! inside the worktree, is never cleared by `heal_state`, is skipped by
//! `get`/`prune`, and is cleared only by `return` (or an explicit single-target
//! `destroy --include-leased`).
//!
//! A lease carries **no owner reservation** (`owner_pid`/`owner_started_at`
//! are zero) — the two ownership facts are independent.
//!
//! `lease_id` is a fresh 128-bit random hex per acquisition and is immutable
//! for that acquisition. P1 adds `expires_at` (TTL); `None` = permanent lease
//! (matches Go's zero `expires_at`), and pre-P1 state files load identically.

use chrono::{DateTime, Utc};

use crate::state::{WorktreeEntry, ZERO_TIME, clear_lease as state_clear_lease, new_lease_id};

/// The machine-readable identity of one lease acquisition. Mirrors Go's
/// `LeaseInfo` JSON contract (`get --lease --json`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LeaseInfo {
    pub path: String,
    pub lease_id: String,
    pub lease_holder: String,
    #[serde(with = "crate::state::rfc3339_nano")]
    pub leased_at: DateTime<Utc>,
}

/// A durable, process-independent reservation (P0 fields; `expires_at` is P1).
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub id: String,
    pub holder: String,
    pub acquired_at: DateTime<Utc>,
    /// None = permanent lease (matches Go zero `expires_at`).
    pub expires_at: Option<DateTime<Utc>>,
}

impl Lease {
    /// Stamps the flattened Go lease fields onto a worktree entry (what Go's
    /// `markAcquired` does in lease mode): sets `Leased`, `LeaseID`,
    /// `LeaseHolder`, `LeasedAt`, and zeroes the owner pair so a lease carries
    /// no owner reservation.
    pub fn write_to(&self, e: &mut WorktreeEntry) {
        e.leased = true;
        e.lease_id = self.id.clone();
        e.lease_holder = self.holder.clone();
        e.leased_at = self.acquired_at;
        e.owner_pid = 0;
        e.owner_started_at = 0;
        if let Some(exp) = self.expires_at {
            e.expires_at = exp;
        } else {
            e.expires_at = ZERO_TIME;
        }
    }

    /// Reconstitutes a `Lease` from a state entry. `expires_at` is re-derived
    /// from the entry's `expires_at` field (P1); for pre-P1 files it is zero,
    /// which maps to `None` (permanent).
    pub fn from_entry(e: &WorktreeEntry) -> Option<Self> {
        if !e.leased {
            return None;
        }
        Some(Lease {
            id: e.lease_id.clone(),
            holder: e.lease_holder.clone(),
            acquired_at: e.leased_at,
            expires_at: if e.expires_at == ZERO_TIME {
                None
            } else {
                Some(e.expires_at)
            },
        })
    }
}

/// Builds the `LeaseInfo` JSON shape from a state entry (Go
/// `leaseInfoFromEntry`).
pub fn lease_info_from_entry(e: &WorktreeEntry) -> LeaseInfo {
    LeaseInfo {
        path: e.path.clone(),
        lease_id: e.lease_id.clone(),
        lease_holder: e.lease_holder.clone(),
        leased_at: e.leased_at,
    }
}

/// Marks a worktree entry as durably leased (Go `markAcquired` in lease mode):
/// generates a fresh lease id, stamps the lease fields, and zeroes the owner
/// reservation. Returns the lease id.
pub fn mark_acquired_lease(wt: &mut WorktreeEntry, holder: &str, now: DateTime<Utc>) -> String {
    let id = new_lease_id();
    let lease = Lease {
        id: id.clone(),
        holder: holder.to_string(),
        acquired_at: now,
        expires_at: None,
    };
    lease.write_to(wt);
    id
}

/// Clears a durable lease from an entry (delegates to `state::clear_lease`).
pub fn clear_lease(wt: &mut WorktreeEntry) {
    state_clear_lease(wt);
}

/// Errors from lease operations.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error(
        "lease precondition failed: expected lease {expected:?} by {expected_holder:?}, but worktree is leased {actual:?} by {actual_holder:?}"
    )]
    PreconditionFailed {
        expected: String,
        expected_holder: String,
        actual: String,
        actual_holder: String,
    },
    #[error("no worktree leased at {0}")]
    NotFound(String),
    #[error("lease at {path} expired at {expired_at}")]
    Expired {
        path: String,
        expired_at: DateTime<Utc>,
    },
    #[error("generating lease identity: {0}")]
    Rng(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{State, heal_state};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-14T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn lease_has_no_owner_reservation() {
        let mut wt = WorktreeEntry {
            name: "1".into(),
            path: "/pool/1/myrepo".into(),
            owner_pid: 999,
            owner_started_at: 12345,
            ..WorktreeEntry::default()
        };
        let id = mark_acquired_lease(&mut wt, "agent-42", now());
        assert!(wt.leased);
        assert_eq!(wt.lease_id, id);
        assert_eq!(wt.lease_holder, "agent-42");
        assert_eq!(wt.leased_at, now());
        // A lease carries no owner reservation.
        assert_eq!(wt.owner_pid, 0, "lease must zero owner_pid");
        assert_eq!(wt.owner_started_at, 0, "lease must zero owner_started_at");
    }

    #[test]
    fn lease_id_is_fresh_per_acquisition() {
        let mut a = WorktreeEntry::default();
        let mut b = WorktreeEntry::default();
        let id_a = mark_acquired_lease(&mut a, "same", now());
        let id_b = mark_acquired_lease(&mut b, "same", now());
        assert_eq!(id_a.len(), 32);
        assert_ne!(id_a, id_b, "lease id must be fresh per acquisition");
    }

    #[test]
    fn lease_survives_heal() {
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
                leased_at: now(),
                ..WorktreeEntry::default()
            }],
        };
        // Even with zero matching processes, heal must not clear the lease.
        heal_state(&mut state, |_| None);
        assert!(state.worktrees[0].leased, "lease must survive heal_state");
        assert_eq!(state.worktrees[0].lease_id, "abc");
    }

    #[test]
    fn from_entry_round_trip() {
        let mut wt = WorktreeEntry::default();
        let id = mark_acquired_lease(&mut wt, "h", now());
        let lease = Lease::from_entry(&wt).unwrap();
        assert_eq!(lease.id, id);
        assert_eq!(lease.holder, "h");
        assert_eq!(lease.acquired_at, now());
        assert_eq!(lease.expires_at, None, "P0 lease has no TTL");

        // Non-leased entry yields None.
        assert!(Lease::from_entry(&WorktreeEntry::default()).is_none());
    }

    #[test]
    fn clear_lease_returns_to_unleased() {
        let mut wt = WorktreeEntry::default();
        mark_acquired_lease(&mut wt, "h", now());
        wt.expires_at = now() + chrono::Duration::hours(1);
        clear_lease(&mut wt);
        assert!(!wt.leased);
        assert_eq!(wt.lease_id, "");
        assert_eq!(wt.lease_holder, "");
        assert_eq!(wt.leased_at, ZERO_TIME);
        assert_eq!(wt.expires_at, ZERO_TIME);
        assert!(Lease::from_entry(&wt).is_none());
    }

    #[test]
    fn lease_fields_serialize_omitempty() {
        let mut wt = WorktreeEntry {
            name: "4".into(),
            path: "/pool/4/myrepo".into(),
            ..WorktreeEntry::default()
        };
        let id = mark_acquired_lease(&mut wt, "automation-A", now());
        let json = serde_json::to_string_pretty(&wt).unwrap();
        assert!(json.contains(&format!(r#""lease_id": "{id}""#)), "{json}");
        assert!(json.contains(r#""lease_holder": "automation-A""#));
        assert!(
            json.contains(r#""leased_at": "2026-08-14T12:00:00Z""#),
            "{json}"
        );
        // Owner fields must be omitted (zero).
        assert!(!json.contains("owner_pid"), "{json}");

        // Unleased entry: no lease keys at all.
        let plain = serde_json::to_string_pretty(&WorktreeEntry {
            name: "1".into(),
            path: "/p".into(),
            created_at: now(),
            ..WorktreeEntry::default()
        })
        .unwrap();
        assert!(!plain.contains("lease_id"), "{plain}");
    }

    #[test]
    fn lease_info_json_shape_matches_go() {
        let mut wt = WorktreeEntry {
            name: "4".into(),
            path: "/pool/4/myrepo".into(),
            ..WorktreeEntry::default()
        };
        mark_acquired_lease(&mut wt, "automation-A", now());
        let info = lease_info_from_entry(&wt);
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(
            json,
            r#"{"path":"/pool/4/myrepo","lease_id":"replaceme","lease_holder":"automation-A","leased_at":"2026-08-14T12:00:00Z"}"#.replace(
                "replaceme",
                &info.lease_id
            )
        );
    }
}
