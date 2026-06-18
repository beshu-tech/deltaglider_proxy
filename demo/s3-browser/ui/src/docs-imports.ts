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
import TUT_DELTA from '../../../../docs/product/tutorials/first-delta-savings.md?raw';
import TUT_SECURE from '../../../../docs/product/tutorials/secure-your-proxy.md?raw';
import TUT_K8S from '../../../../docs/product/tutorials/kubernetes-hello-world.md?raw';
import REF_RATE_LIMITS from '../../../../docs/product/reference/rate-limits.md?raw';
import FAQ from '../../../../docs/product/faq.md?raw';
import HT_GO_TO_PRODUCTION from '../../../../docs/product/how-to/go-to-production.md?raw';
import HT_DEPLOY_WITH_DOCKER_COMPOSE from '../../../../docs/product/how-to/deploy-with-docker-compose.md?raw';
import HT_DEPLOY_ON_KUBERNETES from '../../../../docs/product/how-to/deploy-on-kubernetes.md?raw';
import HT_SERVE_TLS from '../../../../docs/product/how-to/serve-tls.md?raw';
import HT_UPGRADE from '../../../../docs/product/how-to/upgrade.md?raw';
import HT_BACK_UP_AND_RESTORE from '../../../../docs/product/how-to/back-up-and-restore.md?raw';
import HT_RUN_MULTIPLE_INSTANCES from '../../../../docs/product/how-to/run-multiple-instances.md?raw';
import HT_MONITOR_WITH_PROMETHEUS from '../../../../docs/product/how-to/monitor-with-prometheus.md?raw';
import HT_TRACE_REQUESTS from '../../../../docs/product/how-to/trace-requests.md?raw';
import HT_TROUBLESHOOTING from '../../../../docs/product/how-to/troubleshooting.md?raw';
import HT_ROUTE_A_BUCKET_TO_A_BACKEND from '../../../../docs/product/how-to/route-a-bucket-to-a-backend.md?raw';
import HT_MIGRATE_EXISTING_DATA_INTO_THE_PROXY from '../../../../docs/product/how-to/migrate-existing-data-into-the-proxy.md?raw';
import HT_MOVE_A_BUCKET_BETWEEN_BACKENDS from '../../../../docs/product/how-to/move-a-bucket-between-backends.md?raw';
import HT_SET_BUCKET_COMPRESSION_AND_QUOTAS from '../../../../docs/product/how-to/set-bucket-compression-and-quotas.md?raw';
import HT_REPLICATE_A_BUCKET from '../../../../docs/product/how-to/replicate-a-bucket.md?raw';
import HT_EXPIRE_AND_ARCHIVE_OBJECTS from '../../../../docs/product/how-to/expire-and-archive-objects.md?raw';
import HT_ENCRYPT_DATA_AT_REST from '../../../../docs/product/how-to/encrypt-data-at-rest.md?raw';
import HT_ROTATE_ENCRYPTION_KEYS from '../../../../docs/product/how-to/rotate-encryption-keys.md?raw';
import HT_SEND_EVENT_NOTIFICATIONS from '../../../../docs/product/how-to/send-event-notifications.md?raw';
import HT_CREATE_IAM_USERS from '../../../../docs/product/how-to/create-iam-users.md?raw';
import HT_RESTRICT_ACCESS_WITH_CONDITIONS from '../../../../docs/product/how-to/restrict-access-with-conditions.md?raw';
import HT_SET_UP_SSO from '../../../../docs/product/how-to/set-up-sso.md?raw';
import HT_MANAGE_IAM_AS_CODE from '../../../../docs/product/how-to/manage-iam-as-code.md?raw';
import HT_GATE_REQUESTS_WITH_ADMISSION_RULES from '../../../../docs/product/how-to/gate-requests-with-admission-rules.md?raw';
import HT_PUBLISH_A_PUBLIC_FOLDER from '../../../../docs/product/how-to/publish-a-public-folder.md?raw';
import REF_CONFIGURATION from '../../../../docs/product/reference/configuration.md?raw';
import REF_S3_API_COMPATIBILITY from '../../../../docs/product/reference/s3-api-compatibility.md?raw';
import REF_ADMIN_API from '../../../../docs/product/reference/admin-api.md?raw';
import REF_AUTHENTICATION from '../../../../docs/product/reference/authentication.md?raw';
import REF_IAM_PERMISSIONS from '../../../../docs/product/reference/iam-permissions.md?raw';
import REF_CLI from '../../../../docs/product/reference/cli.md?raw';
import REF_METRICS from '../../../../docs/product/reference/metrics.md?raw';
import EXP_DELTA from '../../../../docs/product/explanation/delta-compression.md?raw';
import EXP_MULTIBACKEND from '../../../../docs/product/explanation/multi-backend-architecture.md?raw';
import EXP_SECURITY from '../../../../docs/product/explanation/security-model.md?raw';
import EXP_ENCRYPTION from '../../../../docs/product/explanation/encryption-at-rest.md?raw';
import EXP_JOBS from '../../../../docs/product/explanation/jobs-and-durability.md?raw';
import REF_ENCRYPTION from '../../../../docs/product/reference/encryption.md?raw';
import REF_DECLARATIVE_IAM from '../../../../docs/product/reference/declarative-iam.md?raw';
import REF_JOBS from '../../../../docs/product/reference/jobs.md?raw';
import REF_REPLICATION from '../../../../docs/product/reference/replication.md?raw';
import REF_LIFECYCLE from '../../../../docs/product/reference/lifecycle.md?raw';
import REF_EVENT_OUTBOX from '../../../../docs/product/reference/event-outbox.md?raw';
import REF_CAPACITY from '../../../../docs/product/reference/capacity-planning.md?raw';
import EXP_MIGRATION from '../../../../docs/product/explanation/how-migration-works.md?raw';
import EXP_VERSIONING from '../../../../docs/product/explanation/versioning-vs-s3-versioning.md?raw';
// Generated from the root CHANGELOG.md by scripts/gen-changelog-doc.sh —
// do not hand-edit docs/product/changelog.md (CI checks it stays in sync).
import CHANGELOG from '../../../../docs/product/changelog.md?raw';

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
  'tutorials/first-delta-savings': TUT_DELTA,
  'tutorials/secure-your-proxy': TUT_SECURE,
  'tutorials/kubernetes-hello-world': TUT_K8S,
  'reference/rate-limits': REF_RATE_LIMITS,
  'faq': FAQ,
  'how-to/go-to-production': HT_GO_TO_PRODUCTION,
  'how-to/deploy-with-docker-compose': HT_DEPLOY_WITH_DOCKER_COMPOSE,
  'how-to/deploy-on-kubernetes': HT_DEPLOY_ON_KUBERNETES,
  'how-to/serve-tls': HT_SERVE_TLS,
  'how-to/upgrade': HT_UPGRADE,
  'how-to/back-up-and-restore': HT_BACK_UP_AND_RESTORE,
  'how-to/run-multiple-instances': HT_RUN_MULTIPLE_INSTANCES,
  'how-to/monitor-with-prometheus': HT_MONITOR_WITH_PROMETHEUS,
  'how-to/trace-requests': HT_TRACE_REQUESTS,
  'how-to/troubleshooting': HT_TROUBLESHOOTING,
  'how-to/route-a-bucket-to-a-backend': HT_ROUTE_A_BUCKET_TO_A_BACKEND,
  'how-to/migrate-existing-data-into-the-proxy': HT_MIGRATE_EXISTING_DATA_INTO_THE_PROXY,
  'how-to/move-a-bucket-between-backends': HT_MOVE_A_BUCKET_BETWEEN_BACKENDS,
  'how-to/set-bucket-compression-and-quotas': HT_SET_BUCKET_COMPRESSION_AND_QUOTAS,
  'how-to/replicate-a-bucket': HT_REPLICATE_A_BUCKET,
  'how-to/expire-and-archive-objects': HT_EXPIRE_AND_ARCHIVE_OBJECTS,
  'how-to/encrypt-data-at-rest': HT_ENCRYPT_DATA_AT_REST,
  'how-to/rotate-encryption-keys': HT_ROTATE_ENCRYPTION_KEYS,
  'how-to/send-event-notifications': HT_SEND_EVENT_NOTIFICATIONS,
  'how-to/create-iam-users': HT_CREATE_IAM_USERS,
  'how-to/restrict-access-with-conditions': HT_RESTRICT_ACCESS_WITH_CONDITIONS,
  'how-to/set-up-sso': HT_SET_UP_SSO,
  'how-to/manage-iam-as-code': HT_MANAGE_IAM_AS_CODE,
  'how-to/gate-requests-with-admission-rules': HT_GATE_REQUESTS_WITH_ADMISSION_RULES,
  'how-to/publish-a-public-folder': HT_PUBLISH_A_PUBLIC_FOLDER,
  'reference/configuration': REF_CONFIGURATION,
  'reference/s3-api-compatibility': REF_S3_API_COMPATIBILITY,
  'reference/admin-api': REF_ADMIN_API,
  'reference/authentication': REF_AUTHENTICATION,
  'reference/iam-permissions': REF_IAM_PERMISSIONS,
  'reference/cli': REF_CLI,
  'reference/metrics': REF_METRICS,
  'explanation/delta-compression': EXP_DELTA,
  'explanation/how-migration-works': EXP_MIGRATION,
  'explanation/multi-backend-architecture': EXP_MULTIBACKEND,
  'explanation/security-model': EXP_SECURITY,
  'explanation/encryption-at-rest': EXP_ENCRYPTION,
  'explanation/jobs-and-durability': EXP_JOBS,
  'explanation/versioning-vs-s3-versioning': EXP_VERSIONING,
  'reference/encryption': REF_ENCRYPTION,
  'reference/declarative-iam': REF_DECLARATIVE_IAM,
  'reference/jobs': REF_JOBS,
  'reference/replication': REF_REPLICATION,
  'reference/lifecycle': REF_LIFECYCLE,
  'reference/event-outbox': REF_EVENT_OUTBOX,
  'reference/capacity-planning': REF_CAPACITY,
  'changelog': CHANGELOG,
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
 *   - `faq.md` (top-level)
 *   - `../faq.md` (from a subfolder back to top)
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
