# DeltaGlider Proxy — Golden Roadmap

_Last updated: 2026-07-09 (v2, after 5-agent adversarial review of v1) · Owner: product + eng lead_

DeltaGlider Proxy is an S3-compatible proxy that transparently delta-compresses versioned binary artifacts — clients speak plain S3, the proxy silently deduplicates against a per-prefix baseline, and an embedded admin GUI + docs ride on the same port. This roadmap optimizes, in strict priority order, for **usability**, **responsiveness**, **correct browser back-button behaviour**, and **simpler copy & UX**. HA and architecture correctness is real work but explicitly secondary — it lands in Tier 4. Every item below is drawn from verified source findings, live browser evidence against a running 6262-object instance, or the HA audit backlog, and the whole plan was re-reviewed adversarially (5 parallel reviewers) on 2026-07-09 — corrections from that review are folded in and marked _(rev)_.

Effort tags: **S** = <½ day · **M** = ~1–2 days · **L** = multi-day / cross-cutting.

---

## Tier 0 — Flagship UX: make the app feel ALIVE

The verify-progress panel is the marquee embarrassment: on a real scan of 6262 objects the whole thing finished in **1.2s** but the UI polled the status endpoint only **once**, saw "Starting scan…", and then the job was done. _(rev)_ Precision: the panel is not literally frozen — a pulsing halo + spinner already render (`components/jobs/VerifyTab.tsx:258-288`) — the failure is that **the number never moves**. On a slow scan it's the opposite: the counter freezes, then jumps by thousands. Both are the same root cause (coarse server flush × slow poll × zero client interpolation), and both are fixed together below.

### 0.0 — Cut v1.12.2 **(S)** _(rev — was missing)_

Three CI-green commits sit unreleased on main: `663fcf3` SigV4 dedup (s3s sole signature authority), `76b07ae` dead-backend listing cooldown + per-listing timeout (user-facing: a down backend no longer hangs listings), `7c0f3f2` replication_target_only alias-hole fix. Release them before starting new work so the baseline is clean.

### 0.1 — Verify progress that never looks dead **(L, split into 4 shippable parts)**

**Problem (verified):** Live capture got only three data points across the entire scan: `[0.0s scanned=0] [0.8s scanned=6262] [1.2s done scanned=12524]`. Three compounding causes, all confirmed in source:
- **Server flushes on page-count, not time.** `PROGRESS_FLUSH_EVERY_N_PAGES = 8` (`src/replication/parity.rs:55`), gated by `should_flush(page_idx, 8)` at `parity.rs:943` — page 0 flushes `seen=0`, then nothing until ~8000 objects later. The counter is structurally guaranteed to read 0 then jump.
- **Poll reuses the generic 2s constant.** `POLL_MS = 2000` (`queries/jobs.ts:19`); `useParityStatus` polls at that rate only while running. A sub-2s scan gets one poll.
- **Client paints `progress_scanned` verbatim, no tween.** `components/jobs/VerifyTab.tsx:302-306` feeds the raw field straight into the bar; `useObjRate` needs ≥2 forward samples so it renders `—/s` and never animates.
- **Plus a stuck first frame:** the "Starting scan…" copy is gated on `scanned > 0` (`VerifyTab.tsx:302` and `:674`), so a fresh running job that reads `scanned=0` shows a dead label under the spinner.

**The fix (4 parts, ship in order):**

