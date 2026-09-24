// Single source of truth for delta-compression savings math on the
// client. Mirrors the Rust module at `src/deltaglider/savings.rs`.
//
// Why this file exists: pre-centralisation the SPA had THREE inline
// "savings" formulas (DeltaSavingsChip, useUploadQueue, InspectorPanel),
// each rolling their own cap (99 in one, 99.9 in two others) and their
// own zero-bytes edge handling. The result was that the same "100%
// saved" lie or "−12% saved" looked different in each surface. Every
// number a user reads must come from one of the two helpers below.
//
// Server-driven numbers (per-prefix / per-bucket) should arrive
// pre-computed in `PrefixSavingsResponse` from `/_/api/admin/...`. The
// helpers here handle the per-object / per-batch cases where the
// browser computes locally because it already has both sides
// (originalSize + storedSize). They MUST NOT be used to roll up a
// prefix client-side — see `getPrefixSavings` for that.

/**
 * Per-scope savings summary (suitable for "we just saved you ~X%").
 * Capped at 99 so the UI never reads "100% saved" while bytes are on
 * disk; clamped to 0 on the negative side (a single delta against a
 * larger reference can produce a negative diff, but end-user surfaces
 * should show "no savings yet", not "−12%"). The diagnostic admin
 * surfaces have their own server-computed signed `true_savings_bytes`
 * / `compression_ratio` fields when they need to show the negative.
 */
interface ScopeSavingsView {
  /**
   * Percent savings 0..=99 (integer; floor of the raw value). The
   * canonical compact display number for the breadcrumb chip and any
   * UI that has very little real estate. Surfaces with more pixels
   * (dashboard hero, analytics) should render `pctOneDecimal` so the
   * extra precision matches the larger numerals.
   */
  pct: number;
  /**
   * Percent savings 0..=99.9 with one decimal place (e.g. 89.6). Same
   * underlying value as `pct`, rendered with one extra digit for
   * surfaces with room to display it. Capping rule is identical
   * (99.9 is the max — never 100.0).
   */
  pctOneDecimal: number;
  /** Saturating bytes saved — never negative. */
  savedBytes: number;
  /** True when there's no measurable scope (no original bytes). */
  empty: boolean;
}

/**
 * Compute the per-scope savings view from raw byte totals. Same shape
 * as the Rust `SavingsTotals::savings_percentage` + `saved_bytes`.
 * Stays deterministic on edge cases:
 *
 *   - `originalBytes <= 0`           → `empty: true`, pct=0, savedBytes=0
 *   - `storedBytes >= originalBytes` → `pct: 0` (no savings to brag about)
 *   - raw pct >= 99 with stored > 0  → clamped to 99 (and 99.9 for one-decimal)
 *   - otherwise                      → `Math.floor(rawPct)` / `floor*10/10`
 *
 * The floor-instead-of-toFixed choice prevents 99.95% → "100%" rounding
 * (the bug that surfaced the whole DRY problem).
 */
export function summarizeScopeSavings(
  originalBytes: number,
  storedBytes: number,
): ScopeSavingsView {
  if (originalBytes <= 0) {
    return { pct: 0, pctOneDecimal: 0, savedBytes: 0, empty: true };
  }
  const saved = Math.max(0, originalBytes - storedBytes);
  const rawPct = (saved / originalBytes) * 100;
  // Integer cap — what the chip uses.
  const pct =
    storedBytes > 0 && rawPct > 99 ? 99 : Math.max(0, Math.floor(rawPct));
  // One-decimal cap — same data, more pixels of resolution. We FLOOR
  // to one decimal to keep the "never 100%" contract: a raw 99.95
  // displays as 99.9, not 100.0. `Math.floor(x * 10) / 10` is the
  // deterministic one-decimal floor.
  const oneDecimalFloor = Math.floor(Math.max(0, rawPct) * 10) / 10;
  const pctOneDecimal =
    storedBytes > 0 && oneDecimalFloor > 99.9 ? 99.9 : oneDecimalFloor;
  return { pct, pctOneDecimal, savedBytes: saved, empty: false };
}

/**
 * The per-OBJECT savings view, intended for the single-row InspectorPanel
 * + the post-upload batch summary in UploadPage. Differs from
 * `summarizeScopeSavings` in two ways that matter to the user:
 *
 *   - `pct` is a fractional number floored to one decimal place and
 *     capped at 99.9 — for a single object users expect to see "99.9%
 *     saved" on tiny deltas; the chip-style integer cap is for
 *     aggregations across many objects.
 *   - `storedSize == null` (HEAD not yet resolved) returns `empty:true`
 *     so the UI can show a spinner.
 */
interface ObjectSavingsView {
  pct: number;
  savedBytes: number;
  empty: boolean;
}

export function summarizeObjectSavings(
  originalSize: number,
  storedSize: number | null | undefined,
): ObjectSavingsView {
  if (storedSize == null || originalSize <= 0) {
    return { pct: 0, savedBytes: 0, empty: true };
  }
  const saved = Math.max(0, originalSize - storedSize);
  const rawPct = (saved / originalSize) * 100;
  // Floor to one decimal (same rule as `summarizeScopeSavings`) so a raw
  // 99.95..99.99 never renders "100.0" through `toFixed(1)`, then cap at
  // 99.9 unless the stored size is *literally* 0 — only then is 100%
  // honest. For everything else the reference and/or non-zero delta
  // still consumes bytes.
  const floored = Math.floor(Math.max(0, rawPct) * 10) / 10;
  const pct = storedSize !== 0 && floored > 99.9 ? 99.9 : floored;
  return { pct, savedBytes: saved, empty: false };
}

/** Bytes in one GiB. The UI labels it "GB", like `formatBytes`. */
export const GIB = 1024 ** 3;

/** Bytes → GiB for an editable quota field: 3 decimals, so 500 MiB reads
 *  0.488 instead of rounding to 0. Display only — `bytesFromGib` is the
 *  write path, and an untouched field never writes. */
export function gibFromBytes(bytes: number): number {
  return Number((bytes / GIB).toFixed(3));
}

/** GiB (possibly fractional) → whole bytes; `quota_bytes` is a u64. */
export function bytesFromGib(gib: number): number {
  return Math.round(gib * GIB);
}
