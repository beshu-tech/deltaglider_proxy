/** Most characters kept at the end of a long name: the version, the arch, the extension. */
const TAIL_CHARS = 20;
/** A name this short is never split: an end ellipsis on it hides little. */
const MIN_SPLIT_CHARS = 12;

/**
 * Splits `text` into the part that may be cut (`head`) and the part that
 * always stays visible (`tail`). The tail is one third of the name, at most
 * `tailChars`, so two names that differ only at the end stay distinct at any
 * width. A short name has no tail: it is never cut in the middle.
 */
export function splitForMiddleEllipsis(text: string, tailChars = TAIL_CHARS): { head: string; tail: string } {
  const chars = Array.from(text);
  if (chars.length < MIN_SPLIT_CHARS) return { head: text, tail: '' };
  const keep = Math.min(tailChars, Math.ceil(chars.length / 3));
  return { head: chars.slice(0, -keep).join(''), tail: chars.slice(-keep).join('') };
}
