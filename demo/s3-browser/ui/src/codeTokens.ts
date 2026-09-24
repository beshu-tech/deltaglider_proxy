/**
 * Split operator-facing text into plain runs and technical tokens: CLI flags
 * (`--set-bootstrap-password`) and environment variables (`DGP_CACHE_MB`).
 * The UI font joins a double hyphen into what looks like one dash, so these
 * tokens must render in monospace `<code>`; `CodeTokenText` does that.
 */
export type TextSegment = { kind: 'text' | 'code'; text: string };

// Group 1 is the character before the token (or the start). A lookbehind
// would be shorter, but Safari < 16.4 cannot parse one (ESLint forbids it).
const TOKEN = /(^|[^\w-])(--[a-z][a-z0-9-]*|DGP_[A-Z0-9_]+)/g;

export function splitCodeTokens(text: string): TextSegment[] {
  const out: TextSegment[] = [];
  let last = 0;
  for (const m of text.matchAll(TOKEN)) {
    const at = (m.index ?? 0) + m[1].length;
    if (at > last) out.push({ kind: 'text', text: text.slice(last, at) });
    out.push({ kind: 'code', text: m[2] });
    last = at + m[2].length;
  }
  if (last < text.length) out.push({ kind: 'text', text: text.slice(last) });
  return out;
}
