# MISSING.md — Gap audit: Rust port vs Go upstream

**Ngày audit:** 2026-10-01
**Đối tượng:** `treehouse_rust` @ `main` (v0.1.2, commit `c3e587c`)
**Baseline đối chiếu:** `kunchenguid/treehouse` @ v3.1.0 (commit `cb7fb26`), clone tại `/tmp/th-baseline/treehouse`
**Baseline mà port plan tự nhận:** Go **v2.1.1** (`docs/rust-port-plan.md:5`)

---

## 0. Đọc file này

Audit chạy bằng workflow 67 agent: 6 agent theo domain đọc song song cả hai repo, sau đó **mọi finding đều qua 2 lens đối kháng** (một lens kiểm tra tồn tại, một lens kiểm tra impact) trước khi giữ lại.

- **29 finding đã verify** (4 CRITICAL / 11 HIGH / 14 MEDIUM) — phần chính của file này.
- **1 finding bị refute** — xem §7.
- **54 lead chưa verify** — severity thấp hơi ngưỡng, xem §8. Coi là **gợi ý, không phải kết luận**.

**Bucket** phân loại gốc rễ:

| Bucket | Nghĩa | Ai chịu trách nhiệm |
|---|---|---|
| `P0_PARITY` | Go baseline v2.1.1 **đã có**, Rust không tương đương | Port bug — đây là hợp đồng của chính plan |
| `UPSTREAM_DRIFT` | Upstream thêm **sau** v2.1.1, Rust chưa có | Lag hợp lệ, không phải bug |
| `BUG_RUST` | Rust có khái niệm nhưng implement sai | Bug port |
| `P1_UPGRADE` | Chính `docs/rust-port-plan.md` đã hứa | Nợ triển khai |

**ID** `M-0NN` là số thứ tự nên làm. Thứ tự trong file = thứ tự đề xuất fix.

---

## 1. Verdict

Port **đúng ở pool core**: layout thư mục pool, `short_hash`, `next_name`, `owner_alive` chống PID-reuse, `ClassSet`/`missing_flags`, two-phase destroy reservation + `restore_original`, và lock 10s (`lock.rs:25,61-74` — đã deliver đúng promise P1-A của plan). **Phần này đừng đụng vào.**

Nhưng port **fail-open đúng ở những chỗ nguy hiểm nhất**. Điểm nặng nhất: `confirm()` trả `true` khi stdin EOF, nên `treehouse return <p> < /dev/null` **hard-reset và xoá sạch thay đổi chưa commit, exit 0** — trong khi Go từ chối.

**Port đã trễ 6 release** (v2.2.0 → v3.1.0), trong đó v3.0.0 chiếm 12/16 mục drift. Nhưng các mục drift **không độc lập**: 6 phụ thuộc cùng một thứ là **VCS seam** mà Go đã tách ra. Đó là **một lần viết lại, không phải sáu gap** — xem M-028.

**Cái đáng sửa đầu tiên:** M-001.

---

## 2. Bảng triage

| ID | Sev | Bucket | Vấn đề |
|---|---|---|---|
| M-001 | CRITICAL | P0_PARITY | Make the dirty-return confirmation fail closed on stdin EOF instead of defaulting to yes |
| M-002 | CRITICAL | UPSTREAM_DRIFT | Refuse to reset/detach a pool slot that has lost its .git marker |
| M-003 | HIGH | BUG_RUST | Refresh the process table before every scan; never trust a pool-lifetime snapshot for safety decisions |
| M-004 | HIGH | BUG_RUST | Re-scan with a live process table after termination and fail closed when the scan errors |
| M-005 | HIGH | BUG_RUST | Stop swallowing unknown subcommands into `run`, and release the lease when child spawn fails |
| M-006 | HIGH | P0_PARITY | Reject `destroy --all` with no pool path instead of silently sweeping the current repository's pool |
| M-007 | HIGH | P0_PARITY | A treehouse.toml that omits `max_trees` is a hard parse error instead of keeping the default 16 |
| M-008 | HIGH | BUG_RUST | Global `--env-path` flag is a no-op — the pool still lands in `~/.treehouse` |
| M-009 | HIGH | P0_PARITY | prune's execute phase re-verifies only the reservation, never re-classifies, so a slot that dirtied between plan and delete is destroyed |
| M-010 | HIGH | P0_PARITY | No parent-directory fsync after rename — a crash can lose the state file entirely on ext4/XFS |
| M-011 | MEDIUM | P0_PARITY | Return phase-1 skips from execute_destroy instead of dropping them |
| M-012 | CRITICAL | UPSTREAM_DRIFT | Pool slot reuse has no clone-identity scoping, so a second clone of the same origin gets another clone's worktree reset |
| M-013 | CRITICAL | UPSTREAM_DRIFT | acquire reclaims a clean slot holding unlanded commits without any merge/HEAD safety check |
| M-014 | HIGH | UPSTREAM_DRIFT | return/get never terminate lingering processes, so a slot is reset while a writer is still in it |
| M-015 | HIGH | UPSTREAM_DRIFT | No `recoverMissingStateEntries`: a missing state file leaves on-disk worktrees invisible, so `next_name` reissues an occupied slot name |
| M-016 | MEDIUM | UPSTREAM_DRIFT | Resolve `return <name>` as a slot name, not only as a filesystem path |
| M-017 | MEDIUM | UPSTREAM_DRIFT | Add `return --all` to reclaim every held worktree in a pool |
| M-018 | MEDIUM | P1_UPGRADE | Honor `--format json|toon` on destroy/prune/return/gc instead of silently printing human text |
| M-019 | MEDIUM | UPSTREAM_DRIFT | Emit exit status 3 when a dirty worktree is left unreturned (`get` and `return`) |
| M-020 | MEDIUM | UPSTREAM_DRIFT | Prune and destroy resolve one repo root for the whole pool instead of per-worktree, so a shared pool corrupts other clones' worktrees |
| M-021 | MEDIUM | UPSTREAM_DRIFT | acquire has no markerless-slot fail-closed: a slot whose .git is gone is read through the enclosing repository |
| M-022 | MEDIUM | UPSTREAM_DRIFT | Corrupt-state recovery aborts the entire pool on ONE unreadable slot, instead of recovering the healthy slots around it |
| M-023 | MEDIUM | UPSTREAM_DRIFT | Restore the global `--root` flag and the `TREEHOUSE_ROOT` environment variable |
| M-024 | MEDIUM | UPSTREAM_DRIFT | Restore the seven acquisition flags on `get` and the matching AcquireOptions fields |
| M-025 | MEDIUM | P0_PARITY | No background update check is ever spawned, so the cache is never written and the notice never fires |
| M-026 | MEDIUM | UPSTREAM_DRIFT | Implement the RequireOwnedByCaller / RequireUnleased / RefuseRecovered release preconditions |
| M-027 | HIGH | P0_PARITY | `treehouse update` never downloads or replaces the binary but reports success |
| M-028 | ARCH | — | **VCS seam** — tách backend, mở đường cho jj / APFS / seeding / unique-leaf |

**Quy mô:** 4 CRITICAL · 11 HIGH · 14 MEDIUM — theo bucket: 8 `P0_PARITY`, 4 `BUG_RUST`, 16 `UPSTREAM_DRIFT`, 1 `P1_UPGRADE`.

---

## 3. ⚠️ Cần bạn quyết trước khi ai sửa

Ba mục dưới đây **làm đổi hành vi quan sát được** của command đã release. Không phải bug — nhưng cũng không phải fix an toàn mặc định.

**M-019 — exit code 3 khi worktree dirty chưa được return.**
Go ship nó là BREAKING CHANGE ở v3.0.0 với lý do: *"Callers that read exit 0 as 'the slot was released' were reading the bug this fixes; scripts under `set -e`, or using `treehouse return "$p" && next-step`, will now stop or branch differently at a dirty abort."*
Lưu ý mâu thuẫn: `docs/rust-port-plan.md` Appendix B đang ghi **exit 0 là chuẩn**. Làm M-019 nghĩa là **override chính contract của plan**.
→ Đề xuất: release-note entry + `TREEHOUSE_EXIT_STRICT=1` opt-in trong một minor.

**M-018 — wiring `--format json|toon` cho destroy/prune/return/gc.**
Hiện tại `--format json` trên các lệnh đó **parse được nhưng in ra prose**. Wiring nó sẽ đổi output cho bất kỳ ai đang truyền flag. Rủi ro thấp (không ai parse prose), nhưng vẫn là behavior change.
→ Hoặc wire thật, hoặc bỏ `global = true` để tổ hợp không hỗ trợ **fail loud**.

**M-012 — clone-identity scoping.**
Pool key theo hash của `remote_url`, root mặc định `$HOME/.treehouse` ⇒ **hai clone cùng origin dùng chung pool**. Đây là điều khiến các script chạy song song ở nhiều clone trở nên nguy hiểm. Nhưng nếu chưa ai dùng multi-clone thì có thể defer.

---

## 4. Tier 0 — vá nhỏ, không cần quyết định thiết kế

> Không dependency mới, không đổi hợp đồng CLI. 12 patch, mỗi cái vài dòng.

### M-001 — Make the dirty-return confirmation fail closed on stdin EOF instead of defaulting to yes

| | |
|---|---|
| **Severity** | CRITICAL |
| **Bucket** | **P0_PARITY** — port bug: Go baseline v2.1.1 đã có, Rust không tương đương |
| **Domain** | CLI surface — output routing / confirmation prompts |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> Verified by running the built binary: a worktree with `M a.txt` holding `PRECIOUS UNCOMMITTED DATA` was hard-reset and returned by `treehouse return <path> < /dev/null` with EXIT=0; `git status --porcelain` came back empty and a.txt reverted to `hi`. Every non-TTY caller — CI, a Makefile, a hook, an agent script, a cron job — that runs `treehouse return "$p"` on a dirty worktree silently destroys uncommitted work and reports success. Go refuses, leaves the slot dirty, and exits 3 telling the caller to use `--force`. This is unrecoverable data loss on the tool's single most destructive happy path.

**Rust — hiện trạng.**

> crates/treehouse/src/main.rs:721-728 confirm(): `std::io::stdin().read_line(&mut input)?; let input = input.trim().to_lowercase(); Ok(input.is_empty() || input == "y" || input == "yes")` — a 0-byte read at EOF leaves input empty and returns true, discarding the fact that it was an EOF. Consumed at main.rs:168 (get) and main.rs:227 (return).

**Go — tham chiếu.**

> cmd/return_cmd.go:399-418 confirmWorktreeReturn: `ok, err := ui.Confirm("Worktree has uncommitted changes. Clean and return?", true); if err != nil { return errReturnAbortedNonTTY }; if !ok { return errReturnAborted }` — the caller treats ANY read error as an abort, ignoring the bool. internal/ui/prompt.go:46-50: `line, err := promptReader().ReadString('\n'); input := strings.TrimSpace(...); if err != nil && input == "" { return defaultYes, err }` — the error is propagated, not swallowed.

**Cách sửa.**

> Change `confirm` to return `Result<Option<bool>>` (or add a distinct Err) and propagate io::Error from `read_line`; on `Err`/`Ok(0)` return `Err` so main.rs:227 takes the Go `errReturnAbortedNonTTY` branch (abort + exit 3 + `--force` hint). Keep empty-without-error (bare Enter) meaning "default yes".

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> VERIFIED — I attempted to refute and could not. Both citations are accurate, and the finding is real.
>
> GO SIDE CONFIRMED. /tmp/th-baseline/treehouse/cmd/return_cmd.go:399-418 matches the quoted confirmWorktreeReturn verbatim; internal/ui/prompt.go:46-50 matches verbatim. The caller checks `err != nil` BEFORE the bool, so any read error aborts and the bool is ignored.
>
> THE CLAIM IS STRONGER THAN STATED — it holds at v2.1.1, the port plan's declared baseline, which makes it P0_PARITY and not UPSTREAM_DRIFT. At v2.1.1 Go is STRICTER: `git show v2.1.1:internal/ui/prompt.go:19-22` propagates on ANY read error (`if err != nil { return defaultYes, err }`), and `git show v2.1.1:cmd/return_cmd.go:104-107` combines the checks (`if err != nil || !ok`). The v2.1.1 contract the plan claims to preserve already failed closed on EOF.
>
> RUST SIDE CONFIRMED, and the mechanism is worse than "discards the error." The `?` at main.rs:725 can NEVER fire for EOF. I compiled a standalone repro: at EOF `std::io::stdin().read_line()` returns Ok(0), NOT Err. So the `?` is dead code, `input` stays empty, and main.rs:727 evaluates `input.is_empty()` -> true -> Ok(true). Verified empirically, not from memory.
>
> NO GUARD UNDER ANY OTHER NAME. Grepped all of crates/ for eof|non.?tty|not a terminal|is_terminal|isatty|atty: zero hits. Exactly one stdin() call site exists in the entire workspace (main.rs:725), so no alternate read path exists. Both consumers confirmed at main.rs:168 (get) and main.rs:227 (return). No test covers confirm().
>
> IMPACT IS REAL DATA LOSS, not a wrong exit code. confirm()=true falls through to pool.release() -> release_conditional (pool.rs:357-359) -> reset_worktree (shell.rs:339-349), which runs `git checkout --detach --force`, `git reset --hard`, and `git clean -fd`. Uncommitted changes AND untracked files destroyed irreversibly. Go protects this with a dedicated sentinel (return_cmd.go:29), a distinct exit code ExitNotReturned, and an explicit "worktree not returned ... uncommitted changes were kept" message (return_cmd.go:120-122). Rust silently wipes and prints "Worktree returned to pool."
>
> BUCKET CORRECTION (BUG_RUST -> P0_PARITY): the v2.1.1 evidence above places this squarely in P0_PARITY ("behavior the Go v2.1.1 baseline already had; Rust never matched it"). The distinct errReturnAbortedNonTTY exit code and its dedicated message are a later v3.x refinement and would be separate, lower-severity drift — but the fail-closed-on-EOF behavior itself is baseline. Severity CRITICAL stands: irreversible destruction of uncommitted work with no confirmation and no warning.

</details>

---

### M-002 — Refuse to reset/detach a pool slot that has lost its .git marker

| | |
|---|---|
| **Severity** | CRITICAL |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | VCS / destructive-op safety |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> A pool slot whose `.git` marker was deleted, or which was never a real worktree, can have git walk up to the parent repository and hard-reset it. `reset_worktree` resolves `repo_root` via `rev-parse --show-toplevel` and, when that fails, silently falls back to `worktree.to_path_buf()` and proceeds with `checkout --detach --force` / `reset --hard` / `clean -fd` — no marker check at any point. Because in-project pools are a supported Rust configuration (config.rs:160-177 nests the pool under the repo when `root` is relative), the enclosing repository in that case is the user's actual working tree, and `reset --hard` + `clean -fd` there destroys uncommitted work. This is the highest-severity item in this domain. Note the guard is a v2.3.0 upstream fix rather than v2.1.1 behavior, so it is classified as drift — but the consequence in Rust is real today.

**Rust — hiện trạng.**

> ABSENT — `grep -rn "marker"` across crates returns 4 hits, none of them a guard (two are an unrelated stdout marker in main.rs:815-829, one is a doc comment, one is a state-recovery constant). `reset_worktree` (crates/treehouse-core/src/git/shell.rs:329-351) and `detach_worktree` (shell.rs:353-359) run `checkout`/`reset --hard`/`clean -fd` with cwd set to the worktree and no marker validation. `release_conditional` (crates/treehouse-core/src/pool.rs:323-377) calls `reset_worktree` at pool.rs:357-359 with only a lock-held reservation as protection.

**Go — tham chiếu.**

