//! Reservation tokens: the persisted ownership identities that make the
//! short-lock protocol safe.
//!
//! A reservation is **persisted in the state file** (`owner_pid` +
//! `owner_started_at`, the `{Destroying, OwnerPID, OwnerStartedAt}` triple, or
//! `lease_id`), so it survives lock releases and can be re-validated under a
//! fresh lock. This is the Rust analog of Go's `sameDestroyReservation`.
//!
//! Ownership is three independent facts, never inferred from each other:
//! live process, owner reservation, durable lease.

use crate::lease::Lease;
use crate::process::ProcessTable;
use crate::state::WorktreeEntry;

/// A persisted reservation token for one worktree.
#[derive(Debug, Clone)]
pub struct Reservation {
    pub worktree: String,
    pub kind: ReservationKind,
}

/// The kind of reservation held on a worktree.
#[derive(Debug, Clone)]
pub enum ReservationKind {
    /// A short-lived owner reservation (normal acquire): `{OwnerPID,
    /// OwnerStartedAt}`.
    Owner {
        owner_pid: i32,
        owner_started_at: i64,
    },
    /// The two-phase destroy reservation: a fresh owner pair stamped with
    /// `Destroying=true`, plus the ORIGINAL owner pair to restore on any skip.
    Destroy {
        owner_pid: i32,
        owner_started_at: i64,
        original_owner_pid: i32,
        original_owner_started_at: i64,
    },
    /// A durable, process-independent lease.
    Lease(Lease),
}

impl Reservation {
    /// Whether the reservation still matches a (re-read) worktree entry —
    /// the exact analog of Go's `sameDestroyReservation`. For `Destroy`, all
    /// of Path, Destroying, OwnerPID, OwnerStartedAt must match. For `Owner`/
    /// `Lease`, the persisted identity fields must match.
    pub fn matches(&self, entry: &WorktreeEntry) -> bool {
        if entry.path != self.worktree {
            return false;
        }
        match &self.kind {
            ReservationKind::Destroy {
                owner_pid,
                owner_started_at,
                ..
            } => {
                entry.destroying
                    && entry.owner_pid == *owner_pid
                    && entry.owner_started_at == *owner_started_at
            }
            ReservationKind::Owner {
                owner_pid,
                owner_started_at,
            } => {
                !entry.destroying
                    && entry.owner_pid == *owner_pid
                    && entry.owner_started_at == *owner_started_at
            }
            ReservationKind::Lease(lease) => {
                entry.leased && entry.lease_id == lease.id && entry.lease_holder == lease.holder
            }
        }
    }

    /// Restores the pre-destroy owner state on a skip (Go
    /// `restoreOriginalOwnerReservation`): clears `Destroying` and restores the
    /// original owner pair.
    pub fn restore_original(&self, entry: &mut WorktreeEntry) {
        if let ReservationKind::Destroy {
            original_owner_pid,
            original_owner_started_at,
            ..
        } = self.kind
        {
            entry.destroying = false;
            entry.owner_pid = original_owner_pid;
            entry.owner_started_at = original_owner_started_at;
        }
    }

    /// Stamps a fresh owner reservation onto `entry` (Go `reserveOwner`):
    /// `owner_pid = getpid()`, `owner_started_at = started_at_ms(pid)`. Errors
    /// if the owner's own start time can't be determined.
    pub fn reserve_owner(
        &self,
        entry: &mut WorktreeEntry,
        process: &ProcessTable,
    ) -> Result<(), ReservationError> {
        let pid = std::process::id() as i32;
        let started_at = process.started_at(pid).ok_or_else(|| {
            ReservationError::Undeterminable(format!("process {pid} start time unavailable"))
        })?;
        entry.owner_pid = pid;
        entry.owner_started_at = started_at;
        Ok(())
    }

    /// Builds a `Destroy` reservation for a worktree: saves the original owner
    /// pair, sets `Destroying = true`, and stamps a fresh owner reservation
    /// (Go `reserveDestroyReservation`). The returned token must be persisted
    /// and re-validated via [`Reservation::matches`] under a fresh lock.
    pub fn reserve_destroy(
        worktree: &str,
        entry: &mut WorktreeEntry,
        process: &ProcessTable,
    ) -> Result<Self, ReservationError> {
        let original_owner_pid = entry.owner_pid;
        let original_owner_started_at = entry.owner_started_at;
        entry.destroying = true;
        let pid = std::process::id() as i32;
        let started_at = process.started_at(pid).ok_or_else(|| {
            ReservationError::Undeterminable(format!("process {pid} start time unavailable"))
        })?;
        entry.owner_pid = pid;
        entry.owner_started_at = started_at;
        Ok(Reservation {
            worktree: worktree.to_string(),
            kind: ReservationKind::Destroy {
                owner_pid: pid,
                owner_started_at: started_at,
                original_owner_pid,
                original_owner_started_at,
            },
        })
    }
}

/// Errors from reservation operations.
#[derive(Debug, thiserror::Error)]
pub enum ReservationError {
    #[error("failed to determine owner process identity: {0}")]
    Undeterminable(String),
}

