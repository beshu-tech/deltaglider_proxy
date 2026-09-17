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
 * Unit-tested by scripts/docs-bundle-regression-test.mjs.
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
function pathToId(path: string): string {
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

/**
 * Resolve a markdown link to a DocEntry.
 *
 * Inter-doc links in the product bundle take these shapes:
 *   - `faq.md` (top-level)
 *   - `../faq.md` (from a subfolder back to top)
 *   - `reference/configuration.md` (from top-level into a subfolder)
 *   - `../reference/metrics.md` (from subfolder to subfolder)
 *
 * We normalise all of them by stripping leading `./` / `../` segments and
 * the query/anchor, then matching against each doc's `filename` (which
 * already carries its full path under docs/product/).
 *
 * Returns undefined if the target isn't in the bundle — the caller falls
 * back to rendering the link as a normal anchor, so a missing target
 * degrades to a user-visible 404 (and CI catches it via lychee before it
 * ever ships).
 */
export function findDocByFilename(docs: readonly DocEntry[], filename: string): DocEntry | undefined {
  let target = filename.trim();
  target = target.split('#')[0].split('?')[0];
  while (target.startsWith('../')) target = target.slice(3);
  while (target.startsWith('./')) target = target.slice(2);
  if (!target.endsWith('.md')) return undefined;
  return docs.find((d) => d.filename === target);
}
