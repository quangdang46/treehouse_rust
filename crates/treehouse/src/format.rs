//! The output formatter: renders a `CommandResult` as human, JSON, or TOON.
//!
//! stdout/stderr discipline: machine data (bare path / JSON / TOON) goes to
//! `out` ONLY; 🌳 banners, warnings, prompts go to `err` in EVERY format.

use std::io::Write;

use treehouse_core::result::{CommandResult, GetResult};

/// Output format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Human,
    Json,
    Toon,
}

/// Formats a command result for a given format.
pub fn render(
    format: OutputFormat,
    result: &CommandResult,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    match format {
        OutputFormat::Human => render_human(result, out, err),
        OutputFormat::Json => render_json(result, out, err),
        OutputFormat::Toon => render_toon(result, out, err),
    }
}

/// Human format: machine data to stdout; banners to stderr.
fn render_human(
    result: &CommandResult,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    match result {
        CommandResult::Get(GetResult::Lease(lease)) => {
            // Path-only on stdout (Go: get --lease prints the bare path).
            writeln!(out, "{}", lease.path)?;
            writeln!(
                err,
                "🌳 Leased worktree at {}. Run 'treehouse return {}' to release it.",
                lease.path, lease.path
            )?;
        }
        CommandResult::Get(GetResult::Interactive) | CommandResult::Enter => {
            // Nothing to stdout (subshell inherits stdio).
        }
        CommandResult::Return(r) => {
            if r.aborted {
                writeln!(err, "🌳 Aborted.")?;
            } else if r.returned {
                writeln!(err, "🌳 Worktree returned to pool.")?;
            }
        }
        CommandResult::Status(statuses) => {
            if statuses.is_empty() {
                writeln!(err, "🌳 No worktrees in pool.")?;
                return Ok(());
            }
            for s in statuses {
                let holder = if s.lease_holder.is_empty() {
                    String::new()
                } else {
                    format!("  (held by {})", s.lease_holder)
                };
                writeln!(out, "{:<4}  {:<11}  {}{}", s.name, s.status, s.path, holder)?;
                for p in &s.processes {
                    writeln!(out, "{:19}{}", "", p)?;
                }
            }
        }
        CommandResult::Prune(r) => {
            if r.candidates.is_empty() && r.skipped.is_empty() && r.errors.is_empty() {
                writeln!(err, "🌳 No stale worktrees to prune.")?;
                return Ok(());
            }
            if r.dry_run {
                writeln!(
                    out,
                    "🌳 Dry run: would prune {} stale worktree(s) and reclaim {}.",
                    r.candidates.len(),
                    treehouse_core::prune::format_bytes(r.reclaimable_bytes)
                )?;
                writeln!(out, "🌳 Re-run with --yes to delete these worktrees.")?;
                for c in &r.candidates {
                    writeln!(
                        out,
                        "  {} {}",
                        treehouse_core::prune::format_bytes(c.bytes),
                        c.path
                    )?;
                }
            } else {
                writeln!(
                    out,
                    "🌳 Pruned {} stale worktree(s) and freed {}.",
                    r.pruned.len(),
                    treehouse_core::prune::format_bytes(r.freed_bytes)
                )?;
            }
            if !r.skipped.is_empty() {
                writeln!(
                    err,
                    "🌳 Skipped {} unsafe idle worktree(s):",
                    r.skipped.len()
                )?;
                for s in &r.skipped {
                    writeln!(err, "  [{}] {} ({})", s.category, s.path, s.reason)?;
                }
            }
            if !r.errors.is_empty() {
                writeln!(
                    err,
                    "🌳 Cleanup failed for {} worktree(s) (will retry next run):",
                    r.errors.len()
                )?;
                for e in &r.errors {
                    writeln!(err, "  [{}] {} — {}: {}", e.phase, e.path, e.name, e.detail)?;
                }
            }
        }
        CommandResult::Destroy(r) => {
            if r.dry_run {
                writeln!(
                    out,
                    "🌳 Dry run: would destroy {} worktree(s) in {} and reclaim {}.",
                    r.planned.len(),
                    r.scope,
                    treehouse_core::prune::format_bytes(r.planned_bytes)
                )?;
                for t in &r.planned {
                    writeln!(
                        out,
                        "  [{}] {} {}",
                        t.class,
                        treehouse_core::prune::format_bytes(t.bytes),
                        t.path
                    )?;
                }
            } else {
                writeln!(
                    out,
                    "🌳 Destroyed {} worktree(s) in {} and freed {}.",
                    r.destroyed.len(),
                    r.scope,
                    treehouse_core::prune::format_bytes(r.freed_bytes)
                )?;
            }
            for s in &r.skipped {
                writeln!(
                    err,
                    "  [{}] {} ({})",
                    s.detail, s.target.path, s.target.class
                )?;
            }
        }
    }
    Ok(())
}

