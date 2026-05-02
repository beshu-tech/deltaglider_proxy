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
import REF_REPLICATION from '../../../../docs/product/reference/replication.md?raw';

/** Extract the first `# heading` from markdown content */
function extractTitle(content: string): string {
  for (const line of content.split('\n')) {
    const m = line.match(/^#\s+(.+)/);
    if (m) return m[1].trim();
  }
  return 'Untitled';
}

export type DocGroup =
  | 'Start here'
  | 'Deploy to production'
  | 'Authentication & access'
  | 'Day 2 operations'
  | 'Reference';

export const DOC_GROUPS: readonly DocGroup[] = [
  'Start here',
  'Deploy to production',
  'Authentication & access',
  'Day 2 operations',
  'Reference',
] as const;

/** One-line summary of what a group is for — rendered on the landing. */
export const GROUP_TAGLINE: Record<DocGroup, string> = {
  'Start here': 'Install, first bucket, first upload.',
  'Deploy to production': 'Hardening, TLS, backups, upgrades.',
  'Authentication & access': 'OAuth, SigV4, IAM, rate limiting.',
  'Day 2 operations': 'Monitoring, troubleshooting, FAQ.',
  'Reference': 'Config fields, admin API, metrics, internals.',
};

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

const PRODUCT_DOCS: ProductDoc[] = [
  // Start here
  { path: 'README', content: README, group: 'Start here', order: 0 },
  { path: '01-quickstart', content: QUICKSTART, group: 'Start here', order: 10 },
  { path: '10-first-bucket', content: FIRST_BUCKET, group: 'Start here', order: 20 },

  // Deploy to production
  { path: '20-production-deployment', content: PROD_DEPLOY, group: 'Deploy to production', order: 0 },
  { path: '20-production-security-checklist', content: PROD_SECURITY, group: 'Deploy to production', order: 10 },
  { path: '21-upgrade-guide', content: UPGRADE_GUIDE, group: 'Deploy to production', order: 20 },
  { path: '22-kubernetes-helm', content: KUBERNETES_HELM, group: 'Deploy to production', order: 30 },

  // Authentication & access
  { path: 'auth/30-oauth-setup', content: OAUTH_SETUP, group: 'Authentication & access', order: 0 },
  { path: 'auth/31-sigv4-and-iam', content: SIGV4_IAM, group: 'Authentication & access', order: 10 },
  { path: 'auth/32-iam-conditions', content: IAM_CONDITIONS, group: 'Authentication & access', order: 20 },
  { path: 'auth/33-rate-limiting', content: RATE_LIMITING, group: 'Authentication & access', order: 30 },

  // Day 2 operations
  { path: '40-monitoring-and-alerts', content: MONITORING, group: 'Day 2 operations', order: 0 },
  { path: '41-troubleshooting', content: TROUBLESHOOTING, group: 'Day 2 operations', order: 10 },
  { path: '42-faq', content: FAQ, group: 'Day 2 operations', order: 20 },

  // Reference
  { path: 'reference/configuration', content: REF_CONFIGURATION, group: 'Reference', order: 0 },
  { path: 'reference/admin-api', content: REF_ADMIN_API, group: 'Reference', order: 10 },
  { path: 'reference/authentication', content: REF_AUTHENTICATION, group: 'Reference', order: 20 },
  { path: 'reference/metrics', content: REF_METRICS, group: 'Reference', order: 30 },
  { path: 'reference/how-delta-works', content: REF_DELTA, group: 'Reference', order: 40 },
  { path: 'reference/encryption-at-rest', content: REF_ENCRYPTION, group: 'Reference', order: 50 },
  { path: 'reference/declarative-iam', content: REF_DECLARATIVE_IAM, group: 'Reference', order: 60 },
  { path: 'reference/replication', content: REF_REPLICATION, group: 'Reference', order: 70 },
];

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
