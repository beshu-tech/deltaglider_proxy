// Central registry of product-facing docs bundled into the binary.
//
// IMPORTANT: only markdown files under `docs/product/` may be imported
// here. Files under `docs/dev/` are for GitHub/contributors only and
// must never be bundled. CI enforces this via
// `scripts/check-docs-registry.sh`.
//
// Adding a doc: import below, then add an entry to `PRODUCT_DOCS`
// with the right group and `order` field. Title is derived from the
// first `# heading` in the file — do not pass it here.
//
// Removing a doc: delete the import and the PRODUCT_DOCS entry.
// CI will fail if the underlying .md file is left around without an
// entry (and vice versa).

import README from '../../../../docs/product/README.md?raw';
import QUICKSTART from '../../../../docs/product/01-quickstart.md?raw';
import FIRST_BUCKET from '../../../../docs/product/10-first-bucket.md?raw';
import PROD_DEPLOY from '../../../../docs/product/20-production-deployment.md?raw';
import PROD_SECURITY from '../../../../docs/product/20-production-security-checklist.md?raw';
import UPGRADE_GUIDE from '../../../../docs/product/21-upgrade-guide.md?raw';
import KUBERNETES_HELM from '../../../../docs/product/22-kubernetes-helm.md?raw';
import DOCKER_COMPOSE from '../../../../docs/product/23-docker-compose.md?raw';
import OAUTH_SETUP from '../../../../docs/product/auth/30-oauth-setup.md?raw';
import SIGV4_IAM from '../../../../docs/product/auth/31-sigv4-and-iam.md?raw';
import IAM_CONDITIONS from '../../../../docs/product/auth/32-iam-conditions.md?raw';
import RATE_LIMITING from '../../../../docs/product/auth/33-rate-limiting.md?raw';
import MONITORING from '../../../../docs/product/40-monitoring-and-alerts.md?raw';
import TROUBLESHOOTING from '../../../../docs/product/41-troubleshooting.md?raw';
import FAQ from '../../../../docs/product/42-faq.md?raw';
import REF_CONFIGURATION from '../../../../docs/product/reference/configuration.md?raw';
import REF_ADMIN_API from '../../../../docs/product/reference/admin-api.md?raw';
import REF_AUTHENTICATION from '../../../../docs/product/reference/authentication.md?raw';
import REF_METRICS from '../../../../docs/product/reference/metrics.md?raw';
import REF_DELTA from '../../../../docs/product/reference/how-delta-works.md?raw';
import REF_ENCRYPTION from '../../../../docs/product/reference/encryption-at-rest.md?raw';
import REF_DECLARATIVE_IAM from '../../../../docs/product/reference/declarative-iam.md?raw';
import REF_JOBS from '../../../../docs/product/reference/jobs.md?raw';
import REF_REPLICATION from '../../../../docs/product/reference/replication.md?raw';
import REF_LIFECYCLE from '../../../../docs/product/reference/lifecycle.md?raw';
import REF_EVENT_OUTBOX from '../../../../docs/product/reference/event-outbox.md?raw';

// Grouping + ordering come from the shared manifest — the SINGLE source of
// truth, read by BOTH this in-product viewer and the marketing-website docs
// renderer (marketing/src/pages/docs). The `?raw` content imports above must
// stay static (Vite requirement); only the metadata lives in the manifest.
import manifest from '../../../../docs/product/manifest.json';

/**
 * Raw markdown keyed by the manifest `path` (relative to docs/product/,
 * no extension). The manifest drives iteration order; this map supplies
 * each entry's content. A manifest path with no content here is a build-time
 * error surfaced below — keeps the two lists honest.
 */
const CONTENT_BY_PATH: Record<string, string> = {
  'README': README,
  '01-quickstart': QUICKSTART,
  '10-first-bucket': FIRST_BUCKET,
  '20-production-deployment': PROD_DEPLOY,
  '20-production-security-checklist': PROD_SECURITY,
  '21-upgrade-guide': UPGRADE_GUIDE,
  '22-kubernetes-helm': KUBERNETES_HELM,
  '23-docker-compose': DOCKER_COMPOSE,
  'auth/30-oauth-setup': OAUTH_SETUP,
  'auth/31-sigv4-and-iam': SIGV4_IAM,
  'auth/32-iam-conditions': IAM_CONDITIONS,
  'auth/33-rate-limiting': RATE_LIMITING,
  '40-monitoring-and-alerts': MONITORING,
  '41-troubleshooting': TROUBLESHOOTING,
  '42-faq': FAQ,
  'reference/configuration': REF_CONFIGURATION,
  'reference/admin-api': REF_ADMIN_API,
  'reference/authentication': REF_AUTHENTICATION,
  'reference/metrics': REF_METRICS,
  'reference/how-delta-works': REF_DELTA,
  'reference/encryption-at-rest': REF_ENCRYPTION,
  'reference/declarative-iam': REF_DECLARATIVE_IAM,
  'reference/jobs': REF_JOBS,
  'reference/replication': REF_REPLICATION,
  'reference/lifecycle': REF_LIFECYCLE,
  'reference/event-outbox': REF_EVENT_OUTBOX,
};

