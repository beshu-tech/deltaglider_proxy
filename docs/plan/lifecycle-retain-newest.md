# Lifecycle: count-based retention (`retain-newest: N`) — design

**Decision pending user review.** This documents the design for a count-based
lifecycle action — "keep the newest N objects in a prefix, delete the rest" —
the rule S3 lifecycle itself never shipped. Motivating use case: a weekly backup
cleanup that keeps the two most recent dumps under `db-archive/nightly/` and
deletes everything older, *by count, not by age*.

## Why this isn't free (the core tension)

Today's lifecycle is **per-object and streaming**:

- `planner::plan_object(rule, key, meta, expire_before, …)` decides each object's
  fate **in isolation**, from its own `meta.created_at` vs an age cutoff.
- `worker::run_or_preview` walks the prefix one page at a time via the canonical
  `Pager` (`src/job_loop.rs`): list a page → plan each object → act → persist a
  resume cursor → next page. A crash resumes from the cursor instead of
  re-listing from the top.

That design is *age-friendly* (an object older than `expire_after` is expired no
matter what else exists) and *count-hostile*: "keep newest 2" is a **set-relative**
decision — you cannot know whether to delete object X without having seen the whole
candidate set and ranked X within it. Two further facts sharpen the problem:

1. **Listing is lexicographic by KEY, not by date** (`engine::list_objects` →
   `paginate_sorted` sorts on key). Recency must come from `meta.created_at`.
   Key order equals date order only for date-named keys (`db-2026-06-14.dump`);
   we must not assume it in general.
2. **A naive streaming implementation is catastrophic.** If page 1 holds the two
   oldest objects and the worker "keeps the newest 2 seen so far," it will delete
   nothing on page 1, then as newer objects arrive it could delete the very ones
   it should keep — or, worse, if pages arrive oldest-first and it greedily keeps
   "the newest 2 of this page," it deletes across the whole set incorrectly. A
   wrong "keep newest N" doesn't leave junk — **it deletes data you meant to keep.**

So `retain-newest` needs a **rank-then-delete** model, not the per-object stream.

## Semantics (what the rule means)

```yaml
rules:
  - name: keep-last-two-nightly-dumps
    enabled: true
    bucket: db-archive
    prefix: "nightly/"
    action:
      type: retain-newest
      count: 2                    # keep the 2 newest QUALIFYING objects
      qualify:                    # only objects passing ALL of these are ranked
        min_size: "1MiB"          #   ignore truncated/empty junk
        min_age: "1h"             #   ignore objects still being written
    # delete-side guard (independent of qualify) — see below
    # protect_younger_than: "7d"
    include_globs: ["nightly/**/*.dump"]
    exclude_globs: [".deltaglider/**"]
    batch_size: 100
```

### Two distinct concepts — keep them separate

