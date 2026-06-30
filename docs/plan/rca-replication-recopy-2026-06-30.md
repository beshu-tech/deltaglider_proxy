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
