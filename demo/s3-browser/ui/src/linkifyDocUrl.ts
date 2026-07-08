// Pure helpers for rendering doc links inside plain validation strings.
// Server-side enforcement messages (config refusals, 403s, check() warnings)
// embed a https://deltaglider.com/docs/... URL; the GUI renders it clickable
// and rewrites it to the in-app docs viewer so operators stay in the product.
// Pure module — node-regression-tested (linkify-regression-test.mjs).

export interface LinkSegment {
  kind: 'text' | 'link';
  text: string;
  /** Present on links: in-app docs path for deltaglider.com/docs URLs, else the URL itself. */
  href?: string;
}

/** The one capability-boundary doc anchor (mirrors Rust CAPABILITY_DOC_URL). */
export const CAPABILITY_DOC_URL =
  'https://deltaglider.com/docs/how-to/backend-capability-validation';

const DOCS_URL_RE = /^https:\/\/(?:www\.)?deltaglider\.com\/docs\/([A-Za-z0-9/_.-]+)$/;

/** deltaglider.com/docs/<path> → in-app docs route (`/_/docs/<flattened-id>`),
 *  mirroring the `pathToId` flattening in docs-imports.ts (slashes → dashes).
 *  @public — exercised by scripts/linkify-regression-test.mjs (dynamic import). */
export function docsUrlToInAppHref(url: string): string | null {
  const m = url.match(DOCS_URL_RE);
  if (!m) return null;
  return `/_/docs/${m[1].replace(/\//g, '-')}`;
}

/** Split a plain message into text/link segments. Trailing prose punctuation
 *  after a URL stays text so "see https://…/x." doesn't 404. */
export function splitLinkSegments(text: string): LinkSegment[] {
  const out: LinkSegment[] = [];
  const re = /https:\/\/[^\s)"'<>]+/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    const trimmed = m[0].replace(/[.,;:!?]+$/, '');
    if (trimmed.length === 0) continue;
    if (m.index > last) out.push({ kind: 'text', text: text.slice(last, m.index) });
    out.push({ kind: 'link', text: trimmed, href: docsUrlToInAppHref(trimmed) ?? trimmed });
    last = m.index + trimmed.length;
  }
  if (last < text.length) out.push({ kind: 'text', text: text.slice(last) });
  return out;
}
