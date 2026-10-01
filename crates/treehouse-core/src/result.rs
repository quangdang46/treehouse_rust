//! Command results: one structured result per command.
//!
//! Core produces a single [`CommandResult`] and serializes it to a
//! `serde_json::Value`; the CLI formats that Value as JSON or TOON. Core never
//! knows what TOON is — the abstraction boundary is
//! `CommandResult -> serde_json::Value -> { JSON, TOON }`.

use chrono::{DateTime, Utc};

use crate::destroy::{DestroyResult, DestroySkip, DestroyTarget};
use crate::prune::PruneResult;
use crate::state::ZERO_TIME;

/// A process found inside a worktree (used in status/return output).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProcessJson {
    pub pid: i32,
    pub name: String,
}

/// One worktree's status for JSON output. ALL keys always present:
/// `lease_id`/`lease_holder` are `""` when not leased; `leased_at` is `null`
/// when not leased; `processes` is `[]` when none.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StatusJson {
    pub name: String,
    pub path: String,
    pub status: String,
    pub lease_id: String,
    pub lease_holder: String,
    pub leased_at: Option<String>,
    pub processes: Vec<ProcessJson>,
}

impl StatusJson {
    pub fn from_ws(ws: &crate::pool::WorktreeStatus) -> Self {
        StatusJson {
            name: ws.name.clone(),
            path: ws.path.clone(),
            status: ws.status.clone(),
            lease_id: ws.lease_id.clone(),
            lease_holder: ws.lease_holder.clone(),
            leased_at: if ws.leased_at == ZERO_TIME {
                None
            } else {
                Some(rfc3339(ws.leased_at))
            },
            processes: ws
                .processes
                .iter()
                .map(|p| ProcessJson {
                    pid: p.pid,
                    name: p.name.clone(),
                })
                .collect(),
        }
    }
}

/// The result of a `return` command (NEW schema).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReturnResult {
    pub path: String,
    pub returned: bool,
    pub aborted: bool,
    pub terminated: Vec<ProcessJson>,
    pub warnings: Vec<String>,
}

/// The result of a `destroy` command (NEW schema).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DestroyResultJson {
    pub dry_run: bool,
    pub all: bool,
    pub scope: String,
    pub planned: Vec<DestroyTargetJson>,
    pub destroyed: Vec<DestroyTargetJson>,
    pub skipped: Vec<DestroySkipJson>,
    pub planned_bytes: u64,
    pub freed_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DestroyTargetJson {
    pub name: String,
    pub path: String,
    pub class: String,
    pub bytes: u64,
    pub detail: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DestroySkipJson {
    pub target: DestroyTargetJson,
    pub needed_flags: Vec<String>,
    pub detail: String,
}

impl DestroyResultJson {
    pub fn from_result(r: &DestroyResult) -> Self {
        DestroyResultJson {
            dry_run: r.dry_run,
            all: r.all,
            scope: r.scope.clone(),
            planned: r.planned.iter().map(target_json).collect(),
            destroyed: r.destroyed.iter().map(target_json).collect(),
            skipped: r.skipped.iter().map(skip_json).collect(),
            planned_bytes: r.planned_bytes,
            freed_bytes: r.freed_bytes,
        }
    }
}

fn target_json(t: &DestroyTarget) -> DestroyTargetJson {
    DestroyTargetJson {
        name: t.name.clone(),
        path: t.path.clone(),
        class: t.class.to_string(),
        bytes: t.bytes,
        detail: t.detail.clone(),
    }
}

fn skip_json(s: &DestroySkip) -> DestroySkipJson {
    DestroySkipJson {
        target: target_json(&s.target),
        needed_flags: s.needed_flags.iter().map(|f| f.to_string()).collect(),
        detail: s.detail.clone(),
    }
}

/// The result of a `prune` command (NEW schema).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PruneResultJson {
    pub dry_run: bool,
    pub orphans_included: bool,
    pub global: bool,
    pub pool_count: u32,
    pub candidates: u32,
    pub pruned: u32,
    pub skipped: u32,
    /// Worktrees whose physical cleanup failed (state retained for retry).
    pub errors: u32,
    pub reclaimable_bytes: u64,
    pub freed_bytes: u64,
}