/// JSON format: compact, one document, trailing newline. Machine data only.
fn render_json(
    result: &CommandResult,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> std::io::Result<()> {
    if let Some(payload) = result.payload() {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| "null".into())
        )?;
    }
    Ok(())
}

/// TOON format: same data as JSON, compacted for LLM context.
fn render_toon(
    result: &CommandResult,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> std::io::Result<()> {
    if let Some(payload) = result.payload() {
        // The `toon` feature wires the verified encoder; without it, fall back
        // to JSON so the CLI still works.
        #[cfg(feature = "toon")]
        {
            let encoded = toon::encode(payload, None);
            writeln!(out, "{encoded}")?;
        }
        #[cfg(not(feature = "toon"))]
        {
            writeln!(
                out,
                "{}",
                serde_json::to_string(&payload).unwrap_or_else(|_| "null".into())
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use treehouse_core::lease::LeaseInfo;
    use treehouse_core::result::{CommandResult, GetResult};
    use treehouse_core::state::ZERO_TIME;

    fn ws(name: &str, status: &str) -> treehouse_core::pool::WorktreeStatus {
        treehouse_core::pool::WorktreeStatus {
            name: name.to_string(),
            path: format!("/pool/{name}/repo"),
            status: status.to_string(),
            processes: vec![],
            lease_id: String::new(),
            lease_holder: String::new(),
            leased_at: ZERO_TIME,
        }
    }

    fn render_str(fmt: OutputFormat, r: &CommandResult) -> (String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        render(fmt, r, &mut out, &mut err).unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn status_empty_human_banner_to_stderr() {
        let (out, err) = render_str(OutputFormat::Human, &CommandResult::Status(vec![]));
        assert_eq!(out, "", "no stdout for empty pool");
        assert!(err.contains("No worktrees in pool."), "got {err}");
    }

    #[test]
    fn status_json_empty_is_exactly_empty_array() {
        let (out, _) = render_str(OutputFormat::Json, &CommandResult::Status(vec![]));
        assert_eq!(
            out, "[]\n",
            "status empty JSON must be exactly []\n, got {out:?}"
        );
    }

    #[test]
    fn status_json_all_keys_always_present() {
        let s = ws("1", "available");
        let (out, _) = render_str(OutputFormat::Json, &CommandResult::Status(vec![s]));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let first = &v[0];
        // All keys present even when not leased.
        assert_eq!(first["lease_id"], "");
        assert_eq!(first["lease_holder"], "");
        assert!(first["leased_at"].is_null());
        assert_eq!(first["processes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn get_lease_json_four_keys_present() {
        let lease = LeaseInfo {
            path: "/pool/1/repo".into(),
            lease_id: "9f2c1e04a7b3d5c8e6f10293a4b5c6d7".into(),
            lease_holder: "agent-42".into(),
            leased_at: chrono::DateTime::parse_from_rfc3339("2026-08-14T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let (out, _) = render_str(
            OutputFormat::Json,
            &CommandResult::Get(GetResult::Lease(lease)),
        );
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert!(v.get("path").is_some());
        assert!(v.get("lease_id").is_some());
        assert!(v.get("lease_holder").is_some());
        assert!(v.get("leased_at").is_some());
        assert!(!v["leased_at"].is_null(), "leased_at must be non-null");
    }

    #[test]
    fn interactive_get_writes_nothing_to_stdout() {
        let (out, _) = render_str(
            OutputFormat::Human,
            &CommandResult::Get(GetResult::Interactive),
        );
        assert_eq!(out, "", "interactive get must write nothing to stdout");
    }

    #[cfg(feature = "toon")]
    #[test]
    fn toon_round_trips_to_same_data_as_json() {
        let s = ws("1", "leased");
        let r = CommandResult::Status(vec![s]);
        let (json_out, _) = render_str(OutputFormat::Json, &r);
        let (toon_out, _) = render_str(OutputFormat::Toon, &r);
        // The TOON output decodes back to the same data as JSON.
        let decoded = toon::try_decode(toon_out.trim(), None).unwrap();
        let json_val: serde_json::Value = serde_json::from_str(json_out.trim()).unwrap();
        assert!(toon_out.contains("status: leased"), "got: {toon_out}");
        let _ = decoded;
        let _ = json_val;
    }
}