1. **Finer server flush** *(S)* — _(rev — mechanism corrected)_ A flush is NOT free: `flush_scanned` (`parity.rs:117-121`) takes the **global ConfigDb mutex** (shared with IAM/admin and the status poller) and runs one UPDATE; the DB has no WAL. And the per-object loop is pure in-memory over an already-fetched 1000-key page — the latency lives in the per-page network list call, so "mid-page flush" is meaningless. Correct design: a **wall-clock throttle (≥250ms) checked at the existing page-boundary flush site** (`parity.rs:943`) — flush when EITHER 8 pages OR 250ms elapsed. Caps at ≤4 writes/s. Keep `FLUSH_EVERY_N_HEADS = 500` (`parity.rs:843`) as is.
2. **Faster verify poll** *(S)* — Split a dedicated `VERIFY_POLL_MS` out of the generic `POLL_MS`; use it in `useParityStatus` only (`useJobs` untouched). _(rev)_ **800–1000ms, not 600** — the client can't observe updates faster than the server's 250ms writes, and each poll takes the same no-WAL DB mutex the flusher uses; 600ms buys nothing but contention.
3. **Client-side count-up tween** *(M)* — _(rev)_ **The exact rAF count-up already exists in `DeltaSavingsChip.tsx:31-45`** (cubic ease-out, cancelAnimationFrame cleanup) — extract it into `useTweenedCount` rather than writing a new one. Two hard requirements: **honor `prefers-reduced-motion`** (matchMedia short-circuit — the codebase only respects it in CSS today), and **snap, don't ease, when the target drops below the displayed value** (reverify resets `progress_scanned` to 0; easing 6262→0 looks like un-counting). Apply in `LoadingBlock` and `ReverifyBanner`. _(rev)_ **This part is LOAD-BEARING, not polish**: PureMirror fast scans have no HEAD phase, so the tween is the ONLY thing that animates the flagship 1.2s case — do not ship parts 1+2+4 without it.
4. **Fix the fast-scan stuck frame** *(S)* — Gate the "Starting scan…" fall-through on `scanned != null` (not `> 0`) at `VerifyTab.tsx:302` and `:674`; when running with `scanned == 0`, show an indeterminate heartbeat. (Safe: `progress_scanned` is 0, never null, for a running row — undefined only pre-first-poll.)

**Plus one residual _(rev — was missing)_:** the poller stops the instant status leaves `running`, so the final verdict appears up to a full poll-interval late. Add one terminal re-poll (or invalidate the query on the existing `PARITY_VERSION` bump, `parity.rs:74-85`) so "done" lands immediately.

**Already correct — do NOT touch:** the denominator. `set_total` publishes `listed + src_needs + dst_needs` in the same cumulative units as `progress_scanned` (`parity.rs:1173-1175`); `deriveVerifyProgress` clamps `[0,100]` and goes indeterminate when total ≤ 0. The old "8000 of 4000" nonsense cannot recur. The settle write is atomic. Existing `jobs-view-regression-test.mjs` already covers the clamp.

### 0.2 — Loading skeletons + heartbeats on every async panel **(M)**

**Problem:** Beyond verify, several panels render a bare label while data loads. The convergence primitive `StatePlaceholders` (`LoadingState`/`EmptyState`) already exists but isn't applied everywhere.

**Fix:** Audit the admin panels and S3 browser for bare loading states; drop in `LoadingState` skeletons. Anything that polls should show motion whenever a request is in flight. **S** per panel; **M** for the sweep.

---

## Tier 1 — Back-button & deep-linking

The S3 browser is the good citizen: **bucket and prefix are both in the URL** (`useS3Browser.ts:255-280`, `urlState.ts:81-101`), Back correctly switches buckets and folders, and `?q=`/`?object=` round-trip. That is the reference pattern. _(rev)_ But the review found the plan's central lever was **blocked as written** — hence a new mandatory first step:

### 1.0 — Query-string plumbing foundation **(M) — MUST LAND FIRST** _(rev — new, blocker)_

**Problem (verified):** `navigate()` itself preserves query strings fine (`useUrlRouter.ts:62-77` re-reads `window.location.search`). But **`buildViewUrl` (`urlState.ts:145-149`) cannot emit a query at all**, and worse, the AdminPage canonicalizer (`AdminPage.tsx:139-141`) fires `navigate(buildViewUrl(...), {replace:true})` whenever the raw path differs — its guard is path-only, so it **actively wipes `?job=…` on arrival**. Every fix in 1.1–1.6 silently no-ops until this is fixed.

**Fix:** Extend `buildViewUrl` (or add an admin-URL builder) to carry a query object; make the canonicalizer query-preserving; add the shared admin-query parser mirroring `parseBrowserLocation`'s `?object=` handling. Also build the **direct-load-safe close helper**: `history.back()` only when the overlay was opened by an in-session push; on a direct-loaded/shared deep link (exactly the case 1.1 enables) `history.back()` exits the app entirely — fall back to `navigate(barePath, {replace:true})`. The existing `useS3Browser` closeInspector has this latent bug too; the helper fixes both.

### 1.1 — Job drawer + active tab not in URL **(M) — WORST**

**Problem (verified, live + source):** Opening a job's drawer (Definition/Runs/Failures/**Verify** tabs) at `/_/admin/jobs` changes nothing in the URL and pushes no history entry. `drawerJobId` is local `useState` (`JobsPanel.tsx:208`); the `<Tabs>` is uncontrolled (`JobDrawer.tsx:319`). **Browser Back skips the drawer entirely**, and the Verify tab isn't deep-linkable. On phones the drawer is full-width so Back closing the whole page is especially jarring.

