/**
 * Product docs bundle — PURE helpers (no React, no fetch) over the payload of
 * `GET /_/api/docs`, which the proxy serves from the markdown embedded in
 * the binary, behind a live session.
 *
 * The docs used to be inlined into the JS bundle by a Vite glob. That put
 * ~800 KB of markdown — including the changelog, which names the exact
 * running version — into a static asset any anonymous caller could fetch.
 * Serving them at runtime keeps the bundle free of build-identifying data;
 * scripts/check-bundle-fingerprints.sh guards the regression.
 *
 * Grouping + ordering still come from docs/product/manifest.json — the single
 * source of truth shared with the marketing-website docs renderer — but the
 * proxy ships the manifest inside the same payload, so the UI has no
 * build-time knowledge of the docs at all.
 *
 * Unit-tested by src/__tests__/docsBundle.test.ts.
 */

export type DocGroup = string;

export interface DocEntry {
  id: string;
  title: string;
  /** Path relative to docs/product/ plus `.md`. Used by findDocByFilename to resolve links. */
  filename: string;
  content: string;
  group: DocGroup;
  /**
   * Sort position within the group. Lower = earlier. Landing + sidebar
   * render in ascending order; titles are *not* the sort key (they
   * change with editorial tweaks; order stays stable).
   */
  order: number;
}

/** Wire shape of `GET /_/api/docs`. */
export interface DocsPayload {
  manifest: {
    groups: { id: string; tagline: string }[];
    docs: { path: string; group: string; order: number }[];
  };
  docs: { path: string; content: string }[];
}

/** Everything the docs UI needs, derived once from the payload. */
export interface DocsBundle {
  docs: DocEntry[];
  groups: readonly DocGroup[];
  /** One-line summary of what a group is for — rendered on the landing. */
  taglines: Record<DocGroup, string>;
}

/** Extract the first `# heading` from markdown content */
function extractTitle(content: string): string {
  for (const line of content.split('\n')) {
    const m = line.match(/^#\s+(.+)/);
    if (m) return m[1].trim();
  }
  return 'Untitled';
}

/**
 * Convert a doc path ("auth/30-oauth-setup") into a URL-safe id
 * ("auth-30-oauth-setup"). Subfolder segments collapse to flat ids
 * because the doc URL space (`/_/docs/:id`) is intentionally flat —
 * it's a product surface, not a filesystem browser.
 */
export function pathToId(path: string): string {
  return path.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
}

/**
 * Join the manifest (group + order per path) with the markdown contents.
 * A manifest path with no content is skipped rather than thrown: the server
 * guards manifest ↔ disk parity (scripts/check-docs-registry.sh), and a
 * viewer that can render 58 docs instead of 59 beats a blank page.
 */
export function buildDocsBundle(payload: DocsPayload): DocsBundle {
  const contentByPath = new Map(payload.docs.map((d) => [d.path, d.content]));
  const docs: DocEntry[] = [];
  for (const d of payload.manifest.docs) {
    const content = contentByPath.get(d.path);
    if (content === undefined) continue;
    docs.push({
      id: pathToId(d.path),
      title: extractTitle(content),
      filename: d.path + '.md',
      content,
      group: d.group,
      order: d.order,
    });
  }
  return {
    docs,
    groups: payload.manifest.groups.map((g) => g.id),
    taglines: Object.fromEntries(payload.manifest.groups.map((g) => [g.id, g.tagline])),
  };
}

/** Join `base` (a folder, possibly '') with a relative `href` and collapse `.` / `..`. */
function resolveRelative(base: string, href: string): string {
  const out: string[] = base ? base.split('/').filter(Boolean) : [];
  for (const seg of href.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') out.pop();
    else out.push(seg);
  }
  return out.join('/');
}

/**
 * Resolve a markdown link to a DocEntry.
 *
 * Inter-doc links in the product bundle are written relative to the doc
 * that contains them, exactly as on disk (lychee validates them there):
 *   - `serve-tls.md` (same folder — the most common shape)
 *   - `../faq.md` (from a subfolder back to top)
 *   - `reference/configuration.md` (from top-level into a subfolder)
 *   - `../reference/metrics.md` (from subfolder to subfolder)
 *
 * `from` is the linking doc's `filename`; the href is resolved against its
 * folder first. A link that does not resolve that way falls back to an
 * exact match on the path under docs/product/ (links written from the
 * root, or callers with no context), then to a bare-filename match across
 * the bundle (legacy links written before the docs moved into folders).
 *
 * Returns undefined if the target isn't in the bundle — the caller falls
 * back to rendering the link as a normal anchor, so a missing target
 * degrades to a user-visible 404 (and CI catches it via lychee before it
 * ever ships).
 */
export function findDocByFilename(
  docs: readonly DocEntry[],
  filename: string,
  from?: string,
): DocEntry | undefined {
  const target = filename.trim().split('#')[0].split('?')[0];
  if (!target.endsWith('.md') || /^[a-z]+:/i.test(target)) return undefined;

  if (from) {
    const folder = from.includes('/') ? from.slice(0, from.lastIndexOf('/')) : '';
    const resolved = resolveRelative(folder, target);
    const hit = docs.find((d) => d.filename === resolved);
    if (hit) return hit;
  }
  const flat = resolveRelative('', target);
  const exact = docs.find((d) => d.filename === flat);
  if (exact) return exact;
  const bare = flat.slice(flat.lastIndexOf('/') + 1);
  return docs.find((d) => d.filename.slice(d.filename.lastIndexOf('/') + 1) === bare);
}