The whole subtlety (and the source of the "accidental empty README ruins
everything" failure) is that there are **two** different jobs a size/age threshold
could do, and conflating them is the bug:

1. **Eligibility (`qualify`)** — *should this object even count as one of the N?*
   An object that fails `qualify` is **invisible** to the rule: it is neither kept
   nor deleted, just ignored. This is where `min_size` and `min_age` belong.
2. **Delete guard (`protect_*`)** — *of the objects we decided to delete, is there
   a blanket reason to spare this one anyway?* A guard never promotes a non-newest
   object into the keep set; it only vetoes a delete.

`min_size` and `min_age` are **eligibility filters**, not delete guards — and that
distinction is exactly what protects you. Walk the user's scenario:

> `nightly/` holds `dump-A` (6 GB, newest), `README` (0 B, 2nd newest),
> `dump-B` (6 GB), `dump-C` (6 GB). `count: 2`.

- **If `min_size` were a delete-guard:** the 2 newest are `{dump-A, README}`, so
  the rule keeps a real backup **and a useless 0-byte file**, and deletes
  `dump-B` *and* `dump-C` — two real backups gone. The junk file poisoned the
  retention. ❌
- **As an eligibility filter (the design):** `README` fails `min_size` and never
  enters the ranking. The 2 newest *qualifying* objects are `{dump-A, dump-B}`;
  `dump-C` is deleted; `README` is left untouched (it's simply not this rule's
  business). You kept two real backups, exactly as intended. ✅

So: **a malformed/empty/half-written object can never displace a real backup from
the keep set, and can never itself be counted as "a backup we're keeping."** It
falls outside the rule entirely.

### Ranking and keep/delete

- **Qualify first.** Build the candidate set = objects passing include/exclude,
  internal/marker guards (as today) **and** `qualify.min_size` / `qualify.min_age`.
  Everything failing `qualify` is dropped from consideration and never deleted.
- **Rank** the qualifying candidates by `meta.created_at` descending; deterministic
  tie-break on **key descending** (stable across runs — never "random which one
  survives"). `min_size` uses `meta.file_size` (the *hydrated/original* size, not
  the delta-stored size — "0-byte file" means a 0-byte original).
- **Keep** the first `count`. **Delete** the rest of the qualifying set.
- **`count` must be ≥ 1** (validated at config time; `count: 0` is a typo, not a
  "empty the prefix" rule).
- **Safe under-count:** ≤ `count` qualifying objects → delete nothing.
- **Critical safety property:** because junk is filtered *before* ranking, the
  pathological case "every qualifying object is junk → keep set is junk → all real
  backups deleted" **cannot happen** — a real backup that out-ranks junk is always
  kept, and junk is never a deletion candidate in the first place.

### `qualify` knobs (v1)

| Field | Meaning | Guards against |
|---|---|---|
| `min_size` | object's original size must be ≥ this (humanbytes: `1MiB`, `500KB`) | empty / truncated / placeholder files anchoring the keep set |
| `min_age`  | object must be older than this (humantime: `1h`) | half-written / in-flight objects being counted before the upload finishes |

Both optional; omitted = no filter on that dimension. Both validated at
config-validate time.

## Optional delete-side guard: `protect_younger_than`

Separate from eligibility, the grandfather-father-son intuition — "even beyond the
newest N, never prune anything younger than a week" — is a **delete guard**:

- An object in the *delete* set is spared if it is younger than
  `protect_younger_than`. It does **not** enter the keep set; it's just not deleted
  *this run*. Next run, once it's older, normal ranking applies.
- Use when fresh backups arrive in bursts and you don't want a flurry of new
  writes to prune yesterday's good backup below the count before you've verified
  the new ones.

This stays a distinct knob from `qualify.min_age` precisely because they do
different jobs: `qualify.min_age` says *"too young to count yet"* (ignore);
`protect_younger_than` says *"old enough to count, but don't physically delete it
yet"* (spare). Most users will set only `qualify` and never touch this.

Two orthogonal layers (`qualify` eligibility + one optional delete guard) instead
of tiered keep-daily/keep-weekly grammar. Full GFS tiers remain a possible
follow-up, out of scope for v1.

## Implementation shape

### Where the set-relative decision lives

A new **two-phase** path in the worker for `retain-newest` rules, kept separate
from the streaming age path so neither complicates the other:

1. **Collect phase.** Page through the prefix with the existing `Pager`, but
   instead of acting per object, accumulate a lightweight candidate list:
   `Candidate { key, created_at, size }` for every object that passes the existing
   structural filters (globs, internal, directory-marker). No deletes yet. (Note:
   `qualify` is applied in the pure function below, NOT here — collect stays a
   dumb "what's in the prefix" pass so the qualify logic is fully unit-tested.)
2. **Rank + plan phase.** A new **pure** function — the entire dangerous decision
   in one testable place:
   ```
   planner::plan_retain_newest(
       candidates: &[Candidate],
       count: u32,
       qualify: &QualifySpec,            // { min_size, min_age }
       protect_younger_than: Option<Duration>,
       now: DateTime<Utc>,
   ) -> RetainPlan { keep, ignored, delete, protected }
   ```
   Steps, in order: (a) partition off `ignored` = candidates failing `qualify`
   (too small / too young) — these are never kept and never deleted; (b) sort the
   remaining *qualifying* set by `(created_at desc, key desc)`; (c) `keep` = first
   `count`; (d) of the rest, move any younger than `protect_younger_than` into
   `protected` (spared this run), the remainder into `delete`. **100%
   unit-testable** against the truth table — the logic that can delete data never
   needs a server to verify.
3. **Act phase.** Delete the `delete` set through `engine.delete` (same call,
   same event-outbox `LifecycleExpired` emission, same per-object failure
   recording as the age path). `ignored` and `protected` are surfaced in the
   preview/run response counters (so an operator can *see* "3 ignored: below
   min_size" — the empty-README case is visible, not silent).

### Memory + scale guard

Collecting the candidate set means holding `(key, created_at, size)` for the
whole prefix in memory. For backup prefixes (tens–hundreds of objects) this is
nothing. To stay safe on a pathologically large prefix we cap the collect phase
at a bounded candidate count (reuse the lifecycle scan cap pattern) and, if the
cap is hit, **fail the rule loudly rather than silently keep the wrong set** —
because a truncated candidate set could delete an object that's actually in the
newest N. A `log()` + failure row, never a silent partial.

### Crash-resume

The streaming age path resumes mid-prefix from a cursor. `retain-newest` cannot:
its decision needs the *complete* set, so a half-collected set is meaningless. So
a `retain-newest` run is **atomic per execution** — interrupted mid-collect, it
restarts the collect from scratch next run (the collect phase is read-only;
restarting is free and correct). Only the act phase mutates, and `engine.delete`
is already idempotent. This is a deliberate, documented divergence from the age
path's mid-prefix resume.

## Touch points

- **`config_sections.rs`** — extend the lifecycle `action` enum with
  `RetainNewest { count: u32, qualify: QualifySpec, protect_younger_than:
  Option<String> }` where `QualifySpec { min_size: Option<String>, min_age:
  Option<String> }`. Validate at config-validate time: `count >= 1`, `min_size`
  humanbytes, `min_age` / `protect_younger_than` humantime. (`action` is already a
  tagged enum — `delete` | `transition` — so this is a third variant, not a
  reshape.) `min_size` parses to bytes via the same humanbytes helper used for
  quota config.
- **`lifecycle/planner.rs`** — new pure `plan_retain_newest` + `Candidate` /
  `QualifySpec` / `RetainPlan { keep, ignored, delete, protected }` types;
  `rule_write_buckets` unchanged (still just the source bucket). Heavy unit tests
  (this is the function that can delete data — it carries the test weight).
- **`lifecycle/worker.rs`** — branch on action: existing streaming path for
  `delete`/`transition`, new collect→rank→act path for `retain-newest`. Preview
  reuses the same plan, returns `delete` as candidates and `ignored`/`protected`
  counts (read-only).
- **Admin API** — no new endpoints; `retain-newest` rides the unified Jobs
  surface (`lifecycle:<name>` preview/run-now/pause/resume). Preview response
  gains `objects_ignored` (+ per-candidate reason: `below_min_size` /
  `below_min_age`) and `objects_protected` so "the empty README is ignored, not
  counted" is **visible** before anyone runs it.
- **Docs** — `reference/lifecycle.md` grammar + a how-to ("keep the last N
  backups") that leads with the qualify/junk-protection story. The how-to also
  shows the **zero-code script alternative** for anyone not on a version with this
  action.
- **Admin UI** — see the dedicated GUI section below. (Extends the existing
  lifecycle rule editor; can land after the engine + API if we ship the capability
  first, but it's part of THIS plan, not a follow-up.)

## GUI (extends the existing editor — we already have one)

We are NOT building a new screen. Lifecycle rules already have a full editor on
the unified **Jobs** screen, and `retain-newest` slots into it as a third action
type. The relevant existing surface:

- **`components/jobs/JobsPanel.tsx`** — the one Jobs screen: unified table over
  `GET /jobs` (replication + lifecycle + maintenance rows), with a drawer
  (Definition / Runs / Failures) and the two storage-section editors behind one
  dirty bar + sequential apply queue (per CLAUDE.md).
- **`components/LifecycleRuleFields.tsx`** — the rule form. It ALREADY has an
  **Action `<Select>`** with `delete` | `transition`, conditionally rendering the
  transition sub-fields (destination bucket/prefix, delete-source toggle). This is
  the exact pattern to extend.
- **`components/lifecyclePayload.ts`** — `actionKind(rule.action)` +
  payload-shaping (the YAML↔form boundary). New action variant handled here.
- **`components/LifecycleSummary.tsx`** — the human-readable one-line rule summary
  shown in the table/drawer.

### What changes

1. **Action select gains `Retain newest N` (`retain-newest`).** Selecting it
   swaps the `Expire after` field for the retain sub-form (the `delete`/`transition`
   conditional rendering pattern already in the file):
   - **Keep newest** — `InputNumber` (min 1) → `count`.
   - **Ignore objects smaller than** — size input → `qualify.min_size`
     (humanbytes; empty = no filter). Help text spells out the protection:
     *"Junk/empty files below this size are ignored — never kept, never deleted —
     so an accidental empty file can't push a real backup out of the keep set."*
   - **Ignore objects younger than** — duration input → `qualify.min_age`
     (humantime; empty = no filter). Help: *"Objects still being uploaded don't
     count yet."*
   - **Don't delete anything younger than** *(advanced, collapsed)* —
     `protect_younger_than`. Most users leave this empty.
2. **`lifecyclePayload.ts`** — `actionKind` returns `'retain-newest'`; add
   shaping for the new variant (form state ⇄ `{ type, count, qualify, … }`), with
   a Node regression test in the existing `*-regression-test.mjs` style (the
   admin-editor bug class — comma round-trip / stale closures — is real here; the
   pure payload shaper + Node test is the established guard).
3. **`LifecycleSummary.tsx`** — summary line e.g.
   *"Keep newest 2 (ignore < 1 MiB) in db-archive/nightly/"*.
4. **Preview drawer** — surface the new response counters: **Kept N · Deleting M ·
   Ignored K (below min size/age) · Protected J**. The `Ignored` count with its
   reason is what makes "the empty README is safely ignored" visible BEFORE
   apply — the GUI payoff of the qualify/guard split.

### Reusable beyond lifecycle

`qualify` (min-size / min-age eligibility) is a generic "which objects does this
rule even consider" concept. If we later add count-based knobs elsewhere
(replication filters, the Object-Lock default-retention exemptions), the same
`QualifyFields` sub-component + `QualifySpec` payload shape should be factored out
and reused rather than re-implemented — same discipline as the shared
`MaskedSecretInput` / `RowListEditor` convergence primitives (PR #24).

## Test plan (the deletes-data risk demands it)

Pure-function unit tests for `plan_retain_newest` — the keep/ignore/delete truth
table:

- N objects, count K<N, no qualify → exactly N−K deleted, the K newest kept.
- count ≥ N → nothing deleted (no-op).
- Equal `created_at` → deterministic key-desc tie-break (kept set is stable).
- count = 1 → keeps exactly the single newest. Empty candidate set → no-op.
- **The headline junk-protection case:** `{dump-A 6GB newest, README 0B 2nd,
  dump-B 6GB, dump-C 6GB}`, count 2, `min_size: 1MiB` → README is `ignored`
  (never kept, never deleted); keep `{dump-A, dump-B}`; delete `{dump-C}`. Assert
  README is untouched AND that no real backup was deleted to make room for it.
- `min_size` with EVERY object below it → all `ignored`, zero deleted (a prefix of
  junk is a safe no-op, never "keep junk, delete reals" — impossible by construction).
- `min_age` qualify → an object still younger than min_age is `ignored` (not yet
  eligible to count), not deleted.
- `protect_younger_than` → an object outside the newest-K and old enough to count,
  but younger than the guard, lands in `protected` (spared this run), not `delete`.
- qualify vs guard are independent: an object can be eligible (passes qualify) yet
  protected (guard spares it) — assert it's neither kept nor deleted.

Integration tests on a `TestServer`:

- Seed 5 dated dumps + 1 stray 0-byte file, run-now `retain-newest count:2
  min_size:1MiB` → exactly the 3 oldest dumps gone, 2 newest dumps present
  byte-identical, the 0-byte file untouched.
- Preview is read-only (no deletes, no history rows) and reports `objects_ignored`.
- A fresh upload between runs shifts the keep set correctly next run.
- Candidate-cap breach → rule fails with a failure row, deletes nothing.
- `LifecycleExpired` events emitted for each delete (parity with age path).

## What we are NOT doing (scope discipline)

- Not implementing full GFS tier grammar (`keep-daily`/`keep-weekly`/…). `count`
  + `qualify` + one delete guard covers the asked use case; tiers can follow if
  demanded.
- Not changing the age (`delete`) or `transition` paths — they keep their
  streaming/resume behavior untouched.
- Not introducing versioning or "keep newest N *versions* of a key" — that's the
  S3 `NewerNoncurrentVersions` feature, which needs versioning we deliberately
  don't implement (see `explanation/versioning-vs-s3-versioning.md`). This is
  newest-N *distinct objects in a prefix*, which is the backup-cleanup shape.
```