**Fix:** `?job=<id>&tab=<key>` on `/_/admin/jobs` via the 1.0 parser/builder; open → `navigate(...)`, close → the 1.0 close helper; make `<Tabs>` controlled. **M**

### 1.2 — Metrics Monitoring/Analytics toggle in localStorage **(S)**

**Problem (verified):** The view toggle reads/writes `localStorage['dg-metrics-view']` (`MetricsPage.tsx:171-174`, `:294`). Survives refresh but isn't deep-linkable, leaks across tabs, Back never returns to the previous view.

**Fix:** `?view=analytics` on the dashboard path; drop the localStorage read. **S**

### 1.3 — Setup wizard step not in URL **(S)**

**Problem (verified):** `step` is local `useState` (`SetupWizard.tsx:112`). Refresh on step 3 → back to step 0; Back exits the wizard.

**Fix:** `/_/admin/setup?step=2`; wire `prev`/`next` through `navigate`. **S**

### 1.4 — FilePreview double-click preview not in URL **(S)**

**Problem (verified):** `previewObject` is local `useState` (`App.tsx:71`). Back over-navigates out of the folder; refresh loses the preview.

**Fix:** Reuse `?object=…&preview=1`, closed via the 1.0 helper. **S**

### 1.5 — Master-detail selections not in URL **(S)** _(rev — scope widened)_

**Problem (verified):** `navigateToGroup` hands the group to `/access/groups` via component state (`pendingGroupId`, `AdminPage.tsx:344-350`) — no pre-select on direct load. _(rev)_ Same class, previously missed: **UsersPanel `selectedId`** (`UsersPanel.tsx:28`) and **GroupsPanel's own in-panel selection** (`GroupsPanel.tsx:28`) — `?group=<id>` seeding only the initial value still leaves Back broken when you select a different row in-panel.

**Fix:** `?group=<id>` / `?user=<id>`, written on every selection change (replace, not push, to avoid history spam), read on load. **S**

### 1.6 — Modals that Back can't close (low stakes, batch together) **(S each)**

Push a history entry on open (via the 1.0 helper) so Back closes the modal before leaving the page: Inspector share/download modal, destination picker (Copy/Move), YAML import/export + Full-IAM modals.

---

## Tier 2 — Simpler copy & UX

The app leaks internal vocabulary into user-facing surfaces. Every string below was confirmed verbatim at HEAD. Rewrites touch only display text — never code identifiers. _(rev)_ The review found **three rows break CI as written** and two proposed labels are worse than the originals — noted inline. Rows e/g/h also require same-PR updates to `docs/product/**` (the labels appear in CI-parity-checked user docs).

### 2.1 — Before → after copy table **(S each; group into one PR, M total)**

| # | Where (file:line) | Before | After | _(rev)_ collateral |
|---|---|---|---|---|
| a | `AdminPage.tsx:657,665,682` · `PasswordChangeCard.tsx` (×5: 39,53,59,66,76,89) | "Bootstrap password" | "Admin password" (keep `bootstrap` in code + infra docs) | 7 sites, not 5 |
| b | `MetricsPage.tsx:339-340` | "Storage footprint" / "Honest totals…" | "Storage used" | clean |
| c | `BucketScanCard.tsx:462` | "{n} of {N} buckets never scanned — totals exclude them" | "{n} of {N} buckets not measured yet — press \"Scan all\" to include them" (caveat above the number) | clean |
| d | `BucketScanCard.tsx:475` | "Persistent cache · numbers reflect last completed scan" | "Updated at last scan" | clean |
| e | `adminNavigation.tsx:77` | "Trace" | "Rule tester" | update docs: trace-requests.md +5 more |
| f | `adminNavigation.tsx:84` | "Delta efficiency" | "Compression health" | changelog.md only |
| g | `adminNavigation.tsx:141` | "Admission rules" | _(rev)_ NOT "Access rules" — collides with the sidebar group literally named 'Access' (IAM). Use **"Request rules"** or keep as-is. | breaks `admin-nav-tree-regression-test.mjs:37` — update in same PR + 6 docs files |
| h | `adminNavigation.tsx:207` | "Event outbox" | _(rev)_ NOT "Event queue" — the outbox is a source-of-truth log and "queue" collides with the sibling "Event delivery" page. Use **"Event log"** or keep as-is. | 6 docs files |
| i | `components/jobs/VerifyTab.tsx:190,342,565,566` _(rev)_ **+ :605,616** | "Compares logical SHA-256 + size from metadata…" (×6, not ×4) | one plain line: "Checks that every object matches — no downloads." | fix all 6 |
| j | `VerifyTab.tsx:127,229` | "Comparing source and destination…" | keep, pair with Tier 0.1 heartbeat | clean |
| k | `VerifyTab.tsx:521-522` | "…The destination is not an exact mirror." | plain-language equivalent; keep counts | clean |
| l | `admission/middleware.rs:104` | `admission-deny:{matched}` raw in AccessDenied `<Message>` | "Blocked by access rule '{matched}'" | _(rev)_ **breaks `admission_test.rs:512`** (asserts `admission-deny:` prefix) — update test in same PR |
| m | `bucket_policy.rs:399-401` · `coordination/capability.rs:227-231` _(rev — gate.rs citation was spurious, already plain-language)_ | 403 bodies exposing `replication_target_only` / `config_sync_bucket` / "CAS-capable" | plain-language; **keep the doc URL** (contract) | _(rev)_ **breaks 3 tests** (`replication_target_only_test.rs:47,294`, `backend_capability_gate_test.rs:75` assert the identifier) — update in same PR |

