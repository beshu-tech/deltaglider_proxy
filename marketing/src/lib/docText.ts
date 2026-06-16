// docText.ts — pure text helpers over raw markdown. Kept separate from
// renderDoc.ts so docs.ts can derive titles without importing the unified
// pipeline (and without a circular import).

/** First `# heading` of a doc — its title (matches the product's extractTitle). */
export function extractTitle(markdown: string): string {
  for (const line of markdown.split('\n')) {
    const m = line.match(/^#\s+(.+)/);
    if (m) return m[1].trim();
  }
  return 'Untitled';
}

/** First paragraph (plain text, truncated) — used as the meta description. */
export function extractSummary(markdown: string, max = 155): string {
  const lines = markdown.split('\n');
  let started = false;
  const para: string[] = [];
  for (const raw of lines) {
    const line = raw.trim();
    if (!started) {
      if (line.startsWith('#') || line === '') continue;
      started = true;
    }
    if (started) {
      if (line === '') break;
      para.push(line);
    }
  }
  const text = para
    .join(' ')
    // Collapse markdown links/images to their visible text FIRST, so the URL
    // inside (...) is dropped — not left fused to the link text. ![alt](src)
    // and [text](href) → alt / text.
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
    // Then strip residual inline-markdown punctuation.
    .replace(/[*_`#>]/g, '')
    .replace(/\s+/g, ' ')
    .trim();
  return text.length > max ? text.slice(0, max - 1).trimEnd() + '…' : text;
}
