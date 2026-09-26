/**
 * Admin URL resolution for the 5-group / 15-leaf IA (React-free; the
 * unit test asserts the full remap table).
 *
 * Every URL scheme the app has ever emitted resolves somewhere sensible:
 * the original flat aliases (`/users`), the 4-group `configuration/…`
 * scheme, and the current paths. One exact-match table — no parallel
 * resolution code paths.
 */

export const ADMIN_PATH_REMAP: Record<string, string> = {
  // ── original flat aliases ──
  metrics: 'dashboard',
  users: 'access/users',
  groups: 'access/groups',
  auth: 'access/external-auth',
  backends: 'storage/backends',
  backend: 'storage/backends',
  compression: 'storage/backends',
  encryption: 'storage/backends',
  limits: 'system',
  security: 'system',
  logging: 'system',
  // ── 4-group scheme ──
  'diagnostics/dashboard': 'dashboard',
  'diagnostics/event-outbox': 'integrations/event-outbox',
  'configuration/admission': 'access/admission',
  'configuration/access': 'access/credentials',
  'configuration/access/credentials': 'access/credentials',
  'configuration/access/users': 'access/users',
  'configuration/access/groups': 'access/groups',
  'configuration/access/ext-auth': 'access/external-auth',
  'configuration/storage': 'storage/backends',
  'configuration/storage/backends': 'storage/backends',
  'configuration/storage/buckets': 'storage/buckets',
  'configuration/storage/encryption': 'storage/backends',
  'configuration/storage/replication': 'jobs',
  'configuration/storage/lifecycle': 'jobs',
  'configuration/recovery': 'system',
  'configuration/advanced': 'system',
  'configuration/advanced/listener': 'system',
  'configuration/advanced/caches': 'system',
  'configuration/advanced/limits': 'system',
  'configuration/advanced/logging': 'system',
  'configuration/advanced/sync': 'system',
  'configuration/advanced/event-delivery': 'integrations/event-delivery',
};

/**
 * Resolve a raw admin sub-path to a canonical leaf. `isKnownPath` is the
 * IA membership test (injected so this module stays React-free).
 */
export function resolveAdminPath(raw: string, isKnownPath: (p: string) => boolean): string {
  const path = raw.replace(/^\/+/, '').replace(/\/+$/, '');
  if (!path) return 'dashboard';
  if (path === 'setup') return 'setup';
  const remapped = ADMIN_PATH_REMAP[path];
  if (remapped) return remapped;
  if (isKnownPath(path)) return path;
  return 'dashboard';
}

/** True for a non-empty path that is neither a live leaf nor a legacy alias. */
export function isUnknownAdminPath(raw: string, isKnownPath: (p: string) => boolean): boolean {
  const path = raw.replace(/^\/+/, '').replace(/\/+$/, '');
  if (!path || path === 'setup' || ADMIN_PATH_REMAP[path]) return false;
  return !isKnownPath(path);
}

/**
 * The live leaf nearest to an unknown path: the one sharing the most leading
 * segments, then the most leading characters of the first differing segment
 * ("access/userz" → "access/users"). Nothing in common → the dashboard.
 */
export function nearestAdminPath(raw: string, leaves: string[]): string {
  const segs = raw.replace(/^\/+/, '').replace(/\/+$/, '').split('/');
  let best = 'dashboard';
  let bestScore = 0;
  for (const leaf of leaves) {
    const ls = leaf.split('/');
    let score = 0;
    for (let i = 0; i < Math.min(ls.length, segs.length); i++) {
      if (ls[i] === segs[i]) {
        score += 1000;
        continue;
      }
      let c = 0;
      while (c < ls[i].length && c < segs[i].length && ls[i][c] === segs[i][c]) c++;
      score += c;
      break;
    }
    if (score > bestScore) {
      best = leaf;
      bestScore = score;
    }
  }
  return best;
}
