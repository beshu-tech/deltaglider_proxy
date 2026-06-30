> **UPDATE 2026-06-30 (post multi-agent review + integration repro): the root-cause
> attribution below is WRONG / incomplete.** A discriminating integration test
> (`test_replication_foreign_object_missing_created_at_converges_on_second_run`)
> shows that foreign objects (partial DG metadata, no `dg-created-at`) — both
> delta-eligible `.zip` and non-eligible `.sha1` — **already converge on the 2nd
> run, BEFORE the `created_at→LastModified` fix**, for a same-backend src/dst.
> Reasons established by the review: (a) non-eligible sidecars take the lite LIST
> path which already used the stable S3 `LastModified`; (b) the dest's HEAD returns
> a stable copy-time `created_at`, so `src.LastModified < dest.copytime → Skip`.
> The landed fix is a real **latent-bug + correctness hardening** (it removes a
> per-read `now()` synthesis on the partial-DG-metadata HEAD path, fixes timestamp
> parsing for offset/lowercase-`z` values, and incidentally repairs two lifecycle
> behaviours for foreign objects) — but it is **NOT** demonstrated to be the cause
> of the prod 55K re-copy. The prod scenario differs in ways not yet reproduced:
> **cross-backend (Hetzner S3 → LOCAL1BE filesystem) + distinct buckets**. Leading
> open hypotheses for the real cause: the dest HEAD returns `None` (object never
> landed / key-rewrite mismatch → `Decision::Copy` every run), or a cross-backend
> created_at skew. **Root cause remains OPEN; needs the actual run telemetry from
> the encrypted config DB (admin session) to confirm.** Everything below is the
> original (superseded) analysis, kept for the trail.

---

# RCA — replication re-copies 55K+ objects nightly while Verify shows only 175 diffs

**Date:** 2026-06-30 · **Severity:** High (wasted egress/compute every tick; masks real drift)
**Prod:** dgp.serve.beshu.tech (v1.8.2) · rule `copyHzToB2`, source `beshu/ror/` (Hetzner) → LOCAL1BE

## Symptom

- Operator ran **Verify** → **175 differences** (content-level: logical SHA-256 + size).
- Overnight the replication run **copied 55K+ objects**.
- The two numbers describe the same buckets but disagree by ~300×.

## Root cause

**Replication and Verify use two *different* definitions of "in sync", and the
replication one is broken for foreign objects.**

| | Compares on | Verdict for a byte-identical foreign object |
|---|---|---|
| **Verify** (`parity::compare_pair`) | logical **SHA-256 + size** (content) | `matched` ✅ |
| **Replication** (`planner::should_replicate`, NewerWins) | **`created_at` timestamp only** | **copy** ❌ |

`should_replicate` under the default `NewerWins` policy decides purely by
timestamp (`src/replication/planner.rs:231`):

```rust
(Some(dest), ConflictPolicy::NewerWins) =>
    if src_meta.created_at > dest.created_at { Copy } else { Skip }
```

`created_at` is read from the object's `dg-created-at` user-metadata header, and
**falls back to `Utc::now()` when that header is absent** (`src/storage/s3.rs:474`):

```rust
let created_at_str =
    get_value(&[mk::CREATED_AT, "created-at"]).unwrap_or_else(|| Utc::now().to_rfc3339());
```

**Foreign objects** — written directly to the backend, not through the proxy —
have no `dg-created-at`. So every time the planner HEADs such a source object,
its `created_at` is recomputed as **"now"**, which is always greater than the
destination's stored timestamp → `Copy`, **on every single tick, forever**. The
copy is wasted: the bytes are already identical (Verify proves it), but the
timestamp comparison never converges because "now" advances each scan.

## Evidence (from the live Hetzner backend)

`beshu/ror/` holds **94,102 objects**. By type:

| Ext | Count | `dg-created-at`? | Behaviour |
|---|---|---|---|
| `.delta` | 40,461 | present (proxy-written) | compares correctly |
| **`.sha1`** | **39,418** | **ABSENT (foreign)** | **re-copied every tick** |
| **`.sha512`** | **13,086** | **ABSENT (foreign)** | **re-copied every tick** |
| `.mp4` / `.bin` / `.zip` / `.pom` | ~1,137 | present (proxy-written) | compares correctly |

**~52,504 foreign checksum sidecars** (`.sha1` + `.sha512`) lack `dg-created-at`.
Confirmed by HEAD: a `.sha1` sample returns `dg-created-at: <ABSENT>` and zero
`dg-*` keys; the `.mp4`/`.bin`/`.zip` artifacts all carry it. `beshu/incus/`
snapshots are likewise foreign. **52.5K ≈ the "55K+ copied".**

The destination is the LOCAL1BE filesystem backend, whose foreign-object
`created_at` also falls back (file mtime / `Utc::now()`,
`src/storage/filesystem.rs:246`), so the source's "now" always wins.

## Why Verify and replication disagree by 300×

They are answering different questions. Verify answers "is the **content**
mirrored?" → only 175 objects truly differ. Replication answers "is the source
**newer**?" → 52.5K foreign objects always look newer because their timestamp is
re-synthesised as "now" on every read. Both are internally consistent; the bug is
that the replication signal is meaningless for objects with no stored timestamp.