impl PruneResultJson {
    pub fn from_result(r: &PruneResult, scope: SweepScope) -> Self {
        PruneResultJson {
            dry_run: r.dry_run,
            orphans_included: scope.orphans_included,
            global: scope.global,
            pool_count: scope.pool_count,
            candidates: r.candidates.len() as u32,
            pruned: r.pruned.len() as u32,
            skipped: r.skipped.len() as u32,
            errors: r.errors.len() as u32,
            reclaimable_bytes: r.reclaimable_bytes,
            freed_bytes: r.freed_bytes,
        }
    }
}

/// The result of a `gc` command (NEW schema).
///
/// Same shape as [`PruneResultJson`] — counts, not per-item records — so an
/// agent parsing `--format json` from either command handles one vocabulary.
/// The per-slot detail a human needs is in the human format; a caller that
/// wants names re-reads them from `status`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GcResultJson {
    pub dry_run: bool,
    pub orphans_included: bool,
    pub global: bool,
    pub pool_count: u32,
    pub candidates: u32,
    pub reclaimed: u32,
    pub skipped: u32,
    /// Worktrees whose physical cleanup failed (state retained for retry).
    pub errors: u32,
    pub reclaimable_bytes: u64,
    pub freed_bytes: u64,
}

impl GcResultJson {
    pub fn from_result(r: &crate::gc::GcResult, scope: SweepScope) -> Self {
        GcResultJson {
            dry_run: r.dry_run,
            orphans_included: scope.orphans_included,
            global: scope.global,
            pool_count: scope.pool_count,
            candidates: r.candidates.len() as u32,
            reclaimed: r.reclaimed.len() as u32,
            skipped: r.skipped.len() as u32,
            errors: r.errors.len() as u32,
            reclaimable_bytes: r.reclaimable_bytes,
            freed_bytes: r.freed_bytes,
        }
    }
}

/// How many pools a sweep covered, and whether it was global. Threaded through
/// the result so `--format json` on `prune --all` / `gc --all` says so instead
/// of reporting a single anonymous pool.
#[derive(Debug, Clone, Copy, Default)]
pub struct SweepScope {
    pub global: bool,
    pub pool_count: u32,
    pub orphans_included: bool,
}

/// The one structured result per command. `payload()` returns `None` for
/// interactive get/enter so a stray stdout write never corrupts the subshell.
#[derive(Debug, Clone)]
pub enum CommandResult {
    Get(GetResult),
    Enter,
    Return(ReturnResult),
    Status(Vec<crate::pool::WorktreeStatus>),
    Prune(PruneResult, SweepScope),
    Destroy(DestroyResult),
    Gc(crate::gc::GcResult, SweepScope),
}

/// The result of a `get` command.
#[derive(Debug, Clone)]
pub enum GetResult {
    /// Interactive (subshell): no payload.
    Interactive,
    /// Lease: path + lease identity (Go `LeaseInfo` wire shape).
    Lease(crate::lease::LeaseInfo),
}

impl CommandResult {
    /// The structured payload for machine output, if any.
    pub fn payload(&self) -> Option<serde_json::Value> {
        match self {
            CommandResult::Get(GetResult::Interactive) => None,
            CommandResult::Get(GetResult::Lease(lease)) => {
                Some(serde_json::to_value(lease).unwrap_or(serde_json::Value::Null))
            }
            CommandResult::Enter => None,
            CommandResult::Return(r) => serde_json::to_value(r).ok(),
            CommandResult::Status(statuses) => {
                let arr: Vec<StatusJson> = statuses.iter().map(StatusJson::from_ws).collect();
                Some(serde_json::to_value(arr).unwrap_or(serde_json::Value::Null))
            }
            CommandResult::Prune(r, scope) => {
                serde_json::to_value(PruneResultJson::from_result(r, *scope)).ok()
            }
            CommandResult::Destroy(r) => {
                serde_json::to_value(DestroyResultJson::from_result(r)).ok()
            }
            CommandResult::Gc(r, scope) => {
                serde_json::to_value(GcResultJson::from_result(r, *scope)).ok()
            }
        }
    }
}

/// Renders a lease timestamp for machine-readable output.
///
/// The JSON payloads are a wire contract that scripts parse, so the rendering
/// lives here rather than being open-coded at each call site: Go emits
/// RFC3339Nano while chrono emits second-precision by default, and when the
/// two are reconciled this is the single place that has to change. An inline
/// `to_rfc3339()` at the call site would hide that.
pub fn rfc3339(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339()
}
