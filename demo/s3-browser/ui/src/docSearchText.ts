/**
 * Plain text of a docs page for the docs search (index + result snippet).
 * Pure so src/__tests__/docSearchText.test.ts can check it.
 *
 * Inline code stays: identifiers such as `replication_target_only` are
 * what operators search for. HTML comments (the GENERATED banner of the
 * changelog), the h1 (the result already shows the title), images and
 * fenced code blocks go.
 */
export function docSearchText(md: string): string {
  return md
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/```[\s\S]*?```/g, '')
    .replace(/^# .*$/m, '')
    .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
    .replace(/\[([^\]]+)\]\([^)]+\)/g, '$1')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/^\s{0,3}(#{1,6}|>)\s*/gm, '')
    .replace(/[*~|]/g, '')
    // Emphasis underscores sit at a word edge; ones inside a word
    // (max_delta_ratio) are part of the identifier.
    .replace(/(^|[\s(])_+(?=\S)/g, '$1')
    .replace(/(\S)_+(?=[\s).,;:!?]|$)/g, '$1')
    .replace(/[ \t]+/g, ' ')
    .replace(/\s*\n\s*/g, '\n')
    .trim();
}

/** About `maxLen` characters of `text` around the first query word it contains. */
export function docSearchSnippet(text: string, query: string, maxLen = 120): string {
  const lower = text.toLowerCase();
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const idx = words.map((w) => lower.indexOf(w)).find((i) => i >= 0) ?? -1;
  const flat = (s: string) => s.replace(/\n/g, ' ');
  if (idx === -1) return flat(text.substring(0, maxLen)) + (text.length > maxLen ? '...' : '');
  const start = Math.max(0, idx - 40);
  const end = Math.min(text.length, idx + maxLen - 40);
  return `${start > 0 ? '...' : ''}${flat(text.substring(start, end))}${end < text.length ? '...' : ''}`;
}