## Fixes (in priority order)

1. **Stop re-synthesising `created_at` as `now()` for foreign objects.** A missing
   `dg-created-at` should map to a STABLE timestamp — the S3 `LastModified` of the
   object (already available on the HEAD/list response) — not wall-clock now. This
   is the surgical fix and makes NewerWins converge for foreign objects.
   *(`src/storage/s3.rs:474` + the filesystem equivalent.)*
2. **Make NewerWins fall back to a content check on timestamp ties / missing
   timestamps.** When neither side has a trustworthy `created_at`, compare size
   (and sha when present) instead of blindly copying — align the planner's notion
   of "in sync" with Verify's. Cheapest: when `created_at` is unavailable, skip if
   `size` (and etag/sha when both present) already match.
3. **Operational stopgap (no code):** set the rule's conflict policy to
   `SkipIfDestExists` if the destination is meant to be write-once — foreign
   sidecars that already exist won't be re-copied. (Not appropriate if updates
   must propagate; fix #1 is the real answer.)

## Notes / non-actions

- This is **not** data corruption — every re-copy writes byte-identical content;
  the cost is wasted egress + compute + a noisy "55K copied" that hides the real
  175-object drift.
- The same `Utc::now()` fallback is still present on `main` — fix #1 should land
  there, not just as a hotfix.
- The synced config DB (`dgp-conf/.deltaglider/config.db`) holds the exact run
  history / failures / parity outcome, but it's SQLCipher-encrypted with the
  bootstrap **password** (only the bcrypt *hash* is in `secrets.env`), so the
  numbers above come from the code + the raw backend object metadata, not the DB.

---

# CORRECTED RCA (2026-06-30, from live prod telemetry via admin session)

The `created_at` theory is **fully refuted by the run telemetry**. The real cause:

## Topology (confirmed)
`copyHzToB2`: source `beshu` (Hetzner backend) → dest `beshu-b2` (virtual bucket
routed to the **`b2` = Backblaze backend**, `s3.eu-central-003.backblazeb2.com`).
- Source `beshu`: 93,721 objects. Dest `beshu-b2`: only **21,298** landed (~72K short).
- Rule policy: **`conflict: source-wins`** (NOT newer-wins).

## Root cause — two compounding factors, neither is created_at

1. **`conflict: source-wins` copies EVERY object EVERY run, by design.**
   `planner::should_replicate`: `(Some(_), SourceWins) => Decision::Copy`. There is
   no skip/convergence under source-wins — content/timestamp are never consulted.
   So a healthy run copies all ~93K every 15-min tick. The "55K+ copied overnight"
   is exactly this policy working as specified, not a bug. (`newer-wins` would
   skip-once-present; `source-wins` re-copies forever.)

2. **The Backblaze B2 destination is failing**, so runs error out and the dest
   never reaches parity (stuck at 21K/93K). Retained failures (run 33, ongoing):
   - `Bucket not found: beshu-b2 (after 2 attempts)`
   - `put_object failed (status=500): service error` (Backblaze 500s, ~12 of 20)
   - `destination verify head failed` (B2 HEAD failing)
   A run that hits a fatal/`Bucket not found` is marked `failed`; objects whose B2
   put 500'd never land → still missing next tick → re-attempted. The mix of
   `failed` / `completed_with_errors` rows with errors 1→75 is B2 flakiness.

## Why Verify said 175 but the run "copied 55K"
Verify compares **content** (sha+size) over the portion that DID land → 175 real
diffs. Replication under `source-wins` **re-ships everything regardless of
content**, and keeps retrying the ~72K not-yet-on-B2 objects. The two numbers were
never measuring the same thing — and the gap is policy + a broken dest, not drift.

## Recommended actions (no code change required)
1. **Switch `copyHzToB2` to `conflict: newer-wins`** (or `skip-if-dest-exists`) so
   it stops re-copying objects already on the dest — immediate ~75% load cut.
2. **Fix the Backblaze B2 dest**: confirm the real B2 bucket exists + creds valid +
   investigate the 500s (B2 rate-limit / region / app-key scope). The
   `Bucket not found` suggests an intermittent routing/credential/region issue.
3. Re-run Verify after the dest is healthy + policy fixed — expect diffs → ~0.

The committed `created_at`/`resolve_created_at` change is still valid hardening
(+ the 3 parsing fixes + 2 lifecycle repairs) but is UNRELATED to this incident.

---

# DESIGN FIX (2026-06-30): `source-wins` removed

Per the finding that an UNCONDITIONAL conflict policy makes no sense on a recurring
rule (every replication rule is recurring — there is no one-shot rule concept), the
`source-wins` policy was **removed** and replaced by **`content-diff`** (commit on
branch `feat/replication-content-diff-drop-source-wins`). content-diff copies only
when bytes differ and skips identical objects, so it converges while preserving the
"keep dest = source" intent. A config carrying `source-wins` now fails to load with
a clear `unknown variant` error.

**Prod follow-up still required (operator):** before deploying the new binary,
switch `copyHzToB2` from `source-wins` to `content-diff` (else the config won't
load), AND fix the Backblaze B2 destination (the 500s / bucket-not-found are the
reason the dest is stuck at 21K/93K — independent of the policy change).