> vcs.go:513-525 `destructiveBackendForWorktree` returns `fmt.Errorf("refusing to modify %s: it holds no .git or .jj marker", path)` when `slotMarkerBackend` yields nil, with the rationale at vcs.go:513-519: falling back to the configured backend "in an in-project pool would resolve — and rewrite — the repository ENCLOSING the pool". Wrapped around every destructive entry point: `ResetWorktree` (vcs.go:531-537), `ResetWorktreeWithSeededPaths` (540-546), `ResetWorktreeToRef` (562-568), `ResetWorktreeToRefWithSeededPaths` (571-577), `DetachWorktree` (587-593). The commit that added it is c88b53e (v2.3.0, "markerless fail-closed hardening", #110).

**Cách sửa.**

> In `GitBackend::reset_worktree` and `detach_worktree` (or better, in a `Pool`-level guard mirroring Go's `destructiveBackendForWorktree`), `lstat` `<worktree>/.git` before running any destructive git command and return `GitErrorKind::Other` with Go's message when it is absent or its `gitdir:` target does not resolve. The check is a pure precondition and needs no new plumbing.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Could not refute; the gap is real and the citations are exact. GO: `destructiveBackendForWorktree` at vcs.go:520-525 emits the fail-closed error at vcs.go:524, backed by `slotMarkerBackend` (vcs.go:444-453) and `WorktreeBackendNameChecked` (vcs.go:459-480), and it wraps all five destructive entry points at exactly the cited lines (531-537, 540-546, 562-568, 571-577, 587-593). BUCKET UPSTREAM_DRIFT confirmed: `git show v2.1.1:internal/vcs/vcs.go` contains no `destructiveBackendForWorktree`/`refusing to modify`, so the guard postdates the plan's stated baseline (docs/rust-port-plan.md:5); c88b53e is listed at CHANGELOG.md:57 under `## [2.3.0]` (line 53) and `git tag --contains c88b53e` returns v2.3.0/v3.0.0/v3.0.1/v3.1.0. RUST: aggressive synonym grep across all of crates/ (`marker`, `slot_marker`, `flavor`, `worktree_backend`, `destructive`, `guard`, `refuse`, `jj` case-insensitive) yields no guard; `jj` returns literally nothing, so the port is git-only — but that does not neutralize the bug. `reset_worktree` (shell.rs:329-351) runs checkout/reset --hard/clean -fd with cwd = the worktree; `releasable_worktree` (pool.rs:598-614) checks only state membership, `destroying`, and lease identity; `heal_state` (state.rs:272-275) drops entries whose path vanished, so a markerless-but-present slot stays in the pool. SEVERITY CRITICAL confirmed and reachability is broader than the claim states: in-project pools are supported (config.rs:178 `repo_root.join(expanded).join(".treehouse")`), so `git` with cwd = markerless slot walks up to the ENCLOSING repo, where `reset --hard` discards uncommitted tracked changes and `clean -fd` deletes untracked files in the user's real repository — data loss per the stated rubric. The finder cited only pool.rs:357-359 (release); a second, more reachable site exists at pool.rs:221-223 (acquire step 3), reachable because `is_dirty` at pool.rs:490 also resolves to the enclosing repo and reports it clean, marking the slot available. Refutation attempts that failed: (a) destroy.rs:220 `backing_repository_missing` reads like a marker guard but returns false when `.git` is entirely absent (destroy.rs:222-224) — the opposite of fail-closed — and is a destroy-classification input, not a reset guard; (b) state.rs:232 is a marker check but only in the corrupt-state recovery scan; (c) Go's caller-level pre-gate `IsWorktreeSafeToReset` (vcs.go:582) has no Rust counterpart at all. Note also that Rust has no `reset_to_ref`/`seeded_paths` equivalents, so four of the five guarded Go entry points have no Rust function to guard in the first place.

</details>

---

### M-003 — Refresh the process table before every scan; never trust a pool-lifetime snapshot for safety decisions

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **BUG_RUST** — Rust có khái niệm nhưng implement sai so với Go |
| **Domain** | Safety invariants — process detection |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> Every in-use / owner-liveness decision in the port reads a process snapshot taken when `Pool::open` ran. Two distinct data-loss consequences: (1) DESTROY DELETES A LIVE WORKTREE — `classify_for_destroy` (destroy.rs:173-215) sees no processes for any PID that started after pool open, so a worktree with a running agent is classified `Disposable` and the two-phase protocol deletes it. (2) A LIVE OWNER'S RESERVATION IS SILENTLY HEALED AWAY — `owner_alive` returns false for any PID absent from the stale snapshot, so `heal_state` (called at destroy.rs:298, pool.rs:229, pool.rs:398) zeroes `owner_pid`/`owner_started_at` and clears `Destroying` on a worktree an agent is actively using, handing the slot to a concurrent acquire/destroy. Verified empirically: a table built before a child existed reported 0 processes in that child's cwd; the same table after `refresh()` reported 2. The port's own doc comment (process.rs:33-37) claims "re-enumerate on demand"; it never does.

**Rust — hiện trạng.**

> crates/treehouse-core/src/process.rs:43-62 (`ProcessTable::new()` performs the ONE and ONLY `refresh_processes_specifics`); crates/treehouse-core/src/process.rs:65-72 (`refresh()` exists but has ZERO callers — `grep -rn refresh crates/ | grep -v src/process.rs` → 0 hits); crates/treehouse-core/src/pool.rs:97 (`process: Arc<ProcessTable>` held for the Pool's lifetime), built once at crates/treehouse-core/src/pool.rs:127 and :157; every consumer reads the frozen snapshot: crates/treehouse-core/src/destroy.rs:174-178, crates/treehouse-core/src/destroy.rs:412-414, crates/treehouse-core/src/pool.rs:407-410; crates/treehouse-core/src/reservation.rs:151-159 (`owner_alive` → `process.started_at(pid)`) and crates/treehouse-core/src/state.rs:272-286 (`heal_state`)

**Go — tham chiếu.**

> internal/process/detect.go:45 (`procs, err := process.Processes()` — fresh enumeration on every call); internal/process/detect.go:33-40 (`StartedAt` → `process.NewProcess(pid)` + `proc.CreateTime()`, a live syscall per call, never cached)

**Cách sửa.**

> Call `self.process.refresh()` at the top of `find_in_worktree`, `started_at`, and `parent_pid` (or once at the head of `classify_for_destroy`, `status`, and each `execute_destroy` phase). Cheapest correct change: have `Pool::destroy`/`status` call `self.process.refresh()` before each classification sweep, since the table is per-Pool and each command does a bounded number of scans.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> CONFIRMED, severity downgraded CRITICAL -> HIGH, bucket BUG_RUST upheld.
>
> Both cited Go lines are accurate. internal/process/detect.go:45 does a fresh `process.Processes()` enumeration on every call, and detect.go:33-40 does a live `process.NewProcess(pid)` + `proc.CreateTime()` syscall per invocation. Go has NO process-table/caching object at all — every caller re-enumerates (internal/pool/destroy.go:309, internal/pool/prune.go:596,611,636, internal/pool/pool.go:502,1405,1429,1441, internal/process/terminate.go:23,52).
>
> Rust DID introduce a caching object and never refreshes it. ProcessTable::new() (process.rs:43-62) is the sole enumeration; refresh() (process.rs:65-72) is dead code — the token "refresh" appears NOWHERE in crates/ outside process.rs itself, and every production ProcessTable::new() is either pool construction (pool.rs:127, pool.rs:157) or inside #[cfg(test)] (destroy.rs:540, reservation.rs:189-283, process.rs:371-395). The Arc<ProcessTable> is held for the Pool's lifetime (pool.rs:97). So the core factual claim is correct.
>
> WHY NOT CRITICAL — the refutation: the finding implies a long-lived daemon acting on a permanently frozen table. That is not what happens. sweep_pools (discovery.rs:202-208) invokes ctx_factory PER POOL PER CYCLE, and sweep_all_pools (main.rs:395-435) defines that factory as Pool::open_at. Therefore `watch --interval` constructs a brand-new Pool — and a brand-new ProcessTable — on every single sweep cycle. The staleness window is one pool's sweep duration, not the daemon's lifetime. Go's TOCTOU window is microseconds (classify->remove); Rust's is one command duration. That is a materially widened race, not deterministic data loss, so CRITICAL is not supportable.
>
> WHAT I FOUND THAT IS STRONGER THAN THE CLAIM (keeps it real, at HIGH): the frozen table makes `destroy --include-in-use` deterministically fail, not merely racy. After terminate_with_grace (destroy.rs:392-394), the post-kill re-validation at destroy.rs:411-414 re-reads the SAME frozen snapshot, which still lists the just-killed pids, so `!is_empty()` is always true and destroy always skips with "worktree processes still running after termination". The identical defect neuters CleanupGuard::run() (run.rs:170-175): cmd_run opens the pool at main.rs:786 BEFORE the child is spawned, then blocks in wait_child (run.rs:86) for the child's entire (potentially hours-long) lifetime, so the termination step operates on a table enumerated before the child ever existed and can never see the agent or its descendants.
>
> BUCKET: BUG_RUST, not P0_PARITY. The distinguishing test is the fix shape — P0_PARITY means "add a capability the baseline had"; here refresh() already exists and is correctly implemented, only the wiring is absent, so the fix is to call self.refresh() before scans. Note also that `run` has no Go counterpart at all (Go has no cmd/run.go; no "run" command exists upstream), so the run-path manifestation is a Rust-only-feature defect rather than a parity gap.
>
> Confidence HIGH: I read both trees and grepped exhaustively for every synonym (refresh/refresh_all/refresh_processes/RefreshKind/sysinfo) across all of crates/.

</details>

---

### M-004 — Re-scan with a live process table after termination and fail closed when the scan errors

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **BUG_RUST** — Rust có khái niệm nhưng implement sai so với Go |
| **Domain** | Safety invariants — process termination |
| **Confidence** | HIGH |
| **Verify** | 1/2 lens đồng ý (đã sửa bucket) |

**Triệu chứng / tác động.**

> Two compounding failures on the only gate between "we killed the processes" and "we delete the directory". (a) Because the re-scan re-reads the pre-kill snapshot, the just-SIGKILLed PIDs are still present, so `--include-in-use` destroy can NEVER remove a worktree that actually had processes — it always skips with "worktree processes still running after termination". The opt-in flag is silently a no-op. (b) If the scan returns Err, `unwrap_or_default()` yields `vec![]`, `is_empty()` is true, and the code proceeds straight to `worktree_remove` + `remove_dir_all` — deleting a worktree whose liveness could not be established. Go refuses in exactly that case.

**Rust — hiện trạng.**

> crates/treehouse-core/src/destroy.rs:396-399 (`terminate_with_grace` scans and kills) followed by crates/treehouse-core/src/destroy.rs:412-417 (`find_in_worktree(...).unwrap_or_default().is_empty()` — the SAME frozen snapshot; no `refresh()` anywhere in between). The `unwrap_or_default()` also swallows a scan error into an empty vec, i.e. FAIL-OPEN where Go fails closed.

**Go — tham chiếu.**

> internal/pool/destroy.go:504-510 (`findProcessesInWorktree` is a FRESH gopsutil enumeration; on error it restores the reservation, sets Detail "could not verify worktree processes stopped", and SKIPS — never deletes); internal/pool/destroy.go:511-517 (non-empty survivors ⇒ restore + skip)

**Cách sửa.**

> Refresh the table immediately before the survivor re-scan, and replace `.unwrap_or_default().is_empty()` with a `match` that restores the reservation and skips on `Err` — mirroring destroy.go:504-510. Fixing the table staleness (finding 1) resolves (a); (b) needs the explicit error branch.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> CONFIRMED on both sides; only the severity label is overstated.
>
> VERIFIED GO: destroy.go:498 calls terminateWorktreeProcesses, then :504 calls findProcessesInWorktree(path); on error it restores the reservation, sets Detail "could not verify worktree processes stopped" (:506) and skips — never deletes. :511 non-empty survivors likewise restore + skip. That function is process.FindProcessesInWorktree (destroy.go:52), which calls process.Processes() at detect.go:44-46 — a FRESH gopsutil enumeration per call. I confirmed via `git show v2.1.1:internal/pool/destroy.go` that this block existed at lines 451-460 in the stated v2.1.1 baseline, so it is NOT upstream drift.
>
> VERIFIED RUST: destroy.rs:396-399 terminates, :412-417 re-checks with find_in_worktree(...).unwrap_or_default().is_empty(). Both calls route into ProcessTable::find_in_worktree (process.rs:77-112), which iterates self.system — a Mutex<System> populated once in ProcessTable::new() (process.rs:51-61).
>
> DECISIVE PROOF: ProcessTable::refresh() is defined at process.rs:65 and has ZERO callers — `grep -rn "\.refresh()"` across crates/ and tests/ returns nothing, so it is dead code. sysinfo prunes exited processes only inside refresh_processes_specifics(..., remove_dead_processes=true, ...) (system.rs:377-378), so SIGKILLed PIDs persist in the cached map forever. The re-scan provably reads the same frozen snapshot where Go re-enumerates. The safety invariant "verify processes stopped before deleting the worktree" is genuinely non-functional.
>
> CORRECTION TO THE CLAIM'S REASONING: the unwrap_or_default() fail-open is real in code shape but near-unreachable — find_in_worktree returns Err only when absolute_and_resolve returns None (process.rs:78-79), which requires std::env::current_dir() to fail on a RELATIVE path (process.rs:259-268); state paths are absolute. The dominant defect is the frozen snapshot, not the error swallow.
>
> WHY HIGH, NOT CRITICAL: the deterministic outcome of the stale snapshot is FAIL-SAFE, not destructive. Processes alive at pool-open are in the map, get SIGKILLed, still remain in the stale map, so the survivors check always trips and destroy --include-in-use always skips — the feature is broken but never deletes the wrong worktree. The data-loss direction (a process started AFTER ProcessTable::new(), invisible to every Rust scan, followed by remove_dir_all_guarded at destroy.rs:436) requires a race, most plausibly a pre-destroy hook spawning a background process; that hook runs outside the lock at destroy.rs:338-350, after the snapshot. A wrong-worktree-deleted path therefore exists but is a narrow race, not the common case — "safety invariant missing / core command broken" = HIGH.

</details>

---

### M-005 — Stop swallowing unknown subcommands into `run`, and release the lease when child spawn fails

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **BUG_RUST** — Rust có khái niệm nhưng implement sai so với Go |
| **Domain** | CLI surface — command dispatch |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> Verified: `treehouse statuss` printed `io: spawning command "statuss"` and exited 1 — but slot 4 had already been acquired and durably leased with holder `run:30130`, and `treehouse status` still showed `4 leased (held by run:30130)`. Same for `treehouse lease 1` (the unported Go subcommand): it leased slot 1 then failed to spawn. Every typo or not-yet-ported verb permanently burns a pool slot (24h TTL, never reclaimable while the lease is valid, and `get` refuses to hand out a leased slot), so a handful of mistakes starves a 16-slot pool with no way to see why beyond `status`.

**Rust — hiện trạng.**

> crates/treehouse/src/cli.rs:63-65 `#[command(external_subcommand)] Run(Vec<String>)` — clap routes ANY unmatched subcommand (including a bare typo) into `Run`. crates/treehouse/src/main.rs:740-807 cmd_run acquires a lease, then crates/treehouse-core/src/run.rs:65-68 does `pool.get(&AcquireOptions{ lease: Some(..), .. })` and run.rs:75 does `let child = spawn_child(...)?;` — the `?` returns BEFORE the `CleanupGuard` is constructed at run.rs:78, so the just-taken lease has no owner to release it.

**Go — tham chiếu.**

> cmd/root.go:24-58 declares rootCmd and `init()` registers exactly nine commands (get.go:105, enter.go:39, return_cmd.go:144, status.go:155, prune.go:104, destroy.go:70, lease.go:45, init.go:67, update.go:42). There is no `external_subcommand`/catch-all anywhere in cmd/*.go, so cobra rejects an unknown verb with `unknown command "statuss" for "treehouse"` and acquires nothing.

**Cách sửa.**

> Two changes. (1) In run.rs, construct the CleanupGuard immediately after the acquire, before spawn_child, so a spawn failure unwinds through Drop. (2) Replace `external_subcommand` with a real `run` subcommand carrying `#[arg(last = true)] cmd: Vec<String>` (or `trailing_var_arg`), so clap errors on unknown verbs the way cobra does — and restore the Go-rejection path rather than silently leasing.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Could not refute — both halves verified by reading and by running both binaries. (1) cli.rs:64-65 attaches #[command(excommand)]-style clap external_subcommand to Run(Vec<String>), so clap routes any unmatched verb into cmd_run. Rust's own --help omits `run` entirely, proving it is only a catch-all. Built the Go baseline (cb7fb26) and ran both on the same input: Go prints `unknown command "statuss" for "treehouse"` + `Did you mean this? status` and acquires nothing (cobra legacyArgs, no catch-all among the nine AddCommand registrations); Rust prints `io: spawning command "statuss"` after having already acquired a worktree. (2) run.rs:65 takes the lease, run.rs:75 does `spawn_child(...)?` which returns BEFORE CleanupGuard is constructed at run.rs:78. I captured the resulting on-disk state: leased:true, lease_holder:"run:71664" (dead pid), expires_at +24h — a leaked worktree + lease. This breaks the file's own documented invariant (run.rs:2-8 "cleaned up on EVERY exit path") and plan section 6.5 / P1-D "cleanup ALWAYS". Correcting severity from CRITICAL to HIGH: the CRITICAL band is data loss / wrong worktree deleted / silent corruption / security; none apply. Cleanup never runs, so nothing is deleted or corrupted, and the lease is TTL-bounded and gc-reclaimable (plan 6.5: "a lease can never block a pool slot longer than TTL + next gc sweep"). This is a missing safety invariant plus a wrong exit path on a typo — the HIGH band. Minor citation drift: the finder cited status.go:155 and return_cmd.go:144; the actual AddCommand lines are status.go:154 and return_cmd.go:143.

</details>

---

### M-006 — Reject `destroy --all` with no pool path instead of silently sweeping the current repository's pool

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **P0_PARITY** — port bug: Go baseline v2.1.1 đã có, Rust không tương đương |
| **Domain** | CLI surface — safety guards |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> `treehouse destroy --all --yes` run from any repository now wipes that repository's entire pool. In Go this is a hard error that forces the operator to name the pool, which is the last stop before a mass deletion. Removing it removes the only confirmation that the target is deliberate — a muscle-memory `destroy --all --yes` from the wrong directory deletes the wrong pool.

**Rust — hiện trạng.**

> crates/treehouse/src/main.rs:579-583 `let spec = if all { DestroyTargetSpec::All } else { DestroyTargetSpec::Single(args.path.clone().unwrap()) };` — `args.path` is `Option<String>` (cli.rs:202) and `All` is built with no pool argument; main.rs:571-577 only rejects `--all` when a path IS given and looks like a worktree. Verified: `treehouse destroy --all` printed a dry-run enumeration of `/Users/…/.treehouse/th-probe2-0b9ac1` and exited 0 with no path argument.

**Go — tham chiếu.**

> cmd/destroy.go:39 documents the invariant "There is no cross-pool or global destroy; --all without a pool path is an error", and destroy.go:81-83 enforces it: `if destroyAll { if len(args) == 0 { return errors.New("--all requires a pool path; name the pool to clear, e.g. 'treehouse destroy . --all'") } … }`.

**Cách sửa.**

> Make `path` required when `all` is set: `if all && args.path.is_none() { return Err(anyhow!("--all requires a pool path; name the pool to clear, e.g. 'treehouse destroy . --all'")) }`, and resolve the named pool through an `is_pool_dir` check mirroring Go's `resolveDestroyPoolFromTarget` (destroy.go:190-211) rather than the current string-suffix heuristic at main.rs:571-574.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Could not refute — every citation checks out and the behavior reproduces. Go v3.1.0 cmd/destroy.go:39 documents the invariant and cmd/destroy.go:81-83 enforces it; I also verified via `git show v2.1.1:cmd/destroy.go` that the identical guard existed at v2.1.1 line 93, so this predates the port baseline and is NOT upstream drift. Rust main.rs:571-577 only guards the case where a path IS supplied ("--all takes a pool path, not a worktree path"); nothing rejects the no-path case, and main.rs:579-583 builds DestroyTargetSpec::All from args.path (Option<String>, cli.rs:202) without ever checking None. Grepping crates/ for "requires a pool", "pool path", "cross-pool", "global destroy" yields only comments (treehouse-core/src/destroy.rs:5, main.rs:567) — no enforcing code. Empirically reproduced in a fresh temp repo with one pooled worktree: `treehouse destroy --all` printed "Dry run: would destroy 1 worktree(s) in .../.treehouse/repo-c32f61" and exited 0. Two pieces of evidence the finder missed that strengthen it: (1) docs/rust-port-plan.md:55 states the same invariant as the design contract ("No cross-pool/global destroy exists; `destroy --all` without a pool path is an error"), so Rust violates its own plan; (2) the comment at main.rs:567-568 ("Go `<pool> --all` and `--all` from the repo") is a misreading — Go has no bare --all mode. Separately, bare `treehouse destroy` with no path and no --all panics at main.rs:582 (`Option::unwrap()` on None, exit 101) where Go returns a clean error. BUCKET CORRECTED BUG_RUST -> P0_PARITY: the rubric's defining test is behavior the Go v2.1.1 baseline already had and Rust never matched — Rust has no partial/misfiring version of THIS guard to call "wrongly implemented"; the guard it does have is a different check. SEVERITY CONFIRMED HIGH, not CRITICAL: destroy still requires explicit --yes and still honors the disposable/include-*/never-lease-by-bulk gates (treehouse-core/src/destroy.rs:101,122), so no silent corruption or unflagged data loss — but a documented safety invariant is missing and `destroy --all --yes` wipes the entire ambient pool with no pool ever named.

</details>

---

### M-007 — A treehouse.toml that omits `max_trees` is a hard parse error instead of keeping the default 16

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **P0_PARITY** — port bug: Go baseline v2.1.1 đã có, Rust không tương đương |
| **Domain** | config |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> Verified empirically: a repo treehouse.toml containing only `root = "pools"` yields `failed to parse config file .../treehouse.toml: TOML parse error ... missing field 'max_trees'`. Go loads it fine with max_trees=16. Every treehouse command in that repo then fails at config load — status, get, prune, destroy — because resolve_repo_ctx propagates the error (cli.rs:224). The same applies to ~/.config/treehouse/config.toml containing only a [hooks] table, which is the documented shape for hook-only user config.

**Rust — hiện trạng.**

> crates/treehouse-core/src/config.rs:23 `pub max_trees: u32` has no #[serde(default)], and :76 / :120 / :307 / :325 all do `toml::from_str(&text)` into a FRESH TreehouseConfig. The Rust test at config.rs:611-615 even carries the workaround comment "must include max_trees for TOML parse" — the bug was worked around in the test rather than fixed in the loader.

**Go — tham chiếu.**

> internal/config/config.go:157 `cfg := DefaultConfig()` (MaxTrees 16, config.go:97-101) then :163 `toml.DecodeFile(repoPath, &cfg)` decodes ONTO the pre-populated struct, so a key absent from the document keeps its default. Same shape for the user config at :205/:211 (loadUser).

**Cách sửa.**

> Add #[serde(default = "default_max_trees")] returning DEFAULT_MAX_TREES, or decode into a toml::Value and apply fields one at a time so absent keys leave the defaulted struct untouched — the latter matches Go's decode-onto-default semantics exactly and protects every future optional key too.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Could not refute — the gap is real and every citation is accurate. I verified BOTH sides by compiling and executing probes, not by reading alone.
>
> GO SIDE (ground truth confirmed): internal/config/config.go:97-101 DefaultConfig() returns MaxTrees:16; :157 `cfg := DefaultConfig()` then :163 `toml.DecodeFile(repoPath, &cfg)` decodes ONTO the pre-populated struct, so an absent key retains its default. Same for user config at :205/:211 (loadUser). I ran `go test ./internal/config/` with a probe writing a treehouse.toml of only `root = "~/wt"` → output `PROBE_OK max_trees=16 root="~/wt"`. Critically, `git show v2.1.1:internal/config/config.go` shows the IDENTICAL DefaultConfig+DecodeFile pattern (v2.1.1 lines 23-40), so this is v2.1.1 baseline behavior, not post-baseline upstream drift.
>
> RUST SIDE (confirmed): config.rs:23 `pub max_trees: u32` has NO #[serde(default)], while sibling fields at :24, :26, :29 all DO have #[serde(default)] — the omission is specific to this one field, not a struct-level decision. All four decode sites (:76, :120, :307, :325) call `toml::from_str::<TreehouseConfig>` into a FRESH struct, discarding the default_config() established at :69/:111. I compiled and ran a probe: `missing field 'max_trees'` hard parse error.
>
> REACHABILITY (not theoretical): the upstream Go README lines 720-723 documents a `[hooks]`-only user config containing NO max_trees — exactly what users are told to paste into ~/.config/treehouse/config.toml. I tested that literal snippet against Rust's load_with_env and it hard-fails; a repo treehouse.toml with only `root = "."` (Go's documented in-project storage) also hard-fails. Since TreehouseConfig::load runs on every repo-scoped command, one such file breaks the entire tool.
>
> PROCESS NOTE: my first user-config probe was invalid — I mistakenly called the real TreehouseConfig::load (reads actual $HOME) instead of load_with_env, yielding a misleading max_trees=64. I discarded it and re-ran correctly; the corrected probe reproduces the error. Both probe files were removed; git status is clean on both trees.
>
> The config.rs:611 comment "must include max_trees for TOML parse" confirms the claimed workaround-in-tests-instead-of-fix pattern. BUCKET P0_PARITY correct (v2.1.1 had it; not upstream drift, not a P1 plan item — the plan's config.rs line only specifies the file exists, not serde defaults). SEVERITY HIGH correct: fails loudly with no data loss, silent corruption, or security impact (so not CRITICAL), but it breaks every command in an affected repo, clearing the HIGH bar.

</details>

---

### M-008 — Global `--env-path` flag is a no-op — the pool still lands in `~/.treehouse`

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **BUG_RUST** — Rust có khái niệm nhưng implement sai so với Go |
| **Domain** | config |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> README.md:225 and :244 document `--env-path <DIR>` as "Custom pool root (overrides treehouse.toml root and ~/.treehouse); available on every command". It overrides nothing. Verified empirically: `treehouse --env-path $HOME/CUSTOMPOOL get --lease` printed a path under $HOME/.treehouse/repo-<hash>/1/repo and $HOME/CUSTOMPOOL was never created. Anyone using it for per-project or CI-isolated pools silently shares the default pool, so two checkouts of the same repo collide in one slot namespace.

**Rust — hiện trạng.**

> crates/treehouse/src/cli.rs:31-32 declares the flag; main.rs:67-70 routes to open_pool_with_env_path; cli.rs:246-315 builds a CliEnv { pool_root: env_path } and passes it to Pool::open_with_env; but pool.rs:121 inside that function calls resolve_pool_dir(...) — the NON-_with_env variant — which reads opts.config.root or home_dir() from process env. TreehouseEnv::pool_root() is never consulted. Grep confirms resolve_pool_dir_with_env (config.rs:235) and resolve_pool_root_with_env (config.rs:260) have no production callers, only unit tests and the custom_env example.

**Go — tham chiếu.**

> internal/config/config.go:103-118 ResolveRoot(flag, cfg) — flag > TREEHOUSE_ROOT > config.root; config.go:252-269 ResolvePoolRoot then resolves it. Locked by resolve_test.go:119 TestResolveRoot_Precedence and :170 TestResolveRoot_FlagDotSelectsInProject.

**Cách sửa.**

> In pool.rs:121 call resolve_pool_dir_with_env(repo_root, opts.config.root.as_deref(), remote_url, env.as_ref()) so the injected env is honored, and in main.rs:67 pass --env-path as the root override ahead of config.root (mirroring Go's flag > env > config precedence) rather than as a separate env channel.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> I tried to refute this and could not; I confirmed it empirically, not just by reading. `Pool::open_with_env` (pool.rs:108-118) accepts the injected `CliEnv { pool_root: env_path }` from cli.rs:302-303 but then calls the NON-env resolver `resolve_pool_dir(repo_root, opts.config.root.as_deref(), remote_url)` at pool.rs:121. pool.rs:20 imports only `resolve_pool_dir` — the `_with_env` variant is never imported into the module. The correct functions exist and are correct (`config.rs:235 resolve_pool_dir_with_env`, `config.rs:260 resolve_pool_root_with_env`, where line 267 is exactly `env.pool_root()`), but grep proves they are dead in production: the only references are unit tests at config.rs:552-585 and the custom_env/in_memory_env examples. No `pool_root()` consumer sits in the Pool::open path — discovery.rs:47/59 and lib.rs:93 serve the `--all` sweep and test harness respectively.
>
> EMPIRICAL PROOF: I built the CLI and ran it against a throwaway git repo with HOME redirected to a fresh temp dir:
>   env HOME=/tmp/th-home2.z3ETcz treehouse --env-path /tmp/th-verify.p7oWdP/custom status
> The pool was created at /tmp/th-home2.z3ETcz/.treehouse/repo-9e4b59/ (with treehouse-state.json and .lock), and the --env-path target directory remained completely EMPTY. The flag changed nothing.
>
> BLAST RADIUS: all nine pool-touching commands (get, enter, return, status, prune, gc, destroy, run, doctor) route through main.rs:65-71 open_pool_for_cli — call sites at main.rs:107, 181, 211, 244, 267, 352, 565, 786, 812. The flag is documented as working at README.md:225 and README.md:244.
>
> BUCKET BUG_RUST is correct, not UPSTREAM_DRIFT or P1_UPGRADE. Go's equivalent is `--root` (different name, same concept) declared at cmd/root.go:62, threaded through config.ResolveRoot(rootFlag, cfg) into ResolvePoolDir at cmd/get.go:152, cmd/status.go:56, cmd/enter.go:55, cmd/lease.go:64, cmd/prune.go:51/79, cmd/destroy.go:210, cmd/return_cmd.go:564. The concept predates the v2.1.1 baseline, so this is a port bug.
>
> ONE CORRECTION to the claim's citation: ResolveRoot is at internal/config/config.go:110, not 103 — lines 103-109 are its doc comment. The rest of the Go citation checks out, including resolve_test.go:119 (TestResolveRoot_Precedence) and resolve_test.go:170 (TestResolveRoot_FlagDotSelectsInProject).
>
> TWO THINGS THE CLAIM UNDERSTATED: (1) TREEHOUSE_ROOT is absent from the entire Rust tree (grep -rn TREEHOUSE_ROOT crates/ returns no matches), so the middle tier of Go's precedence (flag > env > config > default) is missing entirely, not just the flag. (2) Severity stays HIGH rather than escalating to CRITICAL because there is a working escape hatch: config.root from treehouse.toml IS read at pool.rs:121, so a user can still isolate a pool. The failure mode is "documented flag silently ignored, falls back to the real ~/.treehouse pool" — not "wrong pool destroyed."

</details>

---

### M-009 — prune's execute phase re-verifies only the reservation, never re-classifies, so a slot that dirtied between plan and delete is destroyed

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **P0_PARITY** — port bug: Go baseline v2.1.1 đã có, Rust không tương đương |
| **Domain** | pool semantics — prune safety re-verification |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> A worktree that goes dirty (or gets a process standing in it) between the dry-run plan and the execute phase is deleted without ever being re-checked. The destroy engine DOES re-classify (destroy.rs:379-383), so the two engines disagree on "disposable" under exactly the race the plan's §2.2 two-phase contract is supposed to close. The pre-destroy hook running outside the lock widens that window arbitrarily.

**Rust — hiện trạng.**

> /Users/tranquangdang21/Projects/treehouse_rust/crates/treehouse-core/src/prune.rs:350-409 — phase 2 checks only `reservation.matches(&state.worktrees[idx])` (line 364) and then goes straight to `self.git.worktree_remove` (line 375) and `std::fs::remove_dir_all` (line 387). No `analyze_idle_worktree` re-invocation, no `is_dirty`, no `is_worktree_in_use`, no merge check.

**Go — tham chiếu.**

> /tmp/th-baseline/treehouse/internal/pool/prune.go:605-625 (finalPruneSafetyCheck: re-runs IsWorktreeInUse → analyzeIdleWorktree under the state lock immediately before the removal) and prune.go:626-651 (finalOrphanPruneSafetyCheck). Called from executePrune at prune.go:838-847. The full classification (dirty + merge into default ref + size) is deliberately recomputed inside the deleting lock, not trusted from the plan.

**Cách sửa.**

> Inside the phase-2 lock, after `reservation.matches`, re-run `analyze_idle_worktree` (or at minimum `is_dirty` + `is_worktree_in_use` + the merge check) against the freshly-read state; on any skip, call `reservation.restore_original` and push a `PruneSkipped` with the right category, exactly as Go's `finalPruneSafetyCheck` does.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> REFUTATION FAILED on all five angles. (1) Not wrong function: v2.1.1 baseline itself has it — `git show v2.1.1:internal/pool/prune.go` shows finalPruneSafetyCheck at :565, called at :482 inside the deleting WithStateLock, re-running IsWorktreeInUse (:569) + analyzeIdleWorktree (:579). This kills the UPSTREAM_DRIFT reading and fixes the bucket at P0_PARITY. (2) No alternate module: grepping the entire execute_prune body (prune.rs:295-418) for analyze_idle_worktree / is_dirty / is_worktree_in_use / is_head_merged returns NONE FOUND. The only phase-2 gate is reservation.matches at prune.rs:364, which validates identity (path + destroying + owner_pid + owner_started_at) not state. Dirty (prune.rs:205) and merge (prune.rs:233) run only in the plan phase, outside any lock (prune.rs:118-141). (3) No feature flag gates it. (4) The finder's Go call-site citation 838-847 is WRONG (that range is linkedWorktreeGitDir) and 605-625 is off by two (actual 607-625), but these are line-number slips — the named functions exist and are called from executePrune at prune.go:519/521, so the substance stands. (5) No defense-in-depth elsewhere — I found the gap is actually UNDERSTATED: Go's delete is vcs.RemoveCleanWorktree -> gitvcs.go:694-697 = `git worktree remove` WITHOUT --force, so git itself refuses a dirty worktree. Rust calls worktree_remove (prune.rs:375) = shell.rs:304-310 WITH --force. The non-forced remove_clean_worktree variant exists (shell.rs:312-318, trait git/mod.rs:95) but has ZERO call sites in the entire crate — dead code. Rust lost both the Go re-classification and the non-forced guard. Additionally the port plan itself (docs/rust-port-plan.md:56) explicitly promises phase 3 "re-verifies sameDestroyReservation ... re-classifies with live state, then deletes" — the first half is satisfied, the second is not. Severity held at HIGH (top of band) rather than CRITICAL: the outcome is permanent loss of uncommitted work, but it is gated on winning a race, since the plan phase runs outside all locks and pre_destroy hooks execute outside all locks (prune.rs:330-343), leaving the window open for the hook's full duration. That is a "safety invariant missing" per the rubric; a grader weighting "data loss" unconditionally would call it CRITICAL.

</details>

---

### M-010 — No parent-directory fsync after rename — a crash can lose the state file entirely on ext4/XFS

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **P0_PARITY** — port bug: Go baseline v2.1.1 đã có, Rust không tương đương |
| **Domain** | atomic write / durability |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> `rename(2)` is atomic with respect to readers but NOT with respect to a power loss: without fsyncing the parent directory, the rename may not be durable. After a hard crash / power cut the pool can come back with a zero-length or absent `treehouse-state.json`, which Rust then treats as CORRUPT and recovers every on-disk worktree as leased — or worse, the old directory entry reappears and the last write is silently lost (a lease or owner reservation is forgotten while a live process still holds the worktree). docs/rust-port-plan.md §2.3 spells the required dance out verbatim: "fsync → rename+dir-sync (POSIX) / ReplaceFileW/MoveFileEx MOVEFILE_WRITE_THROUGH (Windows)".

**Rust — hiện trạng.**

> crates/treehouse-core/src/state_file.rs:46-48 commits with `// Atomic commit: unix rename / Windows MoveFileExW+REPLACE.` then `tmp.persist(path).map_err(|e| e.error)?; Ok(())` — return immediately, no directory sync. Enumerated every fsync in the file: only :43 `tmp.as_file().sync_all()?` (the temp file itself). No `File::open(dir)` + `sync_all()` anywhere. The doc comment at :26-27 asserts "On Windows the parent-directory fsync is skipped (Go's syncDirectory is POSIX-only) — accepted for P0 parity" — that is self-contradictory: the code skips it on POSIX too, where Go has had it since v2.0.1.

**Go — tham chiếu.**

> /tmp/th-baseline/treehouse/internal/pool/state_file_posix.go:10-15 `func commitStateFile(tmpPath, path string, _ bool) error { if err := os.Rename(tmpPath, path); err != nil { return err }; return syncDirectory(filepath.Dir(path)) }`; :17-24 `func syncDirectory(dir string) error { f, err := os.Open(dir); ...; return f.Sync() }`. Identical in the v2.1.1 baseline (`git show 939cb59:internal/pool/state_file_posix.go`) — this is NOT drift, it is a P0 invariant the port never matched. Windows side: state_file_windows.go:32-46 `ReplaceFileW` with `replaceFileWriteThrough = 0x1`, and :58 `MoveFileEx(..., MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)`.

**Cách sửa.**

> After `tmp.persist(path)?`, add a `#[cfg(unix)]` block that opens `path.parent()` and calls `File::open(dir)?.sync_all()?`, propagating the error. Correct the misleading doc comment at state_file.rs:26-27 to say the gap is POSIX-wide, not Windows-only. On Windows, `tempfile`'s persist maps to MoveFileEx without MOVEFILE_WRITE_THROUGH, so add that flag explicitly for parity with state_file_windows.go:58.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Could not refute; every material citation verified against both trees.
>
> GO SIDE (confirmed): state_file_posix.go:10-15 commitStateFile (os.Rename then syncDirectory) and :17-24 syncDirectory (os.Open + f.Sync) match verbatim. Proved NOT drift by checking the actual v2.1.1 baseline: `git show 939cb59:internal/pool/state_file_posix.go` is byte-identical to HEAD cb7fb26. It is live code, not dead: state.go:493 calls commitStateFile from atomicWriteFile. Go's own doc comment at state.go:444-447 states the contract: "syncs it, commits it with the platform's replacement primitive, and syncs the parent directory where the platform supports that."
>
> RUST SIDE (confirmed absent; synonym sweep done): state_file.rs:47-48 persists then returns immediately. Tree-wide grep for sync_all|sync_data|fsync|sync_dir|syncDirectory|sync_parent|fdatasync|F_FULLFSYNC returns exactly ONE code hit: state_file.rs:43, the temp file. Separate sweep for directory File::open/OpenOptions and for helper crates (fs2, sync-all, SyncAllExt, dunce, same-file in both Cargo.toml) found nothing -- every OpenOptions hit is the pool options struct, not std::fs::OpenOptions. No alternate write path exists: all 12 production callers use write_state (the un-synced one) across pool.rs/gc.rs/destroy.rs/prune.rs/discovery.rs, and DefaultEnv::write_file at env.rs:122-127 is a bare std::fs::write with no sync at all.
>
> SELF-CONTRADICTION IS REAL: doc comment state_file.rs:26-27 frames the skip as Windows-only, but no #[cfg] guards any sync in the function -- the POSIX path returns at :48 identically.
>
> INDEPENDENT CORROBORATION THE FINDER DID NOT CITE: the port plan itself promises the dir sync. docs/rust-port-plan.md:60 ("rename+dir-sync (POSIX)") and :99 ("state_file.rs # atomic_write_file (tempfile persist + dir sync + mode preservation)"), listed as P0 at :561/:572 ("atomic+recoverable state"). So this is an unimplemented design-contract element, not a deliberate accepted deviation.
>
> SEVERITY: considered CRITICAL, rejected. A lost rename reverts the state file to a VALID prior version -- not corrupt/truncated -- so recover_corrupt_state (state.rs:201) never fires. And gc::analyze_gc_candidate (gc.rs:146) iterates &WorktreeEntry sourced from state, so untracked worktrees are invisible to gc and leak rather than being wrongly deleted. That keeps it out of the "wrong worktree deleted" tier. It is squarely the rubric's "safety invariant missing" and is durable in both Go and the port plan -- HIGH stands.
>
> Fix is ~6 lines mirroring syncDirectory, #[cfg(unix)]-gated, plus correcting the stale :26-27 comment (Windows already gets write-through via persist's MoveFileExW, so the skip is harmless only on Windows -- the inverse of what the comment claims).

</details>

---

### M-011 — Return phase-1 skips from execute_destroy instead of dropping them

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **P0_PARITY** — port bug: Go baseline v2.1.1 đã có, Rust không tương đương |
| **Domain** | Safety invariants — two-phase destroy |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> A worktree that flips to non-removable between the plan phase and phase 1 — because a concurrent destroy reserved it, or because it became dirty/in-use/leased — is neither destroyed nor reported. It vanishes from the command's output entirely. The user's `treehouse destroy --all` shows fewer targets than exist, with no indication that two were held back, so the skip is invisible and unactionable.

**Rust — hiện trạng.**

> crates/treehouse-core/src/destroy.rs:300 (`let mut skips = Vec::new();`) and crates/treehouse-core/src/destroy.rs:309-315 (pushes the "reserved by another destroy" skip) — but the closure returns `Ok::<_, PoolError>(reserved)` at crates/treehouse-core/src/destroy.rs:334, so `skips` is a dead local that is never propagated. crates/treehouse-core/src/destroy.rs:317-319 (`if self.allows(...).is_err() { continue; }`) likewise discards the skip Go records at v2.1.1 destroy.go:384.

**Go — tham chiếu.**

> v2.1.1 internal/pool/destroy.go:379 (`skips = append(skips, DestroySkip{Target: current})` for "reserved by another destroy") and v2.1.1 internal/pool/destroy.go:384 (`skips = append(skips, skip)` when `opts.allows` fails); both are returned by `executeDestroy` and merged into `DestroyResult.Skipped` (HEAD destroy.go:544, planAndDestroy destroy.go:245)

**Cách sửa.**

> Change the phase-1 closure to return `(reserved, skips)` instead of `reserved`, and extend them into `result.skipped` alongside `exec_skips` at destroy.rs:164-168. Also capture the `allows` error and push it, mirroring Go's line 384.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Could not refute — verified real on both sides. Go v2.1.1 internal/pool/destroy.go:376-380 appends a DestroySkip{Target: current} ("reserved by another destroy") and :381-385 appends the skip when opts.allows fails; both live in `skips`, returned at :491 and merged into result.Skipped at :226. Rust crates/treehouse-core/src/destroy.rs:300 declares `let mut skips = Vec::new()` and pushes at :309-315, but the phase-1 lock closure returns only `Ok::<_, PoolError>(reserved)` at :334, so `skips` never escapes; :317-319 `if self.allows(...).is_err() { continue; }` discards the Err(DestroySkip) payload outright. Phase 2 (:359) iterates only `&reserved`, so a phase-1-skipped target never re-enters, and :168 merges only exec_skips (phase 2). No recovery mechanism: grep for "reserved by another destroy" across crates/ returns exactly one hit (destroy.rs:313), and `cargo build -p treehouse-core` is warning-free because `skips` counts as used (pushed to), so the drop is silent. Two corrections to the claim. (1) Citation off-by-one: the Go v2.1.1 appends are at destroy.go:378 and :383, not :379 and :384. (2) Severity should be MEDIUM, not HIGH. HIGH was justified as "safety invariant missing," and that is not what breaks — the skip still `continue`s at :315/:318, so the gate that PREVENTS deletion works exactly as in Go: no data loss, no wrong worktree removed, no silent corruption. What degrades is the reporting layer: `skipped` comes up short in human output (crates/treehouse/src/format.rs:152) and in JSON (crates/treehouse-core/src/result.rs:99,130), and in the TOCTOU single-target case `destroy <path> --yes` exits 0 with "Destroyed 0 worktree(s)" where Go exits 1 via cmd/destroy.go:215-238, because the exit-1 gate at crates/treehouse/src/main.rs:601-614 reads r.skipped, which no longer contains the phase-1 entry. That is a wrong exit code plus a short JSON array over a narrow race window — the rubric's MEDIUM, not HIGH. Bucket P0_PARITY holds: the behavior existed at the v2.1.1 baseline and Rust never matched it, and it is additionally promised by docs/rust-port-plan.md:235 ("Destroying+live owner -> skip 'reserved by another'") and listed in the destroy --json contract at :427.

</details>

---

---

## 5. Tier 1 — cần một quyết định, rồi vài ngày

> Cần thêm plumbing hoặc chạm wire format. M-012 và M-013 là CRITICAL, **không nên đợi Tier 2**.

### M-012 — Pool slot reuse has no clone-identity scoping, so a second clone of the same origin gets another clone's worktree reset

| | |
|---|---|
| **Severity** | CRITICAL |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | pool semantics — clone identity / reuse |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> Pools are keyed by the origin remote URL (config.rs:139-157, matching Go config.go:230-247), so two clones of the same origin share one pool dir. Clone A's `treehouse get` will happily pick up clone B's slot and run `checkout --detach --force` + `reset --hard` + `clean -fd` on it — destroying whatever clone B's agent was doing, and handing A a worktree whose gitdir points into B's object store. Neither side is told this happened.

**Rust — hiện trạng.**

> /Users/tranquangdang21/Projects/treehouse_rust/crates/treehouse-core/src/pool.rs:478-498 — the reuse loop has no identity comparison. Proof of absence: `grep -rn --include='*.rs' -i 'clone_identity|CommonGitDir|SameFile|same_file' crates/` returns 0 hits; `common_dir` appears 29 times but only as the `GitRepo` struct field name (git/mod.rs:20), never as a comparison.

**Go — tham chiếu.**

> /tmp/th-baseline/treehouse/internal/pool/pool.go:365-372 (acquisitionCommonGitDir + os.SameFile) and 536-548 (`if identityErr != nil { unverifiedClone++; continue }` / `if !os.SameFile(candidateDir, commonDir) { otherClone++; continue }`), with the dedicated error at pool.go:641-646. Feature 1185dc6 "pool: reuse a pooled worktree only for the clone that owns it (#145)"; test file internal/pool/clone_identity_jj_test.go.

**Cách sửa.**

> Add `common_git_dir(path) -> Result<PathBuf>` to GitBackend (resolve `.git/commondir`, canonicalize), compute the requester's once in `Pool::get`, and in acquire_locked `continue` on any slot whose canonicalized common git dir differs or cannot be resolved — counting the two skip reasons so the PoolFull message names them, as Go does.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Could not refute — confirmed on both sides. GO: the guard exists verbatim at pool.go:489-501 (`if identityErr != nil { unverifiedClone++; continue }` / `acquisitionCommonGitDir(wt.Path)` error -> `unverifiedClone++` / `if !os.SameFile(candidateDir, commonDir) { otherClone++; continue }`), with counters at pool.go:462-463 and the dedicated pool-full error at pool.go:632-639 ("A worktree is reused only by the clone it belongs to"). acquisitionCommonGitDir at pool.go:365-371 is cited exactly. Version check: `git tag --contains 1185dc6` -> v3.0.0/v3.0.1/v3.1.0 and `git merge-base --is-ancestor 1185dc6 v2.1.1` -> NOT in v2.1.1, so it postdates the port plan's v2.1.1 baseline: UPSTREAM_DRIFT is correct, not P0_PARITY. RUST: proof of absence holds — `grep -rn --include='*.rs' -iE 'same_file|clone_identity|common_git_dir' crates/` returns 0 hits; the only file-identity code in the tree is state_file.rs:57 (MetadataExt), unrelated. The reuse loop pool.rs:478-506 gates solely on destroying/leased/owner_alive, is_worktree_in_use, and is_dirty, then returns the path at 499-505 with no identity comparison, and WorktreeEntry (state.rs:91-114) carries no clone/identity field to compare against. My strongest refutation attempt was that cross-clone pool sharing may be opt-in in Rust, but that failed: resolve_pool_dir (config.rs:139-157) keys the pool name on a hash of the remote_url (lines 144-147) and defaults the pool root to $HOME/.treehouse (config.rs:160-167), so two clones of the same origin share one pool directory BY DEFAULT with no opt-in — exactly the case Go's comment at pool.go:485-488 describes. The consequence is destructive, not cosmetic: get calls reset_worktree on the reused path (pool.rs:219-223), and shell.rs:329-351 runs `git checkout --detach --force <ref>`, `git reset --hard <ref>`, and `git clean -fd` inside that directory, where git resolves the worktree's .git file into the other clone's git dir — forcing clone B to hard-reset and clean clone A's checkout. CRITICAL holds: `clean -fd` deletes untracked files in an actively used checkout and the reset discards committed-but-unlanded work (silent corruption / data loss). Sole correction to the report: two Go line ranges are off (guard is 489-501 not 536-548; dedicated error is 632-639 not 641-646); the 365-371 citation is exact.

</details>

---

### M-013 — acquire reclaims a clean slot holding unlanded commits without any merge/HEAD safety check

| | |
|---|---|
| **Severity** | CRITICAL |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | pool semantics — acquire/release safety |
| **Confidence** | HIGH |
| **Verify** | 1/2 lens đồng ý (đã sửa bucket) |

**Triệu chứng / tác động.**

> A crashed or rebooted owner leaves owner_pid=0 (heal_state clears it) while its worktree still holds committed-but-unpushed commits. `git status --porcelain` is empty for that tree, so acquire_locked marks the slot available and `reset --hard` + `clean -fd` silently discards every one of those commits. The user gets a fresh worktree with no warning and no backup. This is exactly the data-loss class the property was written to prevent (#79/#104).

**Rust — hiện trạng.**

> /Users/tranquangdang21/Projects/treehouse_rust/crates/treehouse-core/src/pool.rs:478-498 — availability loop is only `destroying || leased || owner_alive` → `is_worktree_in_use` → `git.is_dirty`. No merge check exists anywhere: `grep -rn -i 'safe_to_reset|SafeToReset|head_merged|is_merged' crates/` returns 0 hits for the acquire path. The destructive reset fires at pool.rs:221-223 (`reset_worktree` = `checkout --detach --force` + `reset --hard` + `clean -fd`, shell.rs:329-353) OUTSIDE the lock, and the post-reset re-validation at pool.rs:227-237 only checks that the name still exists in state — it never re-checks HEAD or dirtiness.

**Go — tham chiếu.**

> /tmp/th-baseline/treehouse/internal/pool/pool.go:500-522 (IsWorktreeSafeToReset + headMergedIntoRecordedBase guard); the fix landed as 04d8725 "fix(get): skip reclaiming a pool slot that holds unlanded work (#104)", which replaced `dirty, _ := vcs.IsDirty(wt.Path); if dirty { continue }` with the safe-to-reset guard and pinned the verified commit so the guard and the reset cannot disagree. Go v2.1.1 (the Rust plan's declared baseline) did not have it either, so this is drift, not a port bug — but it is the highest-consequence drift item in this domain.

**Cách sửa.**

> Add `is_worktree_safe_to_reset(path, branch) -> (safe, reset_ref, head)` to the GitBackend trait (git: `merge-base --is-ancestor HEAD <resetRef>` plus a dirty re-check), call it in the acquire_locked availability loop and `continue` on error/unsafe, and thread the captured `reset_ref`+`head` into the post-lock reset so the guard and the reset target the same verified commit.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> The gap is real but the bucket is wrong. EXISTENCE CONFIRMED: crates/treehouse-core/src/pool.rs:478-498 is the entire availability loop and contains only `destroying || leased || owner_alive` (pool.rs:481) -> `is_worktree_in_use` (pool.rs:484-486) -> `git.is_dirty` (pool.rs:490). `grep -c is_head_merged_into_ref crates/treehouse-core/src/pool.rs` returns 0. The merge primitive DOES exist in the Rust tree (git/mod.rs:115 trait method, git/shell.rs:361-378 impl running `merge-base --is-ancestor HEAD <ref>`) so the finder did not miss a synonym — it is simply never called from pool.rs. Its only three call sites are gc.rs:248, destroy.rs:185 and prune.rs:233 (all DELETE paths), never the acquire/reset path. An aggressive synonym sweep of pool.rs for `merge|ancestor|safe|rev-list|revparse|contain` returns only unrelated `ABA-safe` prose at pool.rs:318 and :362. Additionally state.rs:91-114 shows `WorktreeEntry` has NO `base_branch` field, so Go's `headMergedIntoRecordedBase` second reading (pool.go:849-862) is structurally impossible to port as-is — reinforcing the absence. The destructive reset is confirmed: shell.rs:329-351 issues `checkout --detach --force <ref>`, `reset --hard <ref>`, `clean -fd`, fired from pool.rs:221-223 OUTSIDE the lock, and the LOCK #2 re-validation at pool.rs:227-237 only checks that the name still exists in state, never HEAD or dirtiness. The Go side citation is correct: at v3.1.0 pool.go:511-521 has the `IsDirty` then `vcs.IsWorktreeSafeToReset(wt.Path, branch)` (line 516) then `if !safe && !headMergedIntoRecordedBase(wt, branch, head)` (line 520) guard, with the helper defined at pool.go:849-862. BUCKET CORRECTION: P0_PARITY is refuted. The Rust port plan declares Go v2.1.1 as its baseline (docs/rust-port-plan.md:5). At that exact tag, `git show v2.1.1:internal/pool/pool.go` line 125 reads `dirty, _ := git.IsDirty(wt.Path)` and line 130 `git.ResetWorktree(wt.Path, branch)` — character-for-character the Rust behavior, i.e. Rust faithfully matches the contract it was given. The guard landed in 04d8725 "fix(get): skip reclaiming a pool slot that holds unlanded work (#104)"; `git merge-base --is-ancestor v2.1.1 04d8725` proves it is strictly after v2.1.1, and CHANGELOG.md:63 files it under release 3.0.0. Therefore this is legitimate upstream lag, UPSTREAM_DRIFT, not a port bug. The finder's own GO EVIDENCE text concedes this ("this is drift, not a port bug") while its BUCKET field claims P0_PARITY — a self-contradiction. SEVERITY UNCHANGED at CRITICAL: when the path does fire, `reset --hard` plus `clean -fd` orphans committed-but-unmerged commits, which is silent data loss under the stated rubric, independent of bucket.

</details>

---

### M-014 — return/get never terminate lingering processes, so a slot is reset while a writer is still in it

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | pool semantics — return path safety |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> `treehouse return` runs `checkout --detach --force` + `reset --hard` + `clean -fd` on a worktree that may still have a live agent, editor, or dev server writing into it, then hands the slot straight back to the pool. The next `get` inherits a half-written tree and a foreign live process — the exact contamination the fix was written for. Because `detach_and_return` is also a stub, the non-zero-shell-exit branch of `get` returns a slot still carrying whatever HEAD state it had.

**Rust — hiện trạng.**

> /Users/tranquangdang21/Projects/treehouse_rust/crates/treehouse/src/main.rs:209-241 (cmd_return) — checks dirtiness, then calls `pool.release_conditional(&path, &preconditions, None)`. The `before_reset` argument is literally `None`. Same in cmd_get at main.rs:194-197 (`pool.release(&path.to_string_lossy())?`, and `detach_and_return` at main.rs:198-201 is an empty stub `let _ = path; Ok(())`). `terminate_with_grace` (process.rs:152-170) exists and is correct, but grep shows its only callers are destroy.rs:398 and run.rs:176 — never the return path.

**Go — tham chiếu.**

> /tmp/th-baseline/treehouse/cmd/get.go:331-356 (killLingeringProcesses: terminate, then re-scan via UnprotectedProcessesInWorktree, error out with "worktree still has live processes after termination" if any survive) wired in as ReleaseConditional's beforeReset callback at get.go:248-254, so the kill-and-verify runs under the state lock immediately before the destructive reset. Fix 978da87 "fix(return): refuse to reset a worktree that still has live writers (#103)", v2.2.1.

**Cách sửa.**

> Pass a `before_reset` closure from both cmd_return and cmd_get that calls `terminate_with_grace` then re-scans with `find_in_worktree` filtered by the caller's protected chain, returning an error if survivors remain; and implement `detach_and_return` to call `detach_worktree` (skipped for a markerless path) before the release.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> The existence claim is TRUE — I could not refute it. Go's killLingeringProcesses genuinely exists at cmd/get.go:331 and does kill-then-re-verify, wired as ReleaseConditional's beforeReset at cmd/get.go:251-254 so it runs under the state lock immediately before the destructive reset. The Rust return/get path genuinely passes `None` for before_reset at every call site, and the required enumeration function does not exist anywhere in crates/ (swept for lingering, unprotected, survivor, live_processes, "still has live", in_use, live_ across the whole tree — no hidden equivalent under another name). Rust also has an empty `detach_and_return` stub at main.rs:731-734, so the nonzero-exit path is a no-op too.
>
> The BUCKET is wrong, and the finder's own evidence self-contradicts: it cites fix 978da87 and even writes "v2.2.1" in the evidence, yet buckets P0_PARITY. CHANGELOG.md:71 places that entry under the `## [2.2.1]` header at CHANGELOG.md:66, which is three releases AFTER the port plan's stated v2.1.1 baseline. P0_PARITY requires behavior the v2.1.1 baseline already had; this landed later. Per the task's own definition this is legitimate lag — UPSTREAM_DRIFT, not a port bug.
>
> Severity HIGH is upheld: the Rust path performs a destructive reset_worktree with zero liveness check, so a detached writer (e.g. an opencode server ignoring SIGHUP — the exact scenario in the Go doc comment) can be clobbered mid-write, and Rust emits no warning at all, whereas Go refuses the reset and leaves the slot in place. Not CRITICAL because it is upstream-lag rather than a regression against the design contract, and the worktree is one the user explicitly asked to return.
>
> One correction to the finder's supporting details: the Go wiring is at get.go:251-254 (function body), not 248-254, and the hook infrastructure in Rust is NOT missing — release_conditional accepts before_reset and invokes it correctly under LOCK #1; it is simply never supplied by any return-path caller.

</details>

---

### M-015 — No `recoverMissingStateEntries`: a missing state file leaves on-disk worktrees invisible, so `next_name` reissues an occupied slot name

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | corrupted / partial state recovery |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> Two distinct failures. (a) Crash-between-worktree_add-and-state-write: the worktree exists on disk but is absent from state, so Rust's `acquire_locked` (pool.rs:478-506) never sees it, and `next_name` (pool.rs:649-659) computes `max(name)+1` over the truncated list — it will happily re-issue the SAME slot name and run `git worktree_add` into a directory that already holds a real worktree, or overwrite. (b) A missing state file makes the pool read as empty, so `status` under-reports and `destroy --all` reports nothing to clean. Go covers both by scanning. The Rust plan §6.3 table lists "Corrupt/truncated state" but not the missing-state case at all.

**Rust — hiện trạng.**

> crates/treehouse-core/src/state.rs:156-167 `read_state` returns `Ok(State::default())` on NotFound (and :177 identically for the env variant) with NO pool-dir scan; on successful parse it returns `Ok(s)` directly with no scan. Grep for `recover_missing|recoverMissingStateEntries` across crates/ returns nothing. Empirically verified: a pool dir containing `1/myrepo/.git` and no state file yielded `0 entries`.

**Go — tham chiếu.**

> state.go:118-129 `ReadState`: on `os.IsNotExist` it first stats poolDir and, if the dir EXISTS, calls `recoverMissingStateEntries(poolDir, State{})` rather than returning empty. :309-342 that function scans `<poolDir>/<slot>/<repo>`, and for each path not already in state appends `recoverOneWorktree(...)` — i.e. quarantined-as-leased. It is invoked on the SUCCESSFUL-parse path too (:164 `return recoverMissingStateEntries(poolDir, s)`), covering the window where `git worktree add` succeeded but the state write failed. Pinned by state_test.go:31-49 `TestReadState_RecoversWorktreeMissingFromValidState`. Added in 0e87c62/ee8652b — after v2.1.1.

**Cách sửa.**

> Add `recover_missing_state_entries(pool_dir, &mut state)` mirroring state.go:309-342 and call it (a) when the state file is missing but poolDir exists, and (b) after a successful parse. Make `next_name` additionally probe the filesystem for the highest existing `<poolDir>/<n>` directory so a name is never reissued regardless of state contents.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Adversarially verified on both sides; I could not refute it. Every citation is accurate.
>
> GO SIDE CONFIRMED at all cited lines. /tmp/th-baseline/treehouse/internal/pool/state.go:118-129 ReadState stats poolDir on os.IsNotExist and calls recoverMissingStateEntries(poolDir, State{}) when the dir exists; :309-342 scans <poolDir>/<slot>/<repo>, skipping known paths and appending recoverOneWorktree for the rest; :164 shows the success-parse path also returns recoverMissingStateEntries(poolDir, s). quarantineEntry at :359-370 marks recovered entries Leased=true + RecoveredLeaseHolder; recoverOneWorktree at :393-403. state_test.go:31-49 TestReadState_RecoversWorktreeMissingFromValidState confirmed verbatim.
>
> RUST SIDE CONFIRMED ABSENT, no hidden equivalent. crates/treehouse-core/src/state.rs:160 and :177 return Ok(State::default()) on NotFound with no pool-dir scan; :164 and :181 return Ok(s) directly on successful parse. recover_corrupt_state (state.rs:201) is reachable ONLY from the parse-error arm, so it never covers the missing-file or valid-parse-missing-entry cases. heal_state (state.rs:272-286) moves entries the opposite direction — drops entries whose path vanished, never adds from disk. The only worktrees.push outside acquire is pool.rs:534. Aggressive sweep for recover_missing|reconcile|adopt|backfill|scan_|existing_worktrees|quarantin|discover_existing across all crates/ found only the recover_corrupt_state path. Empirically reproduced with the built binary: deleting only the state file makes `treehouse status --json` return [] for a pool with a live worktree.
>
> BUCKET UPSTREAM_DRIFT CONFIRMED. `git show v2.1.1:internal/pool/state.go` returns State{}, nil on IsNotExist with no scan — v2.1.1 did NOT have this. It arrived in 0e87c62 (2026-09-05) and ee8652b (2026-09-13); v2.1.1 is tagged 2026-07-30. The only related port-plan line (docs/rust-port-plan.md:610) concerns the 0-byte CORRUPT case, which Rust does implement, so P1_UPGRADE does not apply.
>
> ONE CORRECTION TO THE HARM CHAIN. The claimed mechanism (next_name reissues an occupied slot name) is true — reproduced live: after deleting only the state file, `get` recomputes slot 1 and git fails with "fatal: '.../pool/1/myrepo' already exists", exit 128. But the implied danger is overstated: user content in that worktree (IMPORTANT.txt) SURVIVED intact. I separately verified git refuses both a registered-worktree path and a dir containing untracked user content, always preserving contents. So this is a loud, non-destructive failure — NOT silent corruption, NOT data loss, NOT a wrong worktree deleted.
>
> SEVERITY STAYS HIGH, but for different reasons than the claim implies. The primary command is permanently wedged: because state is empty on every subsequent load, next_name recomputes "1" each time, so acquire can never succeed without manual filesystem surgery. Meanwhile status/gc/prune/destroy all silently under-report a pool holding live worktrees — gc never reclaims the orphan and destroy cannot target it by name, leaving it unmanageable through the CLI. A core command broken with no in-tool recovery path fits HIGH; MEDIUM would understate the operational impact.

</details>

---

### M-016 — Resolve `return <name>` as a slot name, not only as a filesystem path

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | CLI surface — command surface |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> `treehouse return <name>` — the form the Go help text tells users to use, and the only form that works from inside the repo when a `worktree_path` template puts slots outside the pool layout — is broken in Rust. A caller that took the name from `treehouse status` gets "not managed by treehouse" and no hint that the name form exists, so it falls back to reconstructing absolute paths.

**Rust — hiện trạng.**

> crates/treehouse/src/main.rs:213-217 `let path = args.path.clone().or_else(|| std::env::var("TREEHOUSE_DIR").ok()).ok_or_else(|| anyhow!("no worktree path specified"))?;` then main.rs:226 treats the string as a path unconditionally. `grep -rn 'find_by_name|FindByName|by_name' crates/` → zero hits. Verified: `treehouse return 1` → `worktree 1 is not managed by treehouse`, EXIT=1, despite slot 1 existing.

**Go — tham chiếu.**

> cmd/return_cmd.go:441-495 resolveReturnTarget / resolveReturnTargetByName: the argument is read as a PATH first and only then as a slot NAME via `pool.FindByName`; `couldBeWorktreeName` (return_cmd.go:467-475) rejects anything containing `/` or `\`. Missing names produce `unknownWorktreeNameError` (return_cmd.go:502-515) which lists the pool's actual names. Documented in the command's own Long help at return_cmd.go:53-58. Added in v3.0.0 (#143).

**Cách sửa.**

> Add `Pool::find_by_name` and make cmd_return try the path reading first (preserving Go's ordering so nothing that resolves today changes), then fall back to name resolution against the repo's pool when the path reading fails and the arg has no separator.

**Ghi chú.** `return <name>` và `return --all` là hai finding riêng (`cmd/return_cmd.go:140,258-332`, #143) nhưng cùng một PR. Lưu ý: `cmd_enter` đã có sẵn resolver tại `main.rs:183` — `cmd_return` chỉ chưa gọi.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Claim survives adversarial refutation on all axes; both sides' citations verified exact. GO: cmd/return_cmd.go:441 resolveReturnTarget resolves PATH first and only falls through to slot NAME on errReturnWorktreeUnmanaged (lines 451-457); :466 couldBeWorktreeName rejects anything with `/` or `\`; :480 resolveReturnTargetByName calls pool.FindByName; :501 unknownWorktreeNameError lists actual pool names. Long help at :53-58 documents the three-way addressing. CHANGELOG.md:30 places "return a worktree by name and add return --all (#143)" under v3.0.0, AFTER the port plan's stated v2.1.1 baseline (docs/rust-port-plan.md:5) — so UPSTREAM_DRIFT is the correct bucket, not P0_PARITY.
>
> RUST: genuinely absent from `return`. The finder's absence-proof grep (`find_by_name|FindByName|by_name` -> zero hits) was UNDER-INCLUSIVE but the conclusion still holds: Rust HAS a working name->path resolver, spelled differently and wired to a different command. crates/treehouse/src/main.rs:183 in cmd_enter does exactly what Go's pool.FindByName does (`statuses.iter().find(|s| s.name == args.name)`), with a matching error string at main.rs:186. cmd_return (main.rs:209) never calls it — it reads args.path at main.rs:213-217 and passes it unconditionally to git_is_dirty/release_conditional as a filesystem path. ReturnArgs (crates/treehouse/src/cli.rs:102-115) declares only force/if_lease_id/if_lease_holder/path. So this is a per-command wiring gap, not a missing subsystem.
>
> EMPIRICAL PROOF: built both binaries (cargo build + `go build` of HEAD cb7fb26) and ran the identical scenario in isolated HOMEs with a real pool where slot "1" exists. Go v3.1.0 `return 1` -> "Worktree returned to pool.", EXIT=0. Rust `return 1` -> "worktree 1 is not managed by treehouse", EXIT=1. Go's unknown-name path yields the richer diagnostic the finder cited ("no worktree named \"bogus-name\" in pool (available: 1), and it is not a treehouse-managed worktree path either"); Rust has no equivalent.
>
> SEVERITY CORRECTED HIGH -> MEDIUM. The Rust failure is safe, not destructive: it errors and exits 1 before releasing anything — no worktree deleted, no data loss, no wrong target touched. Path-based `return` still works (verified EXIT=0). Under the stated rubric this is a "feature gap" (one of three addressing modes missing), not a "core command broken or safety invariant missing."
>
> Incidental: the same v3.0.0 commit also added `return --all` (return_cmd.go:57-64); Rust ReturnArgs has no --all — a separate gap from the same PR, not part of this finding.

</details>

---

### M-017 — Add `return --all` to reclaim every held worktree in a pool

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | CLI surface — command surface |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> There is no way to bulk-release a pool in Rust. An agent harness that crashed mid-run leaves every slot in-use, and recovery requires scripting `treehouse return <path>` per slot with no way to enumerate-and-return atomically — so a pool that Go reclaims with one command must be reclaimed slot-by-slot in Rust, and any slot missed keeps the pool short.

**Rust — hiện trạng.**

> crates/treehouse/src/cli.rs:101-114 `struct ReturnArgs` has only `force`, `if_lease_id`, `if_lease_holder`, `path`. `grep -rn 'return_all|returnAll|held_worktrees|returnable_status' crates/` → zero hits. Verified: `treehouse return --all` fails with clap `error: unexpected argument '--all' found`, EXIT=2.

**Go — tham chiếu.**

> cmd/return_cmd.go:140 `returnCmd.Flags().BoolVar(&returnAll, "all", false, "Return every held worktree in this repository's pool …")`; dispatch at return_cmd.go:92-101 (rejects a positional arg and rejects combining with `--if-lease-*`); implementation `returnHeldWorktrees()` at return_cmd.go:258-332 with per-slot `bulkReturnPreconditions` (return_cmd.go:193-203) and `returnableStatus` (return_cmd.go:230-241). Added in v3.0.0 (#143).

**Cách sửa.**

> Add `all: bool` to ReturnArgs; implement cmd_return_all mirroring returnHeldWorktrees — list the pool, keep leased/in-use/dirty/unverified, skip available/damaged/HeldOnlyByCwd, pin each slot to the lease the listing saw, and keep the summary line `🌳 Returned %d of %d held worktree(s); %d skipped; %d not held.` on stderr.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> I tried to refute on four fronts and failed on all four; the gap is genuine.
>
> (1) Go citations are exact, not approximate. /tmp/th-baseline/treehouse/cmd/return_cmd.go:140 registers the flag with the quoted help string; :92-101 is the dispatch that rejects a positional arg and rejects combining with --if-lease-*; :193 bulkReturnPreconditions, :230 returnableStatus, :258-332 returnHeldWorktrees. Every cited line lands on the claimed symbol (HEAD cb7fb26, v3.1.0).
>
> (2) Rust genuinely lacks it — no synonym hunt hit. ReturnArgs (crates/treehouse/src/cli.rs:101-114) has exactly force/if_lease_id/if_lease_holder/path. The --all flags that DO exist belong to other commands: PruneArgs cli.rs:129-130, GcArgs cli.rs:174-175, DestroyArgs cli.rs:187-188, dispatching to cmd_prune_all (main.rs:265) and cmd_gc_all (main.rs:350) — never to cmd_return (main.rs:209-236, which takes args.path verbatim). Grep for return_all/returnAll/held_worktree/returnable_status/bulk_return across crates/ returns zero hits for return. No cfg(feature) escape (only two in the tree, both `toon`, format.rs:189,295) and no env-var hatch. Runtime check on the built binary: `./target/debug/treehouse return --all` → `error: unexpected argument '--all' found`, usage `treehouse return [OPTIONS] [PATH]`.
>
> (3) Bucket UPSTREAM_DRIFT confirmed by checking the baseline tag directly: `git show v2.1.1:cmd/return_cmd.go` registers only --force, --if-lease-id, --if-lease-holder — no --all. CHANGELOG.md:30 dates `return --all` to v3.0.0 (#143), after the plan's declared baseline (docs/rust-port-plan.md:5). It is NOT P1_UPGRADE: plan §5 line 508 contemplates only `return <path>`, and P1-A/B/C/D (plan:562-565) never mention it. Nor BUG_RUST — Rust has no buggy bulk-return concept, it has none.
>
> (4) I DISPUTE THE SEVERITY: HIGH is wrong, MEDIUM is right. The rubric reserves HIGH for "core command broken or safety invariant missing" — neither applies. `return` works correctly for its v2.1.1 contract (single target, all three baseline flags, conditional release via ReleasePreconditions). No invariant is missing, and no wrong worktree can be deleted: the bulk path Go adds is precisely the CONSERVATIVE one — bulkReturnPreconditions (return_cmd.go:193-203) sets RefuseRecovered and RequireUnleased, and returnableStatus (:230-241) skips held-by-cwd, available and damaged slots — so its absence removes a convenience, not a guard. That also rules out CRITICAL. The leak scenario the finder invokes already has a partial remedy it overlooked: Rust ships `gc`/`gc --all` (cli.rs:174-175, main.rs:350) to reclaim expired-lease and dead-owner worktrees. A missing convenience flag on an otherwise-correct command is a feature gap → MEDIUM.
>
> Adjacent observation, NOT part of this finding: the other half of #143 is also absent — Go resolves `return <name>` via resolveReturnTargetByName (return_cmd.go:480) but Rust cmd_return feeds args.path straight into pool.release_conditional with no name lookup, despite `enter` supporting names (main.rs:186). That is a separate finding and should not be folded into this one.

</details>

---

### M-018 — Honor `--format json|toon` on destroy/prune/return/gc instead of silently printing human text

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **P1_UPGRADE** — chính port plan đã hứa nhưng chưa giao |
| **Domain** | CLI surface — output routing |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> The flag parses cleanly, so an agent that does `--format json` gets exit 0 and prose it cannot parse — the worst failure mode for an automation contract, because it is silent rather than a clean rejection. The plan explicitly promised these schemas (`return --json`, `prune --json`, `destroy --json`, `gc --json` in §5.3) and listed them as a P1-B gate; the flag surface exists without any of the implementation behind it.

**Rust — hiện trạng.**

> crates/treehouse/src/cli.rs:26-28 declares `--format` `global = true`, so clap accepts it on every subcommand. But crates/treehouse/src/main.rs:598 hardcodes `format::render(format::OutputFormat::Human, …)` for destroy, main.rs:342 hardcodes Human for prune, `render_gc` (main.rs:521) uses bare `println!` with no formatter at all, and cmd_return (main.rs:209-239) never builds a CommandResult. Verified: `treehouse destroy . --format json --all` printed the human 🌳 Dry-run table; `treehouse prune --format json` printed `🌳 No stale worktrees to prune.`; `treehouse return --format json <path>` was accepted and printed an error string.

**Go — tham chiếu.**

> n/a — Go has no JSON for these commands; the promise is the port contract: docs/rust-port-plan.md:415 "**Commands that accept `--format`:** `get`, `return`, `status`, `prune`, `destroy`, `gc`, `doctor`, `run`", and the P1-B DoD at docs/rust-port-plan.md:585 "— `--format human | json | toon` on get/status/return/prune/destroy/doctor/gc/run".

**Cách sửa.**

> Thread `cli.format` into cmd_prune/cmd_destroy/cmd_gc/cmd_return and dispatch on it: Human → the existing renderers, Json/Toon → `format::render(fmt, &CommandResult::X(result), &mut out, &mut err)`. Either reject `--format` on init/update/enter per plan line 416, or drop `global = true` and declare it per-subcommand so an unsupported combination fails loudly.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> CLAIM CONFIRMED, severity corrected down. I read both trees and reproduced. The port plan promises `--format` on return/prune/destroy/gc at docs/rust-port-plan.md:415 and in the P1-B DoD at :585 (still an unchecked `- [ ]`). Go has no `--format` at all — the only machine flag is `--json` on get/status/lease (cmd/status.go:153, cmd/get.go:97, cmd/lease.go:44) — so the finder's "GO EVIDENCE: n/a" is right and this is a plan promise, not parity. On the Rust side every cited line is accurate: cli.rs:26-28 makes `--format` global so clap accepts it everywhere, while main.rs:598 (destroy) and main.rs:342 (prune) hardcode `format::OutputFormat::Human`, render_gc (main.rs:512-545) uses bare `println!`, cmd_return (main.rs:209-239) never builds a CommandResult, and result.rs:181-188 has no `Gc` variant. I refuted-looked hard for an alternate mechanism (TREEHOUSE_FORMAT env, no_color, other render entry points) and found none: only two `format::render` call sites pass a non-Human value, and both are get/status. Controls prove the plumbing itself is fine — `status --format json` (main.rs:249-257) and `doctor --format json` (main.rs:817) both emit real JSON. Reproduced on a real pool: `destroy --format json` printed the human Dry-run table to stdout, `gc --format json` printed '🌳 Dry run: would reclaim…', `return --format json` printed only the stderr banner, `destroy --format toon` printed the same human table. WHERE I DISSENT: severity HIGH is overstated. All four commands work correctly in human mode with Go-byte-exact output — no data loss, no wrong worktree destroyed, no safety invariant missing. The only consequence is that a script parsing `--format json` gets human text on stdout, i.e. a wrong output shape on a not-yet-shipped P1 item, which the rubric assigns to MEDIUM ("feature gap or wrong exit code/JSON shape"), not HIGH ("core command broken or safety invariant missing"). Further, for prune/destroy the JSON payloads are ALREADY implemented in treehouse-core/src/result.rs:213-218 and format.rs render_json/render_toon already handle those variants — only the call site hardcodes Human, so it is closer to a one-line wiring bug than a missing feature, which reinforces MEDIUM. `return` is likewise merely unwired (result.rs:208 has a Return payload); only `gc` genuinely lacks a CommandResult variant. Bucket P1_UPGRADE stands: promised by the plan, absent upstream, plan checkbox still open.

</details>

---

### M-019 — Emit exit status 3 when a dirty worktree is left unreturned (`get` and `return`)

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | CLI surface — exit codes |
| **Confidence** | HIGH |
| **Verify** | 1/2 lens đồng ý (đã sửa bucket) |

**Triệu chứng / tác động.**

> Verified: interactive `treehouse get` whose subshell exited 1 leaving a dirty worktree returned EXIT=0 with no diagnostic at all, and the slot stayed `dirty` in `status`. Scripts doing `treehouse get && next-step` proceed as if the slot was recycled; the pool then silently starves because `get` skips dirty slots and `prune` will not reclaim them. This is the exact failure mode the v3.0.0 breaking change was made to close.

**Rust — hiện trạng.**

> crates/treehouse/src/main.rs:58-61 is the only exit path for failures: `eprintln!("{e:#}"); std::process::exit(1);`. Grep for `exit(3)` / `ExitNotReturned` / `NOT_RETURNED` across crates/ returns nothing. crates/treehouse/src/main.rs:169-172 (`eprintln!("Worktree left dirty. …"); return Ok(())`) and main.rs:227-230 (`eprintln!("Aborted."); return Ok(())`) both exit 0.

**Go — tham chiếu.**

> cmd/exit.go:9-20 defines `ExitNotReturned = 3`; exit.go:44-53 maps a tagged error to it. Used at cmd/get.go:227-230 (`return withExitCode(ExitNotReturned, ...)` when the subshell leaves the worktree dirty and the prompt is declined) and cmd/return_cmd.go:119-128 (both abort arms) and return_cmd.go:327-330 (`return --all` aborts). CHANGELOG.md v3.0.0 lists this as a BREAKING CHANGE with the rationale "Callers that read exit 0 as 'the slot was released' were reading the bug this fixes; scripts under `set -e`, or using `treehouse return "$p" && next-step`, will now stop or branch differently at a dirty abort."

**Cách sửa.**

> Add an `ExitCode` enum (Failure=1, NotReturned=3) threaded through the anyhow error as a downcastable marker, and have `main` map it to `process::exit(3)`; make the get-dirty-bail and the return-abort arms return the tagged error instead of `Ok(())`.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> CONFIRMED on both sides, with two corrections. Go: cmd/exit.go:19 defines ExitNotReturned=3, ExitCode() at exit.go:44-53 unwraps the tagged error, main.go:33 applies it; used at cmd/get.go:227, cmd/return_cmd.go:120, :125, :327. CHANGELOG.md:22 (v3.0.0 BREAKING CHANGES) carries the quoted rationale verbatim. Crucially, `git show v2.1.1:cmd/exit.go` fails with "path exists on disk, but not in v2.1.1", and at v2.1.1 get.go:100 and return_cmd.go:75 both `return nil` (plain exit 0) — so this is genuinely post-baseline and UPSTREAM_DRIFT is the right bucket, not P0_PARITY. Rust: the only error exit is main.rs:59-60, an unconditional exit(1); main.rs:802 is `run`'s child-status passthrough and :846 is `doctor`, both unrelated. An aggressive synonym grep across all of crates/ for exit.?code|exit.?status|not.?returned|ExitFailure|code == 3 found no exit-3 concept under any alias or feature flag. main.rs:169-171 (get dirty bail) and main.rs:228-229 (return abort) both return Ok(()) → exit 0. CORRECTION 1: one cited Go site is N/A — return_cmd.go:327 is the `return --all` arm, and Rust's ReturnArgs (crates/treehouse/src/cli.rs:102-114) has no `--all` flag at all, so that arm cannot be exercised; the live gap is the other three sites. CORRECTION 2: severity overstated. docs/rust-port-plan.md:618 explicitly specifies the current Rust behavior ("Special cases: `return`/`get` dirty-prompt declined → exit 0"), so Rust is faithfully implementing its own design contract rather than erring; and the defect is strictly a wrong exit code — `return` performs the correct action on disk. The rubric grades "wrong exit code/JSON shape" as MEDIUM and reserves HIGH for a core command broken or a safety invariant missing, neither of which holds. The automation hazard (scripts doing `treehouse return "$p" && next-step` proceeding past a leaked slot) is real but belongs to the MEDIUM class.

</details>

---

### M-020 — Prune and destroy resolve one repo root for the whole pool instead of per-worktree, so a shared pool corrupts other clones' worktrees

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | pool semantics — prune/destroy reclaim scope |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> On a pool shared by two clones, `treehouse prune --yes` or `destroy --all` runs `git worktree remove` from the wrong repository root. Git either errors out (leaving the stale registration to accumulate) or, worse, deregisters/rewrites bookkeeping belonging to a different clone. For the `--all` sweeps `self.root` is not even a git repo, so the removal always fails and prune silently degrades to "records a CleanupError, keeps the entry" forever.

**Rust — hiện trạng.**

> /Users/tranquangdang21/Projects/treehouse_rust/crates/treehouse-core/src/prune.rs:106-110 (`main_repo_root(&self.root)` once) then prune.rs:368-375 passes that single `self.root` to `worktree_remove`. Symmetrically crates/treehouse-core/src/destroy.rs:105-117 resolves one `repo_root` and destroy.rs:430-436 reuses it for every target. When the pool is opened via `Pool::open_at` (prune --all / gc --all path), `self.root` is the pool dir's PARENT, i.e. `~/.treehouse` — not a repository at all.

**Go — tham chiếu.**

> /tmp/th-baseline/treehouse/internal/pool/destroy.go:614-620 (resolvePoolRepoRoot: "A pool may be shared by multiple clones, so callers must resolve every slot independently and must not apply one slot's root to another"), used at destroy.go:588 and prune.go:405; the resolver is worktreePruneContextResolver() (prune.go:401-418) with a per-slot context cache. Feature 94c092f "pool: let prune and destroy reclaim worktrees from other clones in a shared pool (#146)".

**Cách sửa.**

> Add a per-worktree repo-root resolver (mirror `resolvePoolRepoRoot`: `main_repo_root(&wt.path)`) and call it inside the analyze loop and again in the execute phase for each reserved worktree, memoized by resolved root. Thread the resolved root into `worktree_remove` and into `default_branch_merge_ref` per slot.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> The gap is real and the bucket is correct, but the severity rationale is wrong. Rust genuinely lacks per-worktree root resolution: prune.rs:106-110 resolves main_repo_root(&self.root) once, prune.rs:368-371 passes common_dir: self.root.clone() to worktree_remove for every slot, and destroy.rs:105-117 / destroy.rs:430-436 plus gc.rs:102-106 / gc.rs:351-354 are symmetric. Only pool.rs:335 (release_conditional) resolves per-worktree. Go has the opposite: destroy.go:614-620 resolvePoolRepoRoot (with an explicit doc comment that callers "must not apply one slot's root to another", used at destroy.go:588) and prune.go:394-398 singleRepoPruneContextResolver which discards the caller-supplied repoRoot and delegates to the per-slot worktreePruneContextResolver at prune.go:401-418. Bucket UPSTREAM_DRIFT confirmed: `git tag --contains 94c092f` yields only v3.0.0, v3.0.1, v3.1.0 — strictly after the v2.1.1 port baseline. WHAT I REFUTE is the claimed "corrupts other clones' worktrees" / data-loss framing. I tested it directly: `git -C <wrong repo> worktree remove --force <other clone's worktree>` returns "fatal: '<path>' is not a working tree" (exit 128, target untouched), and `git -C <non-repo> worktree remove` returns "fatal: not a git repository". Git validates the target is a registered worktree of the invoking repo, so a foreign clone's worktree is never a candidate. Rust handles that error safely too: prune.rs:375-385 sets cleanup_ok=false, which skips the remove_dir_all at prune.rs:387 and retains the state entry for retry; destroy.rs:437-449 restores the reservation and pushes a skip before ever reaching remove_dir_all_guarded at destroy.rs:451. So there is no silent corruption, no wrong worktree deleted, and no data loss. The actual impact is a loud failure — affected slots land in errors[]/skipped[] instead of being reclaimed — which is precisely the scope of upstream feature #146. Worst case is functional: with Pool::open_at (pool.rs:163-166 sets self.root to the pool dir's parent, i.e. ~/.treehouse, not a repository) the `prune --all` and `gc --all` paths at main.rs:290 and main.rs:402 cannot reclaim anything and record a git_worktree_remove error per slot. That is a genuine feature gap in a cleanup command, not a safety invariant violation, so MEDIUM rather than HIGH.

</details>

---

### M-021 — acquire has no markerless-slot fail-closed: a slot whose .git is gone is read through the enclosing repository

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | pool semantics — acquire slot eligibility |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> With an in-project pool (`root = "."`, supported by config.rs:174-178), a slot whose `.git` file has been deleted makes `is_dirty` and `is_worktree_in_use` run with cwd inside the pool, which git resolves upward to the ENCLOSING repository. Those checks then vouch for the enclosing repo — which is clean and merged — and the slot is marked available. `reset_worktree` (pool.rs:221) then runs `checkout --detach --force` + `reset --hard` + `clean -fd` in the enclosing repository, rewriting the user's actual working tree and deleting its untracked files. This is the worst-case outcome in this whole domain.

**Rust — hiện trạng.**

> /Users/tranquangdang21/Projects/treehouse_rust/crates/treehouse-core/src/pool.rs:478-498 — the reuse loop has no marker/flavor check of any kind. Proof: `grep -rn --include='*.rs' -i 'flavor|marker|WorktreeBackendName' crates/treehouse-core/src/pool.rs` returns 0 hits, and the GitBackend trait (git/mod.rs:60-127) exposes no backend-name accessor at all. The classification that exists in worktree.rs:193-197 (`backing_repo_missing` → Unverified) is never consulted on the acquire path.

**Go — tham chiếu.**

> /tmp/th-baseline/treehouse/internal/pool/pool.go:499-518 (the `vcs.WorktreeBackendName(wt.Path) == ""` branch, which `continue`s and counts `otherFlavor`) and pool.go:640-646 (the "reuse a pooled worktree only for the clone that owns it" comment naming the in-project-pool hazard). Feature c88b53e (#110), v2.3.0.

**Cách sửa.**

> Add a `worktree_backend_name(path) -> String` to GitBackend (reads the `.git`/`.jj` marker, no fallback), and in acquire_locked `continue` on an empty result before any per-slot check runs — same as Go. Also skip in `status` and classify Damaged, as Go does.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> CONFIRMED on the Rust side after five refutation attempts. (1) Grepped all of crates/ for flavor|marker|backend_name|WorktreeBackendName — only unrelated hits (main.rs:815,824,829 JSON output markers; state.rs:187 unrelated doc comment). (2) The GitBackend trait (git/mod.rs:62-127) has 18 methods and exposes no backend-name accessor. (3) heal_state (state.rs:272-286) checks only Path::new(&wt.path).exists() at line 275 — a directory that exists with .git removed survives into the reuse loop. (4) The .git marker check at state.rs:232 is inside recover_corrupt_state, which runs only when the state FILE fails to parse (line 205) and hard-codes leased:true (line 242), which acquire_locked rejects at pool.rs:481 — unreachable from reuse. (5) Go pool.go:468-480 is verbatim as cited, and `git show v2.1.1:internal/pool/pool.go | grep WorktreeBackendName` returns 0 hits, confirming the guard postdates the Rust baseline. The Rust hazard chain is complete: pool.rs:490 is_dirty -> shell.rs:320-327 runs `git status` with cwd = the markerless slot (git walks UP to the enclosing repo) -> pool.rs:221-223 -> shell.rs:329-351 reset_worktree resolves repo_root via `rev-parse --show-toplevel` (shell.rs:180, also walks up) then runs checkout --detach --force + reset --hard + clean -fd against that enclosing repo — exactly the hazard the Go comment names.
>
> TWO CORRECTIONS TO THE CLAIM. (a) The second Go citation is WRONG: pool.go:640-646 is the templated-path comment, not the clone-identity guard. The real clone guard is pool.go:485-501 with the quoted text in the error string at pool.go:633, and it is a SEPARATE feature (commit 1185dc6, #145, tagged v3.0.0~5) versus the markerless guard (c88b53e, #110, v2.3.0~1). The finder bundled two releases' work into one finding. (b) Severity HIGH is overstated: Rust's default pool root is home_dir() (config.rs:159-163), out-of-project, so in the default configuration a markerless slot has no enclosing repo, `git status` fails, and pool.rs:490's map_err(PoolError::Git)? aborts the whole get loudly — no corruption. Silent corruption requires the pool root pointed inside a repo, reachable via the env-configurable root (config.rs:553-555) but non-default, and the in-project pool mode the Go comment defends against (fdb3f99, v2.2.0) is itself unported (0 hits for info/exclude|in-project). Bucket UPSTREAM_DRIFT is correct. A reviewer who weights "configurable pool root inside a monorepo" as ordinary could argue HIGH; on the shipped default it is a hard error, so MEDIUM.

</details>

---

### M-022 — Corrupt-state recovery aborts the entire pool on ONE unreadable slot, instead of recovering the healthy slots around it

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | corrupted / partial state recovery |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> One broken slot (dangling `.git` symlink, EACCES on the marker, an ELOOP loop) makes EVERY pool command fail with `StateError::RecoverScan` — status, get, return, destroy, prune, gc all brick. Go recovers the healthy slots and quarantines only the broken one. Because `read_state` is on the path of every operation, this is a denial-of-service on the whole pool from a single stray symlink that any process could create inside a slot.

**Rust — hiện trạng.**

> crates/treehouse-core/src/state.rs:232-236 `match std::fs::metadata(wt_path.join(".git")) { Ok(_) => {} Err(e) if e.kind() == NotFound => continue, Err(e) => return Err(StateError::RecoverScan(state_path.clone(), e)) }` — the non-NotFound arm returns Err for the WHOLE scan. Empirically verified: pool with one healthy slot + one slot whose `.git` is a self-referential symlink (ELOOP) returned `Err: ... recovery could not scan: Too many levels of symbolic links (os error 62)` — zero entries recovered.

**Go — tham chiếu.**

> state.go:393-403 `recoverOneWorktree` is the single shared resolver: on marker read failure it prints a WARNING and returns `quarantineEntry(slotName, wtPath, err.Error()), true` — recovered, not fatal. state.go:437-439 calls it per-slot inside `recoverCorruptState`; state.go:336 calls the same function inside `recoverMissingStateEntries`, which is why recovery_symmetry_test.go:45-90 can assert both paths behave identically. Pinned by recovery_skip_test.go:41-70 (`TestReadState_RecoverySkipsUnreadableMarkerSlot` — "recovery must not abort because of one broken slot") and :75-92 for the missing-state half. The commit that fixed this asymmetry is 0e87c62/ee8652b-era, i.e. AFTER the v2.1.1 baseline.

**Cách sửa.**

> Extract a `recover_one_worktree(slot_name, wt_path) -> Option<WorktreeEntry>` mirroring state.go:393-403, and replace the `Err(e) => return Err(...)` arm with a quarantine entry carrying the error text. To match Go's `RecoveryError` field you also need that field on `WorktreeEntry`; short term, reuse `RECOVERED_LEASE_HOLDER` and log the reason to stderr as Go does at state.go:396.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> CONFIRMED on both sides; the bucket is right, the severity is not.
>
> GO SIDE — every citation verified. `recoverOneWorktree` at internal/pool/state.go:393-403 is the single shared resolver: on marker read failure it prints a WARNING to stderr and returns `quarantineEntry(slotName, wtPath, err.Error()), true` at line 397 — i.e. recovered-as-damaged, not fatal. It is called per-slot inside `recoverCorruptState` at state.go:435 (claim said 437-439; the call is 435, append 436 — immaterial) and inside `recoverMissingStateEntries` at state.go:336, which is why `recovery_symmetry_test.go:45-90` can assert both paths agree. `RecoveryError` is a real field at state.go:59-67. `recovery_skip_test.go:41-70` (`TestReadState_RecoverySkipsUnreadableMarkerSlot`) pins the corrupt half with the explicit string "recovery must not abort because of one broken slot"; :72-91 pins the missing-state half.
>
> RUST SIDE — gap is real. `recover_corrupt_state` at crates/treehouse-core/src/state.rs:232-236 returns `Err(StateError::RecoverScan(...))` for the WHOLE scan on any non-NotFound metadata error. I reproduced the exact arm logic standalone against a healthy slot plus a self-referential `.git` symlink: slot 1 recovered=true, slot 2 aborts the entire scan with "Too many levels of symbolic links (os error 62) (FilesystemLoop)". Zero entries recovered. Two aggravating facts the finder missed: (1) `WorktreeEntry` (state.rs:87-115) has NO `recovery_error` field, so Rust cannot represent Go's damaged-slot state even if it wanted to — `grep -rn "recovery_error\|RecoveryError" crates/` returns nothing; (2) `recoverMissingStateEntries` does not exist anywhere in crates/ — the missing-state path at state.rs:160 just returns `State::default()`, so orphans are never recovered there either. So the two-path symmetry Go guarantees is absent on both halves.
>
> WHY UPSTREAM_DRIFT (not P0_PARITY) — the decisive fact. The Rust code is a FAITHFUL port of the v2.1.1 baseline the plan targets. At `git show v2.1.1:internal/pool/state.go:123` Go did precisely the same thing: `os.Stat(.git)` and on `!os.IsNotExist(err)` it `return State{}, fmt.Errorf(...recovery could not inspect %s...)` — fail-closed, identical semantics. `git log -S recoverOneWorktree` shows it was introduced by ee8652b (PR #134), and tag counts confirm v2.1.1=0, v2.2.0=0, v2.2.1=0, v2.3.0=0, v3.0.0=4. So the refinement landed after the port's stated baseline. Legitimate lag, not a port bug. (Minor correction to the claim's prose: the commit is ee8652b, not 0e87c62 — 0e87c62 is the unrelated .worktreeinclude feature; the "after v2.1.1" conclusion still holds.)
>
> WHY DOWNGRADED TO MEDIUM. The port plan at docs/rust-port-plan.md:61 and :533 explicitly specifies "fail closed if the scan can't complete" / "fail closed if scan fails" — the Rust behavior is the PROMISED CONTRACT, implemented as designed, not a defect. The failure is loud, not silent: it surfaces as `PoolError::State` and exits non-zero (main.rs:328-330 for prune; every command in pool.rs/destroy.rs/prune.rs/gc.rs/doctor.rs does the same `.map_err(PoolError::State)`). It fails in the SAFE direction — refusing to return partial state rather than guessing ownership. It requires a compound precondition: the state file must ALREADY be corrupt/truncated AND a slot marker must be unreadable. No data loss, no wrong worktree deleted, nothing destructive — the user gets an actionable error. A command that correctly refuses to act unsafely is not "broken"; this is a robustness/feature gap in a rare path (missing per-slot quarantine + RecoveryError reporting), which is exactly MEDIUM.

</details>

---

### M-023 — Restore the global `--root` flag and the `TREEHOUSE_ROOT` environment variable

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | CLI surface — flags |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> Root redirection by flag or env is the documented way to keep a pool off `$HOME`, put it in-project (`--root .`), or point a shared pool at a specific disk. In Rust the only lever is the differently-named `--env-path`, and any user or CI image that sets `TREEHOUSE_ROOT` gets it silently discarded — the pool is written to the default location with no warning, which can fill a disk the user explicitly tried to keep clear.

**Rust — hiện trạng.**

> `grep -rn 'TREEHOUSE_ROOT|root_flag|ResolveRoot' crates/` → zero hits; the only env vars the Rust tree reads are TREEHOUSE_DIR, TREEHOUSE_LEASE_HOLDER, TREEHOUSE_LEASE_TTL, TREEHOUSE_NO_UPDATE_CHECK. crates/treehouse/src/cli.rs:30-32 offers `--env-path <DIR>` instead. Verified: `--root /tmp/whatever` → `error: unexpected argument '--root' found`; `TREEHOUSE_ROOT=/tmp/th-probe3-poolroot treehouse status` silently ignored the variable and created nothing there.

**Go — tham chiếu.**

> cmd/root.go:62-63 `rootCmd.PersistentFlags().StringVar(&rootFlag, "root", "", "Worktree root directory, overriding TREEHOUSE_ROOT and config; relative paths (e.g. \".\" for an in-project pool) resolve from the repo root")`, consumed through `config.ResolveRoot(rootFlag, cfg)` by every repo-scoped command (get.go:152, enter.go:55, status.go:56, lease.go:64, prune.go:79, return_cmd.go:565). Added in v2.2.0 (#93).

**Cách sửa.**

> Add a global `--root <DIR>` alongside (or renaming) `--env-path`, give it the Go precedence (`flag > TREEHOUSE_ROOT > treehouse.toml root > ~/.treehouse`), and resolve relative values like `.` against the repo root rather than the process cwd.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Attempted refutation failed on every axis; the gap is confirmed and in fact worse than claimed.
>
> GO SIDE (all citations verified by direct read): cmd/root.go:62-63 defines the persistent `--root` flag with the exact help string quoted. internal/config/config.go:63 declares `const RootEnvVar = "TREEHOUSE_ROOT"`; config.go:110-117 implements ResolveRoot with flag -> env -> config precedence, matching the documented contract. All claimed consumption sites exist: get.go:152, enter.go:55, status.go:56, lease.go:64, prune.go:79, return_cmd.go:564, plus destroy.go:210 (which the claim omitted). Only nit: the claim cites return_cmd.go:565 but the actual call is on line 564.
>
> RUST SIDE (absence proven by aggressive grep, not inference): `grep -rhoE 'TREEHOUSE_[A-Z_]+' crates/*/src/` returns only TREEHOUSE_DIR, TREEHOUSE_LEASE_HOLDER, TREEHOUSE_LEASE_ID, TREEHOUSE_LEASE_TTL, TREEHOUSE_NO_UPDATE_CHECK. TREEHOUSE_ROOT appears nowhere in the entire repo (excluding target/). Full flag inventory of cli.rs shows no `--root` and no visible/hidden alias; the only global args are --format and --env-path. Empirical: `treehouse status --root /tmp/x` exits with clap usage error code 2; `TREEHOUSE_ROOT=/tmp/thv/envroot treehouse status` created nothing at that path.
>
> STRONGER THAN CLAIMED: the implied substitute `--env-path` is inert, not merely differently named. cli.rs:246-315 constructs a CliEnv overriding pool_root(), but pool.rs:121 calls the NON-env `resolve_pool_dir`, which hardcodes home_dir() at config.rs:163-166. The env-aware resolve_pool_root_with_env/resolve_pool_dir_with_env have only test callers (config.rs:552-578), so there is no production path that honors an injected pool root. Verified empirically: `--env-path /tmp/thv2/pool1 get --lease` still wrote to $HOME/.treehouse/repo-63812c and /tmp/thv2/pool1 was never created. Net effect: the Rust port has NO working per-invocation pool-root override at all; only the static treehouse.toml `root` key works (I confirmed both absolute and relative "." cases create the expected pool).
>
> BUCKET: UPSTREAM_DRIFT is correct and I confirm it independently. CHANGELOG.md:73-78 places the feature in v2.2.0, after the v2.1.1 baseline named in docs/rust-port-plan.md:5, so it is not P0_PARITY. I also checked it is not P1_UPGRADE: zero matches for --root / TREEHOUSE_ROOT / env-path in docs/rust-port-plan.md, so the plan never promised it.
>
> SEVERITY CORRECTED HIGH -> MEDIUM. Per the stated rubric, HIGH requires a broken core command or a missing safety invariant; neither applies. Every command works correctly at the default location, nothing crashes, no wrong worktree is deleted, and prune/destroy safeguards are untouched. This is an override-surface feature gap, which is MEDIUM by definition. The one argument behind HIGH -- an env var silently ignored -- fails toward the safe, well-tested default rather than toward corruption, so it does not reach HIGH.

</details>

---

### M-024 — Restore the seven acquisition flags on `get` and the matching AcquireOptions fields

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | CLI surface — flags |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |
| **Gộp từ** | gộp cả `--root` (`cmd/root.go:62-63`, #93) |

**Triệu chứng / tác động.**

> Every acquisition-shape control from Go is gone. Concretely broken in Rust: cutting from a non-default branch (`--base`/`base_branch`, v3.0.0 #119), branch-per-agent workflows (`-b`, #150), offline/air-gapped acquires (`--no-fetch`, v2.2.0), seeding ignored files from `.worktreeinclude` or `--include-file` (#129/#131), `--unique-leaf` (#126), a templated `worktree_path` (#140), and APFS copy-on-write sharing (#153). An agent fleet script written against Go cannot be ported by flag rename — the features do not exist.

**Rust — hiện trạng.**

> crates/treehouse/src/cli.rs:76-90 `struct GetArgs` has only `lease`, `lease_holder`, `ttl`, `json`. crates/treehouse-core/src/pool.rs:49-54 `struct AcquireOptions { branch, lease }` — and `branch` is documented "Override the branch to reset to", i.e. it is Go's `BaseBranch`, not Go's `-b` branch creation. crates/treehouse/src/main.rs:144 hardcodes `pool.get(&AcquireOptions::default())`, so even that field is unreachable from the CLI. Verified: every one of `--branch --no-fetch --base --unique-leaf --worktree-path --include-file --apfs-sharing` is rejected with `error: unexpected argument`.

**Go — tham chiếu.**

> cmd/get.go:94-105 registers `--lease`, `--lease-holder`, `--json`, `--no-fetch`, `--branch/-b`, `--base`, `--include-file`, `--unique-leaf`, `--worktree-path`, `--apfs-sharing`, each wired into `pool.AcquireOptions{SkipFetch, Branch, BaseBranch, WorktreePath, IncludeManifest, UniqueLeaf, APFSSharing}` at get.go:161-169.

**Cách sửa.**

> Extend `GetArgs` and `AcquireOptions` with `no_fetch: bool`, `base: Option<String>`, `branch_name: Option<String>` (distinct from the existing reset-to `branch`), `include_file: Option<PathBuf>`, `unique_leaf: bool`, `worktree_path: Option<String>`, `apfs_sharing: Option<String>`, plus the Go guard strings (`--json requires --lease`, `--apfs-sharing requires off or fresh`, `--branch requires a non-empty branch name`) and the config/env fallbacks TREEHOUSE_UNIQUE_LEAF / TREEHOUSE_WORKTREE_PATH / TREEHOUSE_APFS_SHARING.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> Attempted refutation on four axes; all failed to overturn the finding. (1) Go citations are exact: cmd/get.go:98-104 registers all seven flags, cmd/get.go:161-169 wires them into pool.AcquireOptions{SkipFetch, Branch, BaseBranch, WorktreePath, IncludeManifest, UniqueLeaf, APFSSharing}, struct at internal/pool/pool.go:96-118. (2) Rust genuinely lacks it, not a naming miss: grepping all of crates/ for no_fetch|no-fetch|SkipFetch, unique_leaf|unique-leaf, apfs, include_file|worktreeinclude|includeManifest, and base_branch|BaseBranch returns ZERO hits. No config-layer fallback either — TreehouseConfig (config.rs:22-31) has only max_trees, root, hooks, lease_ttl_secs. The worktree_path hits in Rust are unrelated (the acquired worktree's own path in run.rs:49, process.rs:78). Runtime confirmed: `treehouse get --branch foo` -> "error: unexpected argument '--branch' found". (3) The claim's nuanced assertion is correct: Rust AcquireOptions.branch (pool.rs:198-201) resolves the branch used to reset the worktree (pool.rs:224-226), which is Go's BaseBranch (pool.go:100-103), not Go's Branch (pool.go:104-106, "creates and checks out a new Git branch"). Rust therefore has neither field under its correct name. (4) BUCKET IS CORRECT AND DECISIVE: `git show v2.1.1:cmd/get.go` registers ONLY --lease, --lease-holder, --json (lines 42-44), and Rust's GetArgs (cli.rs:76-90) has all three plus --ttl — so every v2.1.1 flag is present and P0_PARITY is ruled out. `git log -S` dates them all after the baseline: --no-fetch in v2.2.0 (164d903), the other six in v3.0.0 (#119 base, #126 unique-leaf, #131 include-file, #140 worktree-path, #150 branch, #153 apfs). The port plan's P1 sections (docs/rust-port-plan.md:502-555) promise stale leases/--ttl, gc, crash recovery, doctor, run, and --format json/toon — none of these flags — so P1_UPGRADE does not apply. UPSTREAM_DRIFT stands. WHERE THE FINDING OVERSTATES: severity HIGH requires "core command broken or safety invariant missing." Neither holds. `treehouse get` still acquires and hands off a worktree correctly via main.rs:144; these are opt-in per-acquisition conveniences. None is a safety invariant: unique_leaf/worktree_path are creation-time naming (Go itself documents them as never moving existing worktrees), apfs_sharing is off by default, include_file seeds ignored files. This is a feature gap, which the rubric places at MEDIUM.

</details>

---

### M-025 — No background update check is ever spawned, so the cache is never written and the notice never fires

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **P0_PARITY** — port bug: Go baseline v2.1.1 đã có, Rust không tương đương |
| **Domain** | updater |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> ~/.treehouse/update-check.json is written only when a user manually runs `treehouse update` (main.rs:650), which additionally does not replace the binary. So the update notice at main.rs:47-53 is unreachable in practice and `treehouse update` degrades to a version-string print. Independently, the Go hardening that keeps the child out of the caller's pooled worktree — so `status` does not report a treehouse-owned process and `return` does not kill it — has no counterpart; adding the spawn naively would reintroduce that hazard.

**Rust — hiện trạng.**

> crates/treehouse/src/main.rs:24-39 implements the `--update-check` CHILD side, but grep over crates/ for `current_exe|setsid|CREATE_NEW_PROCESS|process_group|pre_exec` finds nothing that re-spawns the binary — the only std::process::Command::new in main.rs is line 711, which launches the user's login shell. The consuming notice at main.rs:44-55 depends on ~/.treehouse/update-check.json existing.

**Go — tham chiếu.**

> cmd/root.go:55 `_ = updater.SpawnBackgroundCheck(version)` runs on every invocation. internal/updater/updater.go:170-201 SpawnBackgroundCheck, :209-217 backgroundCheckCommand (inherits TREEHOUSE_NO_UPDATE_CHECK=1), :229-241 detachedWorkingDir, :253-296 outsideEnclosingWorktree/enclosingWorktreeRoot, plus sysproc_unix.go:11-15 (Setsid) and sysproc_windows.go:11-15 (CREATE_NEW_PROCESS_GROUP). Confirmed at the v2.1.1 baseline era: `git show v2.2.0:internal/updater/updater.go` line 168.

**Cách sửa.**

> Add spawn_background_check(): resolve std::env::current_exe() plus fs::canonicalize, spawn with TREEHOUSE_NO_UPDATE_CHECK=1 and Stdio::null(), set setsid via pre_exec on unix and CREATE_NEW_PROCESS_GROUP on Windows, and pick a current_dir() outside the enclosing worktree root before spawning.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> CONFIRMED after aggressive search of both trees. Go side citations are accurate: cmd/root.go:55 `_ = updater.SpawnBackgroundCheck(version)` gated on IsCacheStale, and I additionally verified it existed at the port's stated v2.1.1 baseline (`git show v2.1.1:cmd/root.go` line 50; `git show v2.1.1:internal/updater/updater.go` line 168 = func SpawnBackgroundCheck), so P0_PARITY is correct — this is a port bug, not upstream drift. Rust side genuinely lacks the concept: I read all 317 lines of crates/treehouse-core/src/updater.rs, which exposes only Version::parse, cache_path, is_cache_stale, read_cache, check_latest, write_cache, update_available and their _with_env variants — there is no spawn function of any kind. A grep over the whole crates/ tree for current_exe|setsid|SETSID|CREATE_NEW_PROCESS|process_group|pre_exec|std::process::Command|spawn()|detached|nohup|background_check returns only git subprocesses, the curl call at updater.rs:133, the user login shell at main.rs:711, run.rs (supervising the user's own command, unrelated), and e2e tests; process.rs contains no Command/spawn at all. main.rs:24-39 is the --update-check CHILD handler and is therefore dead code, since no parent ever re-execs the binary with that flag. The is_cache_stale call that does exist (main.rs:644) lives inside cmd_update, not in the PersistentPreRun-equivalent notice path — Go gates the spawn on staleness in PersistentPreRun (cmd/root.go:54-55), while Rust main.rs:44-55 only reads an already-existing cache. TWO CORRECTIONS to the finding: (1) 'the cache is never written' is imprecise — cmd_update at main.rs:650 does call write_cache, but only when the user explicitly runs `treehouse update`; the background/per-invocation path the claim targets genuinely never writes it. (2) Severity is MEDIUM, not HIGH: the background check is a passive advisory notice, not a core command and not a safety invariant, with no data loss, no wrong worktree, and no corruption. It is a real feature gap and is explicitly in the plan's own P0 parity scope (docs/rust-port-plan.md:561 and :571), but does not meet the HIGH bar. Note a distinct and more severe defect surfaced during verification but is NOT part of this claim: cmd_update (main.rs:637-661) is a stub that prints 'Successfully updated' without ever downloading or replacing the binary.

</details>

---

---

## 6. Tier 2 — kiến trúc, tính bằng tuần

### M-026 — Thêm release preconditions **cùng lúc** với `lease` subcommand

| | |
|---|---|
| **Severity** | MEDIUM |
| **Bucket** | **UPSTREAM_DRIFT** — upstream thêm sau v2.1.1, Rust chưa có (lag hợp lệ) |
| **Domain** | Safety invariants — leases and conditional release |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |
| **Gộp từ** | gộp `lease` subcommand (#128) |

**Triệu chứng / tác động.**

> The `get` subshell-exit path releases with no precondition at all. If the slot was taken over while the subshell ran — a `treehouse lease` on a live agent home, or a later acquisition — the exiting session still resets the worktree and clears the new owner's reservation, destroying whatever the new owner was doing. Go added `RequireOwnedByCaller` precisely for this. `RequireUnleased` is also missing, so a bulk `return --all` cannot assert "nobody leased this since I listed it", and there is no `RefuseRecovered` guard.
>
> **Gộp thêm:** finding `Restore the standalone lease subcommand that durably leases an existing worktree by name` (`cmd/lease.go:21-45`, `internal/pool/pool.go:259`, #128) là consumer đầu tiên của preconditions này.

**Rust — hiện trạng.**

> crates/treehouse-core/src/pool.rs:591-594 (`ReleasePreconditions` has ONLY `expected_lease_id` and `expected_lease_holder`); crates/treehouse-core/src/pool.rs:616-641 (`validate_release_preconditions_inner` implements only the two identity checks); `grep -rni "RequireOwnedByCaller|owned_by_caller|RequireUnleased|require_unleased|RefuseRecovered|refuse_recovered" crates/` → 0 hits each; crates/treehouse/src/main.rs:173 (`pool.release(&path.to_string_lossy())?` — the get subshell exit path is fully UNCONDITIONAL)

**Go — tham chiếu.**

> internal/pool/pool.go:932-955 (ReleasePreconditions carries ExpectedLeaseID, ExpectedLeaseHolder, RequireUnleased, RequireOwnedByCaller, RefuseRecovered) validated by internal/pool/pool.go:1111-1150 (`validateReleasePreconditions`) including internal/pool/pool.go:1417-1438 (`checkOwnedByCaller`, which refuses a slot that is now durably leased, already released, or reserved by another session). Go's `ValidateReleasePreconditions` doc (pool.go:967-981) explains why the guarded action must run under the same lock. Landed in CHANGELOG v3.0.0 via #128 (RequireOwnedByCaller) and #143 (RequireUnleased).

**Cách sửa.**

> Add the three fields to `ReleasePreconditions`, port `checkOwnedByCaller` into `pool.rs` (compare `owner_pid`+`owner_started_at` against this process, testing `leased` FIRST since the lease path zeroes the owner fields), and pass `require_owned_by_caller: true` from the get-exit call site at crates/treehouse/src/main.rs:173.
>
> Làm `lease` và `return --all` mà **không** kèm preconditions là hướng nguy hiểm: hai command đó trao quyền release cho bên thứ ba, còn release path hiện không kiểm tra gì.

**Ghi chú.** **Phải ship cùng M-016 (`return <name>`) và M-017 (`return --all`) trong cùng một thay đổi.** Thêm command mà không có precondition này sẽ biến nó thành CRITICAL: `main.rs:173` hiện release slot một cách vô điều kiện, và slot đó có thể đã thuộc về bên thứ ba.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> CONFIRMED REAL, but severity corrected HIGH → MEDIUM. I tried to refute this and could not on existence; I could on severity.
>
> EXISTENCE — every citation is exact, on both sides:
> - Go pool.go:938/942/946/953/957 declares all five fields (ExpectedLeaseID, ExpectedLeaseHolder, RequireUnleased, RequireOwnedByCaller, RefuseRecovered). validateReleasePreconditions at pool.go:1111-1143 checks all three missing ones (1112 RequireOwnedByCaller → checkOwnedByCaller, 1120 RefuseRecovered, 1123 RequireUnleased). checkOwnedByCaller at pool.go:1418-1438 refuses a slot that is now durably leased (1420-1424), already released (1426-1428), or reserved by another session (1436-1438), comparing both PID and start-time.
> - Rust pool.rs:591-594 `ReleasePreconditions` has exactly two fields; validate_release_preconditions_inner at pool.rs:616-641 implements only the identity checks and short-circuits at 620 when both are None. I grepped every synonym across crates/ (owned_by_caller, require_unleased, refuse_recovered, caller, reserved_by, owner_of, session_id) — the only owner-reservation logic that exists is `Reservation`/`owner_alive` in reservation.rs, wired into acquire (pool.rs:481) and destroy/gc two-phase, never into release.
> - main.rs:173 `pool.release(&path.to_string_lossy())?` is indeed the fully unconditional get exit path, as claimed.
>
> BUCKET — UPSTREAM_DRIFT is correct, verified by git, not memory: `git show v2.1.1:internal/pool/pool.go | grep -c` returns 0 for all three fields. Each was introduced after the baseline by a specific commit: RequireOwnedByCaller by b227e59 (#128), RequireUnleased by 99f4db2 (#143), RefuseRecovered by 706ae59 (#156). #128/#143 are in CHANGELOG v3.0.0, #156 in v3.0.1. P1_UPGRADE does not apply either — port-plan §2.4 (docs/rust-port-plan.md:64-67) promises only `--if-lease-id`/`--if-lease-holder` conditional release, which Rust does implement.
>
> WHY MEDIUM, NOT HIGH — all three consumers are themselves absent from the Rust CLI, so the hazard is latent rather than live:
> 1. RequireOwnedByCaller defends against a concurrent `treehouse lease` taking over a live agent home. That subcommand does not exist in Rust: the Command enum (cli.rs:46-75) has zero occurrences of "Lease", and `grep -c` over it returns 0. Rust only leases at acquisition time (`get --lease`, main.rs:109+), where the leasing process is itself the owner — there is no third party who can lease a slot someone else holds. Acquire additionally skips leased/owner_alive entries (pool.rs:481), so a second `get` cannot take over a live session's slot either.
> 2. RequireUnleased/RefuseRecovered are consumed only by bulkReturnPreconditions (return_cmd.go:194-202), which backs `return --all` (#143). Rust's ReturnArgs (cli.rs:101-112) has only force / if_lease_id / if_lease_holder / path — no `--all`, no `--name`.
> 3. The paired HEAD-move hazard is independently absent: Go's guarded DetachWorktree (get.go:196-206) has no Rust counterpart — `detach_and_return` is a no-op stub (main.rs:731-734).
>
> So no current Rust invocation can construct the scenario these preconditions guard. It is a real forward-compatibility gap that must land together with `treehouse lease` and `return --all` (adding those without the preconditions is what would make it CRITICAL), not a live safety-invariant breach. A latent missing-safety-net feature gap is MEDIUM, not HIGH.

</details>

---

### M-027 — `treehouse update` never downloads or replaces the binary but reports success

| | |
|---|---|
| **Severity** | HIGH |
| **Bucket** | **P0_PARITY** — port bug: Go baseline v2.1.1 đã có, Rust không tương đương |
| **Domain** | updater |
| **Confidence** | HIGH |
| **Verify** | 2/2 lens đồng ý |

**Triệu chứng / tác động.**

> A user runs `treehouse update`, sees "🌳 Successfully updated treehouse 0.1.1 -> 0.1.2", and the binary on disk is still 0.1.1. There is no way to self-update and no error ever surfaces. Secondary: the network-failure branch (main.rs:653-656) prints a message and returns Ok(0), where Go returns a wrapped error (exit 1) at cmd/update.go:22-24 — a script gating on exit status reads an unreachable network as success.

**Rust — hiện trạng.**

> crates/treehouse/src/main.rs:635-666 — cmd_update only reads the cache or calls check_latest, then prints "🌳 Successfully updated treehouse {current} -> {latest}". No download, no replace. Grep over crates/ for `sha256|sha2|checksum|atomic_replace|tar::|flate2|zip::|quarantine` returns only the pool-dir hash in git/mod.rs:131 and the doc comment in updater.rs:6 — no implementation.

**Go — tham chiếu.**

> /tmp/th-baseline/treehouse/cmd/update.go:21-37 — updater.CheckLatest(version) then updater.Apply(result); Apply at internal/updater/updater.go:319-370 downloads (downloadToTemp:542), verifies sha256 (verifyChecksum:482-529), extracts (extractTarGz:580 / extractZip:634), removes macOS quarantine (quarantine_darwin.go:8-12), and atomically replaces the exe (atomicReplace:679-733).

**Cách sửa.**

> Port Apply(): download the release asset to a temp file, verify against the release .sha256 sidecar, extract the binary, chmod 0755, then write a temp file in the target's own directory and rename over the target (same-dir temp so the rename stays on one filesystem). Return Err on every failure path.

<details>
<summary>Lý do verify (2 lens đối kháng — giữ nguyên để đối chiếu khi fix)</summary>

> The gap is REAL — I confirmed it on both sides, but the claimed bucket and severity are both wrong.
>
> RUST SIDE (confirmed absent): cmd_update at crates/treehouse/src/main.rs:635-666 only reads the cache or calls check_latest, then at :660-661 prints "Successfully updated treehouse {current} -> {latest}" with no download and no binary replacement. Aggressive synonym grep over the whole crates/ tree returned ZERO hits for: tar::/flate2/zip::/gzip/Archive::new (0), quarantine/xattr/com.apple (0), atomic_replace (0), and no fn apply/fn download/browser_download_url. sha256/sha2 appears only at git/mod.rs:131-133 (the 6-hex pool-dir hash) and in the doc comment at updater.rs:6. No feature flag gates it (crates/treehouse/Cargo.toml exposes only `toon`; treehouse-core adds only `hardening`), and there is no todo!/unimplemented!/FIXME stub. No tar/flate2/zip/reqwest dep exists in the workspace — the machinery was never written. Note the module doc comment at updater.rs:6-7 PROMISES "update downloads the release asset, verifies sha256, and atomically replaces the executable"; only the version-check/cache half was implemented. The false-success line is reachable, not dead code: live check returns a newer tag -> write_cache at :650 -> update_available true at :660 -> prints success at :661.
>
> GO SIDE (citations all verified accurate): cmd/update.go:32 calls updater.Apply(result); Apply at internal/updater/updater.go:319-370 calls downloadToTemp:542, verifyChecksum:482, extractBinary, removeQuarantine (quarantine_darwin.go:8-12, xattr -d com.apple.quarantine), and atomicReplace:679-733. I opened each cited line and confirmed.
>
> BUCKET CORRECTION -> P0_PARITY (claimed BUG_RUST is wrong). I checked the port's baseline instead of trusting the finder: docs/rust-port-plan.md:5 declares baseline Go v2.1.1, and `git show v2.1.1:cmd/update.go` contains the full updater.Apply(result) call with the complete download->verify->extract->de-quarantine->atomic-replace pipeline ALREADY PRESENT at v2.1.1. So this is NOT upstream drift and NOT a P1 item. The plan lists `update` in P0 scope twice (line 561 deliverables, line 571 definition-of-done). P0_PARITY is defined as "behavior the Go v2.1.1 baseline already had; Rust never matched it. This is a port bug" — an exact match. BUG_RUST ("has the concept but implements it wrongly/unsafely") does not fit: there is no subtly-wrong implementation to misclassify, the pipeline is simply absent.
>
> SEVERITY CORRECTION -> HIGH (claimed CRITICAL is wrong). The stated CRITICAL triggers are data loss, wrong worktree deleted, silent corruption, or security — none apply. It in fact fails SAFE: the binary is never replaced, so there is no path to installing an unverified or corrupt binary (no security exposure). What remains is "core command broken," which the rubric assigns to HIGH. The aggravating factor that keeps it from MEDIUM is that the success message is affirmatively false, so a user believes they are on the latest version when they are not — a misleading report, not corruption.
>
> Confidence HIGH: read both codebases, every claim cited and verified, baseline established via git rather than inference.

</details>

---

### M-028 — VCS seam (viết lại kiến trúc)

**Đây là một việc, không phải sáu gap.**

Go đã tách `internal/vcs/` (interface) + `internal/vcs/gitvcs/` + `internal/vcs/jjvcs/` + `internal/fileclone/`, tất cả land cùng một commit `a2e554c` (v2.3.0). Rust vẫn là `GitBackend` phẳng, git-only — `grep -rn "jj\|jujutsu" crates/` → **0 hit**.

Làm seam này thì **một lượt** thu được:
- markerless/flavor check trong acquire (M-020)
- hardening markerless (đã vá ở M-002 — khi có seam thì làm đúng cách thay vì vá cục bộ)
- `--apfs-sharing` (#153)
- `.worktreeinclude` seeding (#129) + per-acquisition seed manifest (#131)
- plumbing cho `--include-file` và `--unique-leaf`

Vá từng mục riêng lẻ trên `GitBackend` phẳng nghĩa là phải làm lại `pool.rs`, `prune.rs`, `destroy.rs`, `gc.rs`, `run.rs` **ba lần**.

**Bằng chứng Rust hoàn toàn không có:**
| Khái niệm | Grep trong `crates/` |
|---|---|
| `jj` / `jujutsu` | 0 hit |
| `apfs` / `clonefile` / `fileclone` | 0 hit |
| `base_branch` | 0 hit |
| `unique_leaf` | 0 hit |
| `worktree_path` | 0 hit |
| `gitignore` (`.git/info/exclude`, in-project pool) | 0 hit |
| `.worktreeinclude` | 0 hit |

> 21 hit "seed" trong Rust **không phải** seeding — tất cả là test double `TreehouseEnv::seed_file` tại `env.rs:199`.

---

## 7. Bị refute (đã kiểm tra, không phải gap)

**Claim:** Rust thiếu `version` field trong state file ⇒ Go sẽ quarantine toàn bộ worktree.

**Kết luận: sai.** `internal/pool/state.go:143` có carve-out tường minh:
`legacy := s.Version == 0 && (keyErr == nil || errors.Is(keyErr, fs.ErrNotExist))` → `if legacy && !hasSeedState(*wt) { setSeedInventory(wt, nil, true); continue }`. Hai disjunct là exhaustive nên luôn fire với file do Rust ghi; khối quarantine tại `:150-161` không bao giờ tới được. Comment tại `:139-142` gọi đúng case này, và `upgrade_test.go:130` (`TestUpgrade_AdoptsTwoXRewriteBesideKey`) ghim nó lại.

Phần còn lại đúng: HMAC seed-inventory guarantee của Go bị vô hiệu trên đúng pool đó — **wire-format nit mức LOW**, không phải CRITICAL.

---

## 8. Lead chưa verify (54) — coi là gợi ý

Tìm được **83 finding**, verify 30 cái top theo severity → 29 sống. 54 cái dưới đây **chưa qua lens đối kháng**. Trước khi fix, hãy tự đọc code.


| Sev | Bucket | Lead |
|---|---|---|
| CRITICAL | UPSTREAM_DRIFT | No state `version` field: Rust silently downgrades a Go v3 pool to version 0, which makes Go quarantine every worktree as recovered |
| HIGH | UPSTREAM_DRIFT | install.ps1 cannot replace a running treehouse.exe (the bug class Go fixed in v3.0.0 #121) |
| MEDIUM | BUG_RUST | Both installers skip checksum verification when the sidecar is missing instead of failing |
| MEDIUM | BUG_RUST | `Version::parse` rejects partial versions, so the update notice silently never appears |
| MEDIUM | BUG_RUST | Update check shells out to `curl` with no timeout instead of using an HTTP client |
| MEDIUM | BUG_RUST | Check release preconditions before prompting to discard uncommitted changes in `return` |
| MEDIUM | BUG_RUST | Implement the worktree detach in `detach_and_return` instead of returning Ok(()) |
| MEDIUM | BUG_RUST | Make `treehouse update` actually apply the update instead of printing success |
| MEDIUM | BUG_RUST | Restore the removableWorktreeContainer guard: pool-containment, canonicalization, and reject "." |
| MEDIUM | BUG_RUST | get's post-reset re-validation does not actually check the reservation |
| MEDIUM | P0_PARITY | Port the in-project pool exclusion (.gitignore self-ignore + info/exclude) into get |
| MEDIUM | P0_PARITY | Fall back to the current directory when `return` is given neither a path nor `$TREEHOUSE_DIR` |
| MEDIUM | P0_PARITY | Map flag-parsing and argument errors to exit 1, matching Go's reserved-code policy |
| MEDIUM | P0_PARITY | Report the actual available slot names in `enter`'s not-found error instead of always claiming the pool is empty |
| MEDIUM | P0_PARITY | Restore destroy's skip hints, the "Skipped N worktrees:" header, and the other-flavor migration framing |
| MEDIUM | P0_PARITY | Add the missing `--include-leased` + `--all` refusal on destroy |
| MEDIUM | P0_PARITY | Restore the branch/recovery columns in `status` human output and the base-branch header line |
| MEDIUM | P0_PARITY | Match Go's `status --json` schema: add the always-present `branch` key and Go's field order |
| MEDIUM | P0_PARITY | Match Go's `init` message and emit the full documented config template |
| MEDIUM | P0_PARITY | Read and heal pool state under the state lock when building the destroy target list |
| MEDIUM | P0_PARITY | prune removes only the leaf worktree directory and has no pool-ownership guard on the removal path |
| MEDIUM | P1_UPGRADE | Port plan §8 P0 gate "Go tests → Rust tests" is roughly a quarter met: 166 Rust tests vs 654 Go tests |
| MEDIUM | P1_UPGRADE | Rust emits an `expires_at` key that Go has no field for — Go silently discards it, so a TTL lease becomes permanent after any Go write |
| MEDIUM | UPSTREAM_DRIFT | Config schema silently ignores five upstream keys with no warning |
| MEDIUM | UPSTREAM_DRIFT | `TREEHOUSE_ROOT` and the `--root` flag are absent — the whole root-precedence chain is gone |
| MEDIUM | UPSTREAM_DRIFT | No exit code 3 for a dirty worktree that was not returned |
| MEDIUM | UPSTREAM_DRIFT | Release matrix drops windows/arm64 and there is no Nix packaging path |
| MEDIUM | UPSTREAM_DRIFT | Introduce a VcsBackend seam so jj (and future backends) can be added without rewriting call sites |
| MEDIUM | UPSTREAM_DRIFT | Add Jujutsu workspace backend with slot-flavor dispatch |
| MEDIUM | UPSTREAM_DRIFT | Implement .worktreeinclude seeding of ignored files into a new or recycled worktree |
| MEDIUM | UPSTREAM_DRIFT | Detect squash-merged worktrees via path-scoped tree comparison when ancestry is absent |
| MEDIUM | UPSTREAM_DRIFT | Support a configurable base branch for get, validated before any worktree is created or reset |
| MEDIUM | UPSTREAM_DRIFT | Wait for SIGKILLed processes to be reaped before returning from terminate |
| MEDIUM | UPSTREAM_DRIFT | Filter the caller's own process tree out of the processes status reports |
| MEDIUM | UPSTREAM_DRIFT | Resolve the owning repository per worktree in destroy instead of once for the whole pool |
| MEDIUM | UPSTREAM_DRIFT | status lacks the unverified/damaged classes and mis-reports a failed process scan as a quiet slot |
| MEDIUM | UPSTREAM_DRIFT | No squash-merge detection: an unmerged slot parked on its recorded base can never be reclaimed by prune or destroy |
| MEDIUM | UPSTREAM_DRIFT | Configurable base branch for get is absent: AcquireOptions.branch silently changes the default branch instead |
| MEDIUM | UPSTREAM_DRIFT | Worktree placement is hardcoded to <pool>/<slot>/<repo>: no unique-leaf option and no path template |
| MEDIUM | UPSTREAM_DRIFT | No --no-fetch: every get hard-fails offline instead of falling back to local refs |
| MEDIUM | UPSTREAM_DRIFT | Corrupt-state recovery marks every slot leased forever; the automatic safe-release pass is missing |
| MEDIUM | UPSTREAM_DRIFT | No auto-free of proven-safe quarantined slots — `recoverQuarantinedEntries` has no Rust counterpart, so recovered pools stay permanently full |
| MEDIUM | UPSTREAM_DRIFT | Recovery marker check is git-only (`.git`) — jj worktrees are skipped entirely by Rust's recovery scan |
| MEDIUM | UPSTREAM_DRIFT | No `version` guard on read — Rust cannot detect a future-version state file and will happily clobber it |
| LOW | BUG_RUST | Report the actionable --include-* flag and re-measured size on phase-2 skips |
| LOW | P0_PARITY | Fix `--version` output and the update-notice arrow to match Go byte-for-byte |
| LOW | P1_UPGRADE | Reject `--format` on init/update/enter instead of accepting and ignoring it |
| LOW | UPSTREAM_DRIFT | Repo-level `[hooks]` are discarded with no warning, unlike Go |
| LOW | UPSTREAM_DRIFT | Reconstruct the full vcs.Backend contract; port SkipFetch, IsWorktreeSafeToReset and the seeded-path reset variants |
| LOW | UPSTREAM_DRIFT | Prune stale worktree registrations before adding a new worktree in get |
| LOW | UPSTREAM_DRIFT | Report the checked-out branch and detached state per pool slot in status |
| LOW | UPSTREAM_DRIFT | Add opt-in APFS clonefile sharing for freshly created worktrees |
| LOW | UPSTREAM_DRIFT | Start the subshell as an interactive login session (`-i -l`) for bash/fish/zsh |
| LOW | UPSTREAM_DRIFT | prune does not run the vcs.PruneWorktrees stale-registration cleanup before worktree add |

**Hai cái đáng soi trước** (severity cao nhất trong nhóm này):

- **HIGH** — `install.ps1:191-196` không thể thay binary đang chạy. `Move-Item -Force $binPath $tmpInstall` rồi `Move-Item -Force $tmpInstall $finalPath`; không có rename-aside, không rollback. Go đã sửa đúng lớp bug này ở v3.0.0 #121 (`internal/updater/updater.go:722-730`, `:735-753` — backup name unique theo từng lần thử rồi rename target đang khoá sang bên cạnh). Chưa verify trên Windows thật.
- **MEDIUM** — non-UTF-8 path degrade thành chuỗi rỗng tại `crates/treehouse-core/src/git/shell.rs:297,307,315` (`path.to_str().unwrap_or("")`), git sẽ resolve ngược lại cwd. Đáng có một issue riêng cho Unix.

---

## 9. Test coverage

| | Go v3.1.0 | Rust |
|---|---|---|
| Test files | ~63 | 2 integration (`e2e.rs` 230 dòng, `common.rs`) + 26 inline module |
| Test funcs | ~654 | ~166 `#[test]` |

`e2e.rs` 230 dòng so với `internal/pool/pool_test.go` ~4700 dòng của Go.

Rust **không có** analogue cho:
- `clone_identity_jj_test.go`
- `destroy_hook_matrix_test.go`
- `recovery_symmetry_test.go`
- `worktree_path_test.go`

**Đáng chú ý:** 4/4 finding CRITICAL ở trên hiện **không có test Rust nào bảo vệ**. Coverage gap đúng chỗ nguy hiểm nhất.

---

## 10. Method & caveat

**Đã làm:** 6 finder theo domain (CLI surface / state wire / pool semantics / VCS layer / safety invariants / config+tests+delivery) đọc song song cả hai repo, mỗi finding bắt buộc trích dẫn `file:line` cả hai vế; finding "thiếu ở Rust" bắt buộc grep cả cây `crates/` kèm từ đồng nghĩa. Sau đó mỗi finding qua 2 lens đối kháng (existence / impact), mặc định `refuted=true` nếu phân vân. Bucket bị sửa trong quá trình verify.

**Chưa làm / rủi ro còn lại:**

- **Không chạy test suite nào.** Không `cargo test`, không `go test`. Kết luận từ đọc source + các probe binary dựng lên, chạy, rồi xoá.
- **Toàn bộ verify runtime trên darwin.** Các mục Windows (`MOVEFILE_WRITE_THROUGH`, hành vi `install.ps1` khi exe đang chạy) dựa trên đọc code + ngữ nghĩa Win32, **không** thực thi.
- **jj backend, `internal/fileclone`/APFS, engine `.worktreeinclude` chưa trace** — flag chắc chắn thiếu, nhưng chưa xác minh core có giấu máy gì dưới tên khác không. (Xác nhận: 21 hit "seed" của Rust đều là test double.)
- **Tag `v2.1.1` vắng trong clone baseline.** Release commit `939cb59` vẫn đọc được, mọi call `P0_PARITY` đều neo vào `git show 939cb59:…` hoặc `git log -S`. Hai domain không đọc được baseline trực tiếp và phải bracket — nếu một phân tách P0-vs-drift nào mang tính quyết định, hãy verify lại trên clone đầy đủ.
- **Tiền đề chưa kiểm chứng:** ba finding MEDIUM (mất field state, và trọng số reachability của markerless/clone-identity) giả định Go binary và Rust binary **dùng chung một pool**. Không test hay doc nào ở hai repo exercise điều này. Nếu thực tế hai binary không bao giờ trộn, finding wire-format rớt xuống **LOW** và phân tích shared-pool thu hẹp còn case in-project pool.
