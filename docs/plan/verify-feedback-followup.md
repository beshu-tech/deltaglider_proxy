# Plan: triage + act on expert feedback for the Verify (parity) background job

## Triage (against CURRENT main, not the reviewer's snapshot)

The reviewer is sharp but two "Warnings" are **already fixed** in the Phase-4
follow-up commits they didn't see:

- **#4 cancel tail blind spot** — DONE. `check_cancel()` runs at both resolve
  boundaries (`parity.rs:885,899`) AND a terminal-write guard re-reads status
  before `parity_result_done` (`replication.rs:360`), settling `cancelled`
  instead of overwriting with `done`. Exactly the reviewer's recommended fix.
- **#2 "full-row load per page"** — DONE. The per-page cancel check already uses
  the cheap status-only `parity_status()`, never `parity_result_load`.

Genuinely-open items, and the verdict on each:

| Item | Verdict |
|---|---|
| #1 HA double-scan (no `trigger_config_sync` after lease acquire) | **FIX** — 1 line, matches convention |
| #2 per-page mutex tax (contention half) | **FIX (light)** — AtomicBool cancel + throttle progress writes |
| #3 leaked lease on task-abort | **SKIP** — reviewer agrees it's acceptable; boot-reconcile + TTL recover |
| Arch#1 shared BackgroundJob abstraction | **DO (last, reversible)** — user opted in |
| Arch#4 PARITY_VERSION test barrier | **FIX** — small, kills future `sleep` smell |
| nits (temp table / HMAC log / orphan-ref) | one doc line for orphan-ref divergence; rest SKIP |

User chose **"Everything incl. abstraction"** → all three tiers, sequenced
high-value-first, abstraction last.

## Build order (each its own commit straight to main; per-phase gate before push)

