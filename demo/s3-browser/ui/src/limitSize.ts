/**
 * The largest-file limit as the operator types it: a number of GiB.
 * Pure, so the rule is unit-tested without the form.
 */

/** Up to 5 TiB, the largest S3 object. */
const LIMIT_MAX_GIB = 5 * 1024;

/** The sentence for a typed limit, or null. Empty is "no limit typed yet". */
export function limitSizeError(text: string): string | null {
  const t = text.trim();
  if (t === '') return null;
  const n = Number(t);
  if (!/^\d+(\.\d{1,2})?$/.test(t) || n < 0.01 || n > LIMIT_MAX_GIB) {
    return `Enter a size between 0.01 and ${LIMIT_MAX_GIB.toLocaleString('en-US')} GiB.`;
  }
  return null;
}