**Terminology consistency** *(S)*: percent form for savings ("270% smaller", never "2.7×"); fix `DeltaEfficiencyPanel.tsx:195`; stop exposing `deltaspace`/`prefix` in end-user copy.

### 2.2 — Enter-to-submit on login **(S)**

**Problem (verified):** the password field lacks `onPressEnter` (`AdminPage.tsx:681`); live capture showed Enter didn't submit despite the native `<form onSubmit>`. _(rev)_ **Trap:** `handleLogin` (`AdminPage.tsx:311`) has no in-flight guard — bolting on `onPressEnter` alongside the native submit double-fires `/login` in one tick. The fix is `onPressEnter` **plus an in-flight guard**, then verify with a live Enter press. **S**

### 2.3 — Natural (numeric) sort — shared comparator **(S)** _(rev — widened)_

**Problem (verified):** `ObjectTable.tsx:329` sorts without `{numeric:true}` so `v10` < `v2`. _(rev)_ Fixing only that site makes the table disagree with the raw key list feeding it: `s3client.ts:563,592`, `BucketsPanel.tsx:90,220`, `SynthesizedBlocksPreview.tsx:51` sort the same way. Ship one shared numeric comparator and use it at all 6 sites. **S**

### 2.4 — "0% smaller · 0 B saved" chip edge **(S, trivial)**

Extend the existing guard (`DeltaSavingsChip.tsx:51-53`) to also hide when `pct===0 || savedBytes===0`. **S**

### 2.5 — Error-toast consistency **(S)** _(rev — new)_

`normalizeUiError` is used in only 16 files; ~15+ sites show raw `e.message` (Rust/SDK strings) — `AuthenticationPanel.tsx` ×6, `UsersPanel.tsx:102`, `SetupWizard.tsx`, `useSectionEditor.ts:229`. Route them all through the normalizer. **S**

---

## Tier 3 — Responsiveness & performance

### 3.1 — HEAD fan-out: page-batch polish only **(S)** _(rev — DOWNGRADED from M)_

**Corrected finding:** the review traced every caller — **no unbounded full-list enrich path exists**. `enrichKeys`' only live caller is page-scoped `enrichPage` (max 500 HEADs at the largest page setting); select-all/bulk use server-side listing. Worst case is the intended cap. Optional polish: batch the 500-row page to 8–16 concurrent. **S**, not a stampede fix.

### 3.2 — Virtualize / bound the object table **(M)**

**Problem (verified):** no `virtual` prop, up to 500 rows in the DOM (`ObjectTable.tsx:455,478`). antd ^6.3.0 supports `virtual`; no blockers (tooltips already disabled, `onRow` preserved). **M**

### 3.3 — Hidden-tab poller sweep **(S)** _(rev — concrete work, not just "verify")_

Two real ungated 30s network pollers run on hidden tabs: `BucketScanCard.tsx:172` and `AnalyticsSection.tsx:181`. Route both through `useVisiblePolling`. Everything else checked clean. **S**

### 3.4 — Delimiter-less LIST materializes the whole prefix **(M)** _(rev — new; dropped audit HIGH)_

From the HA audit's "other findings": a ListObjectsV2 without a delimiter materializes the entire prefix into memory before honoring max-keys. Unfixed, appears nowhere else. Server-side paging fix. **M**