### T1 — HA sync after lease acquire (#1) — ~1 line
`src/api/admin/replication.rs::verify`: after a successful
`parity_try_acquire_lease`, call the same `trigger_config_sync` the replication
mutations use, so instance A's `running`+lease is visible to B before B's 5-min
poll. Closes the double-scan window. (Read-only audit, so worst case today is
wasted work, not corruption — but the convention is "push after every mutation",
and it's one line.)
- Check: grep that `trigger_config_sync` is reachable from the admin state here
  (it's used by the IAM/config mutation paths); wire the same call.
- No new test — it's the existing sync primitive on a new caller; covered by
  `config_sync_ha_test.rs` shape.

### T2 — cheap cancel channel + throttled progress (#2)
The per-page DB round-trip is consistent with the replication worker (which also
locks the shared `Arc<Mutex<ConfigDb>>` per page), so this is an optimisation,
not a bug. Two small changes:
- **Cancel via `Arc<AtomicBool>`**, not a DB read per page. `verify_cancel` still
  flips the DB row to `cancelling` (authoritative for restart/HA/poll), AND sets
  an in-process `AtomicBool` the running scan reads with `Ordering::Relaxed` (no
  lock). The DB `cancelling` row is the durable signal; the AtomicBool is the
  fast in-process shortcut. `ponytail:` comment names this dual-signal.
  - Wiring: a `parity_cancels: Mutex<HashMap<rule, Arc<AtomicBool>>>` on
    AdminState (or reuse an existing registry). The spawn inserts its flag,
    `verify_cancel` sets it, the spawn removes it on settle.
  - `check_cancel` / per-page check read the AtomicBool first; the DB
    `cancelling` check stays as the boot/HA fallback at resolve boundaries only.
- **Throttle progress writes** to every Nth page (e.g. N=8) + always on the final
  page, so a 100k-object scan does ~12 progress writes, not hundreds. The doc
  comment already says "spinner, not live count" — coarse progress is fine.
  `ponytail:` comment names N and the upgrade path.
- Check: a unit test that the AtomicBool short-circuits the scan (drive
  `scan_prefix` with a pre-set flag → returns `Err(CANCELLED)` without a DB hit).

### T3 — PARITY_VERSION observable barrier (Arch#4)
Mirror `IAM_VERSION`/`EXT_AUTH_VERSION`: a `static PARITY_VERSION: AtomicU64`
bumped when a scan settles (done/failed/cancelled), exposed at
`GET /_/api/admin/.../parity/version` (or fold into the existing version
surface). `tests/common` gets a `wait_for_parity_settle` helper. Then the parity
integration test waits deterministically instead of polling the status row.
- Check: the existing `parity_test.rs` switches its poll loop to the barrier;
  still green against MinIO.
- ponytail: only ship the counter + ONE consumer (the test). No speculative
  endpoints beyond the one the test needs.

### T4 — OUTCOME: do NOT extract the abstraction (investigated, declined)

Investigated for extraction; the honest finding is **there is nothing to
extract yet**:
- The genuine shared seam — lease acquire/renew/release, failure-ring prune,
  zombie-scan — is ALREADY factored into `config_db/job_store.rs`. Parity's
  `parity_try_acquire_lease`/`renew`/`release` delegate straight to
  `job_store::*_leader_lease`, exactly like replication + maintenance.
- What remains uniquely in parity's spawn is the ORCHESTRATION (one-shot
  detached task + `tokio::select!` heartbeat + `catch_unwind` + settle). Grep
  confirms this idiom has **exactly one instance** in the codebase
  (`catch_unwind` appears only here + a usage_scanner TEST).
- Maintenance is a different shape entirely: a persistent claim-drain-poll
  loop (queue consumer) with heartbeats interleaved in multi-phase logic and a
  deliberate non-settling `LEASE_LOST` path. Forcing it + parity into one
  driver would gut maintenance's queue semantics or fake-queue parity — the
  bad fit this plan explicitly said to refuse.

A driver wrapping ONE caller is a one-implementation abstraction — barred by
ponytail rung 1 (YAGNI) and the project's own CLAUDE.md anti-pattern list
("no interface with one implementation"). **Revisit when a genuine 2nd
one-shot-detached-leased task lands** (the reviewer's own "4th task" trigger).
Recorded as a `ponytail:` note at the parity spawn so it isn't re-litigated.

### (original T4 plan — superseded by the OUTCOME above) extract `BackgroundJob` / status-FSM abstraction
Three async background flavours now exist: replication/lifecycle (scheduled,
cursor+poison-token), maintenance (one-off, lease-aware requeue), parity
(read-only audit, own FSM). The shared seam is the **lease + status-FSM +
boot-reconcile** triad — all three already delegate lease/zombie-scan to
`config_db/job_store.rs`. The abstraction extracts the *status-FSM driver*
(acquire → set_running → spawn(panic-guarded, heartbeat-renew) → settle terminal
→ release), which today is hand-rolled in `replication.rs::verify` and mirrored
in maintenance/worker.
- **Scope discipline (ponytail):** extract ONLY the spawn-and-settle driver that
  parity + maintenance literally duplicate. Do NOT unify the three *result
  tables* or the scheduling/cursor logic (those genuinely differ). One new
  `src/background/job_driver.rs` (or extend `background.rs`) with a
  `run_leased_job(db, lease_key, ttl, version_counter, async fn -> Result)` that
  owns: lease acquire/renew/release, catch_unwind, heartbeat select-loop,
  terminal settle. Parity's `verify` spawn becomes a call to it; maintenance's
  worker spawn too (if it fits without contortion — if it doesn't, leave it and
  say so).
- **Reversible:** if maintenance doesn't fit the driver cleanly, ship it for
  parity only and note that maintenance stays bespoke — don't force a bad fit.
- Full adversarial swarm + ponytail review before this commit (it touches the
  hot lease path of two subsystems).
- Check: parity + maintenance integration tests stay green; lease-leak boot
  reconcile still works.

## Per-phase protocol (standing instruction)
Each phase: implement → `cargo fmt` + `clippy -D` + `cargo test --lib` +
parity_test (MinIO) + frontend `npm run build` (NOT just `tsc --noEmit` — stale
`.tsbuildinfo` hides errors) → commit straight to main → push → CI green before
next phase. T4 additionally gets a full agent-swarm + ponytail review (per the
phase-review protocol) since it touches shared infra.

## Explicitly NOT doing (and why)
- #3 leaked-lease-on-abort: reviewer agrees it's acceptable; recovered by boot
  reconcile + 30-min TTL. No graceful-shutdown abort handle.
- Unifying the three result tables / scheduling logic: real differences, not
  duplication. The abstraction is the *driver*, not the *data*.
- `_parity_live` temp-table drop / HMAC-prefix log: harmless, skip.
- domain re-home (`replication/parity.rs` → `src/audit/`): premature; revisit if
  lifecycle/encryption parity ever land.

## Critical files
- `src/api/admin/replication.rs` (T1 sync, T2 cancel flag, T4 driver call)
- `src/replication/parity.rs` (T2 AtomicBool read + progress throttle)
- `src/api/admin/mod.rs` (T2 cancel registry on AdminState, T3 version surface)
- `src/iam/mod.rs` pattern → new `PARITY_VERSION` (T3)
- `tests/common/mod.rs` + `tests/parity_test.rs` (T3 barrier)
- `src/background.rs` or new `src/background/job_driver.rs` (T4)
- `src/maintenance/worker.rs` (T4, only if it fits cleanly)
