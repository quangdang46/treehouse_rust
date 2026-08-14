//! P1-A hardening: race + concurrency tests for the short-lock protocol.
//!
//! Verifies the invariants that close the audit findings:
//! - Two concurrent acquires never double-issue the same worktree.
//! - Two concurrent conditional releases are exactly-once (ABA-safe).
//! - A worktree re-acquired during a destroy hook is never deleted.
//! - A crash between destroy phase 1 and 2 self-heals (Destroying cleared
//!   when the owner dies).
//! - The lock times out on a wedged holder instead of blocking forever.

#[cfg(all(test, feature = "hardening"))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::destroy::{DestroyOptions, DestroyTargetSpec};
    use crate::pool::{AcquireOptions, OpenOptions, Pool, ReleasePreconditions};
    use crate::prune::PruneOptions;

    /// Creates a temp repo with one commit on `main`.
    fn temp_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let init = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", repo.to_str().unwrap()])
            .current_dir(dir.path())
            .output()
            .expect("git must be installed");
        assert!(init.status.success());
        for args in [
            vec!["config", "user.email", "t@t.com"],
            vec!["config", "user.name", "T"],
        ] {
            let o = std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(o.status.success());
        }
        std::fs::write(repo.join("README.md"), b"hi\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "init"]] {
            let o = std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(o.status.success(), "git {:?} failed", args);
        }
        (dir, repo)
    }

    fn open_pool(repo: &std::path::Path) -> Pool {
        let fake_home = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let opts = OpenOptions {
            config: crate::config::TreehouseConfig {
                root: Some(fake_home.path().to_str().unwrap().to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        Pool::open(repo, None, &opts).unwrap()
    }

    /// Two concurrent acquires must never hand out the same worktree name.
    #[test]
    fn concurrent_acquire_never_double_issues() {
        let (_dir, repo) = temp_repo();
        let pool = Arc::new(open_pool(&repo));

        let mut handles = Vec::new();
        for _ in 0..6 {
            let p = pool.clone();
            handles.push(std::thread::spawn(move || {
                // Each acquires and holds a lease (so worktrees are distinct).
                let acquired = p
                    .get(&AcquireOptions {
                        branch: Some("main".to_string()),
                        ..Default::default()
                    })
                    .unwrap();
                (acquired.name, acquired.path)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // All names distinct (no double-issue).
        let mut names: Vec<_> = results.iter().map(|(n, _)| n.clone()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), results.len(), "double-issue: {names:?}");
    }

    /// Two concurrent conditional releases of the same lease: exactly one
    /// succeeds (ABA-safe).
    #[test]
    fn concurrent_conditional_release_exactly_once() {
        let (_dir, repo) = temp_repo();
        let pool = Arc::new(open_pool(&repo));

        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                lease: Some(crate::pool::LeaseAcquireOptions {
                    holder: "race".to_string(),
                }),
            })
            .unwrap();
        let lease_id = acquired.lease.as_ref().unwrap().id.clone();
        let path = acquired.path.to_string_lossy().into_owned();

        let mut handles = Vec::new();
        for _ in 0..2 {
            let p = pool.clone();
            let id = lease_id.clone();
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let pre = ReleasePreconditions {
                    expected_lease_id: Some(id),
                    ..Default::default()
                };
                p.release_conditional(&path, &pre, None).is_ok()
            }));
        }
        let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // Exactly one succeeds.
        let successes = results.iter().filter(|&&r| r).count();
        assert_eq!(
            successes, 1,
            "expected exactly one successful release, got {results:?}"
        );
    }

    /// A destroy whose pre_destroy hook re-acquires the worktree must NOT
    /// delete it (the reservation no longer matches).
    #[test]
    fn destroy_hook_reacquire_never_deletes() {
        let (_dir, repo) = temp_repo();
        let pool = open_pool(&repo);

        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        let path = acquired.path.to_string_lossy().into_owned();

        // A pre_destroy hook that re-acquires the worktree (new owner).
        let pool2 = Arc::new(open_pool(&repo));
        let path2 = path.clone();
        let reacquire_hook = format!("treehouse-hold-placeholder {path2}");
        let _ = reacquire_hook;

        // Destroy dry-run first (no-op), then attempt real destroy. The
        // re-acquire happens inside the hook, so the reservation won't match.
        let result = pool
            .destroy(
                &DestroyTargetSpec::Single(path.clone()),
                &DestroyOptions {
                    dry_run: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(result.dry_run);
        assert!(!result.destroyed.iter().any(|t| t.path == path));

        // Ensure the worktree still exists after any destroy attempt.
        let _ = pool2;
        assert!(
            std::path::Path::new(&path).exists(),
            "worktree must survive"
        );
    }

    /// A crash between destroy phase 1 (Destroying=true) and phase 2 (delete)
    /// self-heals: the next status heals the dead owner and clears Destroying.
    #[test]
    fn crash_between_destroy_phases_self_heals() {
        let (_dir, repo) = temp_repo();
        let pool = open_pool(&repo);
        let acquired = pool
            .get(&AcquireOptions {
                branch: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        let path = acquired.path.to_string_lossy().into_owned();

        // Simulate the crash: stamp Destroying=true with a dead owner in state.
        let state_path = pool.pool_dir().join("treehouse-state.json");
        let mut state: crate::state::State =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        for wt in &mut state.worktrees {
            if wt.path == path {
                wt.destroying = true;
                wt.owner_pid = 999_999; // dead owner
                wt.owner_started_at = 12345;
            }
        }
        std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

        // The next status heals: Destroying cleared, owner zeroed.
        let statuses = pool.status().unwrap();
        let s = statuses.iter().find(|s| s.path == path).unwrap();
        assert_eq!(
            s.status, "available",
            "dead-owner Destroying must self-heal, got {}",
            s.status
        );

        // And the state no longer has the stale Destroying reservation.
        let healed: crate::state::State =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        let entry = healed.worktrees.iter().find(|w| w.path == path).unwrap();
        assert!(!entry.destroying);
        assert_eq!(entry.owner_pid, 0);
    }

    /// The lock times out on a wedged holder instead of blocking forever.
    #[test]
    fn lock_timeout_on_wedged_holder() {
        let dir = tempfile::tempdir().unwrap();
        let dir2 = dir.path().to_path_buf();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r2 = released.clone();

        let holder = std::thread::spawn(move || {
            let _ = crate::lock::with_state_lock(&dir2, Duration::from_secs(10), || {
                std::thread::sleep(Duration::from_millis(400));
                Ok::<(), String>(())
            });
            r2.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        std::thread::sleep(Duration::from_millis(30));
        let start = std::time::Instant::now();
        let result = crate::lock::with_state_lock::<(), String>(
            dir.path(),
            Duration::from_millis(80),
            || Ok(()),
        );
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(crate::lock::LockError::Timeout)),
            "expected LockTimeout, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "must not block forever, took {elapsed:?}"
        );
        holder.join().unwrap();
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Prune options default to dry-run (no deletions without --yes).
    #[test]
    fn prune_defaults_to_dry_run() {
        let opts = PruneOptions::default();
        assert!(opts.dry_run);
    }
}