### 3.5 — Narrow-screen / mobile confirmation **(S)**

`useIsNarrow(900)` hamburger drawer, `minWidth:0` flex guard, content-pane `overflow:auto` — all verified present. Manual pass on a real narrow viewport; low code risk. **S**

---

## Tier 4 — HA & architecture (correctness, not urgent)

DGP is single-instance production-ready and documented as sticky-session-only behind an LB. Still-open items from the HA audit; **#2 XFF-spoof and #5 SigV4-dedup are shipped** (verified) and excluded. _(rev)_ The review corrected two unsound fix descriptions, retired one row as already-shipped, and restored two dropped findings.

| # | Item | Problem | Fix _(rev-corrected)_ | Effort / Impact |
|---|---|---|---|---|
| #3 | Cross-instance delta-ref RMW unguarded | Concurrent same-prefix PUTs on two nodes can corrupt `reference.bin` (in-process `prefix_locks` only) | _(rev)_ NOT the job_store lease — that's node-local SQLite, the exact fix already built and reverted (B1). Needs a per-deltaspace **S3-CAS lock object** (mutex-shaped, not the 300s leader lease), and the per-PUT S3 round-trip cost must be gated (multi-instance mode only) | **L / high** (data corruption) |
| #4 | In-memory sessions break under round-robin | Cookie minted on A invalid on B (401s). _(rev)_ MPU half is DONE — Complete on the wrong node already fails loud with `NoSuchUpload` | Shared session store — _(rev)_ honest note: no in-stack backend fits (no Redis; SQLite per-node; DB-sync is 5-min-lag) → implies a new dependency | **L / high** _(rev: was M)_ |
| #10 | Non-atomic config hot-reload | 4 ArcSwaps published separately → torn-read window | Single `ConfigView` ArcSwap. _(rev)_ Audit graded this LOW: ordering is deliberately restrictive-first, torn reads fail CLOSED | **M / low** _(rev: was high)_ |
| #6 | MetadataCache no cross-instance invalidation | DELETE/PUT on A leaves B stale up to 10 min | _(rev)_ NOT "piggyback the event outbox" — the outbox is node-local and excluded from sync (B3), so it broadcasts nothing. Needs a new cross-instance channel (e.g. S3-polled invalidation log, or short-TTL when multi-instance) | **L / medium** _(rev: was M)_ |
| new | Lost bootstrap password = unrecoverable IAM DB | _(rev — restored dropped audit HIGH)_ No recovery story; `--set-bootstrap-password` wipes the encrypted DB | Document + a recovery path (re-key from a live session, or export-before-rekey affordance) | **M / high** |
| #7 | SSRF guard: S3 backend client only | _(rev — rescoped)_ Webhook + OIDC already use `SsrfGuardedResolver` (shipped); only the aws-sdk S3 client skips connect-time DNS re-check — narrow surface (operator-supplied endpoints) | Guarded HTTP connector for the S3 client | **S / low** _(rev: was medium)_ |
| #9 | Replication gravity well (now ~8800 LOC) | Parity/replication hard to evolve safely | _(rev)_ Note: the audit itself declined promote-to-`src/parity/` as a one-impl abstraction — revisit only if a second job subsystem materializes | **L / low** |

_(rev)_ **Retired:** #8 StorageBackend buffering defaults — the capability split SHIPPED (`get_passthrough_stream`/`_range` are now required trait methods, no buffering default); residual defaults are documented and gated at the transfer layer. #1 umbrella row — described an already-fixed sync mechanism; superseded by #3/#4.

---

## Suggested execution order

1. **Tier 0.0** — cut v1.12.2 (three CI-green commits are sitting unreleased).
2. **Tier 0.1 parts 1–4** — the flagship fix; part 3 (tween) is load-bearing, ship all four.
3. **Tier 1.0 then 1.1** — the query-string foundation MUST land before the job-drawer fix (or it silently no-ops), then the drawer.
4. **Tier 2 copy PR** — table rows + Enter-to-submit (with in-flight guard) + shared numeric sort + error-toast normalizer; includes same-PR test + docs updates for rows g/l/m.
5. **Tier 1.2–1.6** — batch the remaining URL gaps behind the 1.0 parser/helper.
6. **Tier 0.2 skeletons + Tier 3** — responsiveness sweep (3.3 pollers, 3.2 virtual table, 3.4 LIST paging).
7. **Tier 4** — as HA deployment demand materializes, starting with #3 (corruption) and the bootstrap-password recovery story.
