/** Characters kept at the end of a long name: the version, the arch, the extension. */
const TAIL_CHARS = 20;

/**
 * Splits `text` into the part that may be cut and the tail that stays.
 * A short name has no tail: it is never cut.
 */
export function splitForMiddleEllipsis(text: string, tailChars = TAIL_CHARS): { head: string; tail: string } {
  const chars = Array.from(text);
  if (chars.length <= tailChars * 2) return { head: text, tail: '' };
  return { head: chars.slice(0, -tailChars).join(''), tail: chars.slice(-tailChars).join('') };
}