/// Whether a worktree's owner reservation is held by a live, matching process
/// (Go `ownerAlive`): requires BOTH a nonzero pid and start time, AND that the
/// process's actual start time (epoch **millis**) equals the recorded one
/// (PID-reuse safe).
pub fn owner_alive(entry: &WorktreeEntry, process: &ProcessTable) -> bool {
    if entry.owner_pid == 0 || entry.owner_started_at == 0 {
        return false;
    }
    match process.started_at(entry.owner_pid) {
        Some(started_at) => started_at == entry.owner_started_at,
        None => false,
    }
}

/// Self-heals a dead owner reservation on `entry` (mirrors `heal_state`): when
/// `owner_pid != 0` but the owner is not alive, zero the owner pair and clear
/// `Destroying`. Never touches lease fields.
pub fn heal_owner(entry: &mut WorktreeEntry, process: &ProcessTable) {
    if entry.owner_pid != 0 && !owner_alive(entry, process) {
        entry.owner_pid = 0;
        entry.owner_started_at = 0;
        entry.destroying = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn entry(pid: i32, started: i64) -> WorktreeEntry {
        WorktreeEntry {
            name: "1".into(),
            path: "/pool/1/myrepo".into(),
            owner_pid: pid,
            owner_started_at: started,
            ..WorktreeEntry::default()
        }
    }

    #[test]
    fn matches_is_same_destroy_reservation() {
        let process = ProcessTable::new();
        let mut e = entry(0, 0);
        let r = Reservation::reserve_destroy("/pool/1/myrepo", &mut e, &process).unwrap();
        // Matching entry => matches.
        assert!(r.matches(&e));
        // A re-acquired worktree (different owner pair) => does NOT match.
        let mut reacquired = e.clone();
        reacquired.owner_pid = 999;
        assert!(
            !r.matches(&reacquired),
            "re-acquired worktree must not match"
        );
        // Destroying cleared => does NOT match.
        let mut cleared = e.clone();
        cleared.destroying = false;
        assert!(!r.matches(&cleared));
        // Different path => does NOT match.
        let mut diff_path = e.clone();
        diff_path.path = "/other".into();
        assert!(!r.matches(&diff_path));
    }

    #[test]
    fn restore_original_exactly_restores_pre_destroy_state() {
        let process = ProcessTable::new();
        let mut e = entry(100, 1000);
        let r = Reservation::reserve_destroy("/pool/1/myrepo", &mut e, &process).unwrap();
        // Original pair was (100, 1000); now Destroying + fresh owner.
        assert!(e.destroying);
        assert_ne!((e.owner_pid, e.owner_started_at), (100, 1000));

        // Simulate a skip: restore the original reservation.
        r.restore_original(&mut e);
        assert!(!e.destroying);
        assert_eq!(e.owner_pid, 100);
        assert_eq!(e.owner_started_at, 1000);
    }

    #[test]
    fn owner_alive_requires_matching_start_time() {
        let process = ProcessTable::new();
        // Zero fields => not alive.
        assert!(!owner_alive(&entry(0, 0), &process));
        assert!(!owner_alive(&entry(123, 0), &process));
        assert!(!owner_alive(&entry(0, 12345), &process));

        // Non-existent pid => not alive.
        assert!(!owner_alive(&entry(999_999, 12345), &process));

        // Our own process with its actual start time => alive.
        let me = std::process::id() as i32;
        let started = process.started_at(me).unwrap();
        let mut e = entry(me, started);
        assert!(owner_alive(&e, &process));

        // Recycled pid (start time differs) => NOT alive — the ABA guard.
        e.owner_started_at = started + 1;
        assert!(!owner_alive(&e, &process));
    }

    #[test]
    fn heal_clears_dead_owner_but_never_leases() {
        let process = ProcessTable::new();
        let me = std::process::id() as i32;
        let started = process.started_at(me).unwrap();

        // Dead owner (pid 999_999) => healed.
        let mut dead = entry(999_999, 12345);
        heal_owner(&mut dead, &process);
        assert_eq!(dead.owner_pid, 0);
        assert_eq!(dead.owner_started_at, 0);
        assert!(!dead.destroying);

        // Live owner => untouched.
        let mut live = entry(me, started);
        heal_owner(&mut live, &process);
        assert_eq!(live.owner_pid, me);

        // Leased entry => lease fields never touched, even with a dead owner.
        let mut leased = entry(999_999, 12345);
        leased.leased = true;
        leased.lease_id = "abc".into();
        leased.lease_holder = "holder".into();
        leased.leased_at = DateTime::parse_from_rfc3339("2026-08-14T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        heal_owner(&mut leased, &process);
        assert!(leased.leased, "heal must not clear a lease");
        assert_eq!(leased.lease_id, "abc");
        assert_eq!(leased.owner_pid, 0, "dead owner still healed");
    }

    #[test]
    fn reserve_owner_uses_millis() {
        let process = ProcessTable::new();
        let mut e = WorktreeEntry {
            path: "/pool/1/myrepo".into(),
            ..WorktreeEntry::default()
        };
        let reservation = Reservation {
            worktree: "/pool/1/myrepo".into(),
            kind: ReservationKind::Owner {
                owner_pid: 0,
                owner_started_at: 0,
            },
        };
        reservation.reserve_owner(&mut e, &process).unwrap();
        assert_eq!(e.owner_pid, std::process::id() as i32);
        assert!(
            e.owner_started_at > 1_000_000_000_000,
            "owner_started_at must be epoch millis, got {}",
            e.owner_started_at
        );
    }
}