/** Extract the first `# heading` from markdown content */
function extractTitle(content: string): string {
  for (const line of content.split('\n')) {
    const m = line.match(/^#\s+(.+)/);
    if (m) return m[1].trim();
  }
  return 'Untitled';
}

// Group ids + taglines + ordering all derive from the shared manifest.
export type DocGroup = string;

export const DOC_GROUPS: readonly DocGroup[] = manifest.groups.map((g) => g.id);

/** One-line summary of what a group is for — rendered on the landing. */
export const GROUP_TAGLINE: Record<DocGroup, string> = Object.fromEntries(
  manifest.groups.map((g) => [g.id, g.tagline]),
);

export interface DocEntry {
  id: string;
  title: string;
  /** Path relative to docs/product/. Used by findDocByFilename to resolve links. */
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

interface ProductDoc {
  /** Path under docs/product/ — used as the doc's URL path under /_/docs/ */
  path: string;
  content: string;
  group: DocGroup;
  order: number;
}

// Derived from the shared manifest: iterate the manifest entries (which own
// group + order) and attach the statically-imported content for each path. A
// manifest path with no matching content throws loudly at module load — that
// only happens if a doc was added to the manifest without an import here, which
// CI (check-docs-registry.sh) also guards.
const PRODUCT_DOCS: ProductDoc[] = manifest.docs.map((d) => {
  const content = CONTENT_BY_PATH[d.path];
  if (content === undefined) {
    throw new Error(
      `docs manifest lists "${d.path}" but no ?raw import is registered in CONTENT_BY_PATH (docs-imports.ts)`,
    );
  }
  return { path: d.path, content, group: d.group, order: d.order };
});

/**
 * Convert a doc path ("auth/30-oauth-setup") into a URL-safe id
 * ("auth-30-oauth-setup"). Subfolder segments collapse to flat ids
 * because the doc URL space (`/_/docs/:id`) is intentionally flat —
 * it's a product surface, not a filesystem browser.
 */
function pathToId(path: string): string {
  return path.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
}

export const DOCS: DocEntry[] = PRODUCT_DOCS.map((d) => ({
  id: pathToId(d.path),
  title: extractTitle(d.content),
  filename: d.path + '.md',
  content: d.content,
  group: d.group,
  order: d.order,
}));

/**
 * Resolve a markdown link to a DocEntry.
 *
 * Inter-doc links in the product bundle take three shapes:
 *   - `01-quickstart.md` (top-level)
 *   - `../20-production-deployment.md` (from a subfolder back to top)
 *   - `reference/configuration.md` (from top-level into a subfolder)
 *   - `../reference/metrics.md` (from subfolder to subfolder)
 *
 * We normalise all of them by:
 *   1. Stripping leading `./` or `../` segments.
 *   2. Matching against each doc's `filename` (which already carries
 *      its full path under docs/product/).
 *
 * Returns undefined if the target isn't in the bundle — the caller
 * falls back to rendering the link as a normal anchor, so a missing
 * target degrades to a user-visible 404 (and CI catches it via
 * lychee before it ever ships).
 */
export function findDocByFilename(filename: string): DocEntry | undefined {
  // Strip common relative-path segments. After this, `filename` is
  // either a bare name ("foo.md"), a subfolder path ("auth/foo.md"),
  // or junk. `DOCS` filenames always have the form "<path>.md" where
  // `path` is the canonical PRODUCT_DOCS key.
  let target = filename.trim();
  // Strip query string / anchor — we match on path only.
  target = target.split('#')[0].split('?')[0];
  // Normalise away leading ../ and ./ sequences.
  while (target.startsWith('../')) target = target.slice(3);
  while (target.startsWith('./')) target = target.slice(2);

  const exact = DOCS.find((d) => d.filename === target);
  if (exact) return exact;

  // Fallback: bare filename match across all docs (handles legacy
  // links like `CONFIGURATION.md` that predate the subfolder move).
  // We deliberately do NOT match partial paths — that would make
  // renaming docs unsafe.
  const bare = target.replace(/^.*\//, '');
  return DOCS.find((d) => d.filename.replace(/^.*\//, '') === bare);
}
