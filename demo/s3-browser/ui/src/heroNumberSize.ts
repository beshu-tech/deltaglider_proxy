/**
 * heroNumberSize — deterministic font sizing for the HeroSavingsPanel numeral.
 *
 * The hero figure ("2,054%") renders in Manrope weight-800 with
 * `font-variant-numeric: tabular-nums` and letter-spacing -0.045em. With
 * tabular figures every digit has the same advance width, so the rendered
 * width of the FINAL string is a pure linear function of the font size —
 * no DOM text measurement needed, and the count-up animation can never
 * cause a mid-animation font jump (we size for the final value up front).
 *
 * Per-character width budget (em, measured against Manrope 800
 * tabular-nums at -0.045em tracking, with a little headroom):
 *
 *   digit                  ≈ 0.58em
 *   thousands sep / dot    ≈ 0.30em  (',' and '.')
 *   '%' suffix             rendered at 0.42em of the numeral size plus a
 *                          small margin — budgeted as a flat 0.42em share.
 *
 * `fontPx = containerWidth / totalEmWidth`, clamped to [40, 138] px.
 * 138 matches the old `clamp(72px, 9.5vw, 138px)` ceiling so short values
 * still render huge; 40 is a legibility floor (an extreme value in a very
 * narrow column may then ellipse past the column, which is acceptable).
 */

const DIGIT_EM = 0.58;
const SEPARATOR_EM = 0.3;
const SUFFIX_EM = 0.42;
const MIN_PX = 40;
const MAX_PX = 138;
const FALLBACK_WIDTH_PX = 240; // grid column min — used before first measure

/**
 * Compute the hero numeral font size in px.
 *
 * @param charCount        Length of the final numeral string WITHOUT the
 *                         '%' suffix (its share is budgeted internally).
 *                         Both displayed forms are covered: the grouped
 *                         integer `Math.round(ratio*100).toLocaleString()`
 *                         ("1" … "100,000") and the one-decimal percent
 *                         lead `pct.toFixed(1)` ("95.1"). Separator count
 *                         is derived as floor(charCount / 4), which is
 *                         exact for both forms (a grouped integer of
 *                         length 5–7 has one comma, length 9–11 two, …;
 *                         "95.1" / "100.0" have one dot).
 * @param containerWidthPx Measured width of the column the numeral must
 *                         fit in. Non-finite / non-positive values fall
 *                         back to the grid column minimum (240px).
 * @returns Font size in px, clamped to [40, 138]. Below the ceiling the
 *          estimated rendered width never exceeds the container width.
 */
export function heroFontPx(charCount: number, containerWidthPx: number): number {
  const chars = Math.max(1, Math.floor(charCount));
  const separators = Math.floor(chars / 4);
  const digits = chars - separators;
  const emWidth = digits * DIGIT_EM + separators * SEPARATOR_EM + SUFFIX_EM;
  const width =
    Number.isFinite(containerWidthPx) && containerWidthPx > 0
      ? containerWidthPx
      : FALLBACK_WIDTH_PX;
  // Floor to 0.01px so emWidth * result can never exceed the budget.
  const px = Math.floor((width / emWidth) * 100) / 100;
  return Math.min(MAX_PX, Math.max(MIN_PX, px));
}
