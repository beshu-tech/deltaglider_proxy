import type { IamPermission, WhoamiResponse } from './adminApi';

type UiAction = 'read' | 'write' | 'delete' | 'list' | 'admin';

function actionMatches(permissionActions: string[], action: UiAction): boolean {
  const aliases: Record<UiAction, string[]> = {
    read: ['read', 's3:getobject'],
    write: ['write', 's3:putobject'],
    delete: ['delete', 's3:deleteobject'],
    list: ['list', 's3:listbucket', 's3:listallmybuckets'],
    admin: ['admin', 's3:*', 's3:createbucket', 's3:deletebucket'],
  };
  return permissionActions.some(a => {
    const normalized = a.toLowerCase();
    return normalized === '*' || normalized === 's3:*' || normalized === action || aliases[action].includes(normalized);
  });
}

function resourceMatches(resource: string, bucket: string, key = ''): boolean {
  if (resource === '*') return true;
  if (!bucket) return resource === '*';
  const target = key ? `${bucket}/${key}` : bucket;
  if (resource === bucket || resource === `${bucket}/*`) return true;
  if (resource.endsWith('*')) return target.startsWith(resource.slice(0, -1));
  return resource === target;
}

function globMatches(pattern: string, value: string): boolean {
  const escaped = pattern.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*');
  return new RegExp(`^${escaped}$`).test(value);
}

function conditionValues(value: string | string[]): string[] {
  return Array.isArray(value) ? value : [value];
}

function normalizeFolderPrefix(prefix: string): string {
  const trimmed = prefix.trim().replace(/^\/+/, '').replace(/\/{2,}/g, '/');
  if (!trimmed) return '';
  return trimmed.endsWith('/') ? trimmed : `${trimmed}/`;
}

function writablePrefixFromResource(resource: string, bucket: string): string | null {
  if (resource === '*' || resource === bucket || resource === `${bucket}/*`) return '';
  const bucketRoot = `${bucket}/`;
  if (!resource.startsWith(bucketRoot)) return null;
  if (!resource.endsWith('/*')) return null;
  return normalizeFolderPrefix(resource.slice(bucketRoot.length, -1));
}

function conditionsMatchForUi(
  conditions: IamPermission['conditions'],
  action: UiAction,
  key: string,
  denyRule: boolean
): boolean {
  if (!conditions) return true;

  for (const [operator, entries] of Object.entries(conditions)) {
    const op = operator.toLowerCase();
    for (const [conditionKey, rawValue] of Object.entries(entries)) {
      const ck = conditionKey.toLowerCase();
      if (action === 'list' && ck === 's3:prefix') {
        const values = conditionValues(rawValue);
        if (op === 'stringequals') {
          if (!values.includes(key)) return false;
          continue;
        }
        if (op === 'stringlike') {
          if (!values.some(v => globMatches(v, key))) return false;
          continue;
        }
      }

      // The browser cannot know request-only context like source IP.
      // Deny rules fail closed; allow rules must be proven applicable.
      return denyRule;
    }
  }

  return true;
}

export function canUse(identity: WhoamiResponse | null, action: UiAction, bucket = '', key = ''): boolean {
  if (!identity) return false;
  if (identity.mode === 'open') return true;
  if (identity.mode === 'bootstrap') return identity.user?.is_admin === true;
  if (identity.user?.is_admin) return true;

  const permissions = identity.user?.permissions ?? [];
  const denied = permissions.some(p =>
    (p.effect ?? 'Allow').toLowerCase() === 'deny' &&
    conditionsMatchForUi(p.conditions, action, key, true) &&
    actionMatches(p.actions, action) &&
    p.resources.some(r => resourceMatches(r, bucket, key))
  );
  if (denied) return false;

  return permissions.some(p =>
    (p.effect ?? 'Allow').toLowerCase() !== 'deny' &&
    conditionsMatchForUi(p.conditions, action, key, false) &&
    actionMatches(p.actions, action) &&
    p.resources.some(r => resourceMatches(r, bucket, key))
  );
}

/** Derive writable folder roots for this user in a bucket. */
export function writablePrefixesForBucket(identity: WhoamiResponse | null, bucket: string): string[] {
  if (!identity || !bucket) return [];
  if (identity.mode === 'open' || identity.mode === 'bootstrap' || identity.user?.is_admin) return [''];

  const set = new Set<string>();
  const permissions = identity.user?.permissions ?? [];
  for (const permission of permissions) {
    if ((permission.effect ?? 'Allow').toLowerCase() === 'deny') continue;
    if (!actionMatches(permission.actions ?? [], 'write')) continue;
    for (const resource of permission.resources ?? []) {
      const derived = writablePrefixFromResource(resource, bucket);
      if (derived === null) continue;
      // Filter through the effective policy evaluator so deny rules still win.
      if (canUse(identity, 'write', bucket, derived)) {
        set.add(derived);
      }
    }
  }
  return Array.from(set).sort((a, b) => a.localeCompare(b));
}

/**
 * At a given location, expose immediate child folders implied by writable prefixes.
 * Example: current "team/" + writable "team/a/builds/" => virtual child "team/a/".
 */
export function virtualWritableChildren(
  currentPrefix: string,
  realFolders: string[],
  writablePrefixes: string[],
): string[] {
  const current = normalizeFolderPrefix(currentPrefix);
  const realSet = new Set(realFolders.map(normalizeFolderPrefix));
  const out = new Set<string>();

  for (const writablePrefix of writablePrefixes) {
    const writable = normalizeFolderPrefix(writablePrefix);
    if (!writable || writable === current) continue;
    if (current && !writable.startsWith(current)) continue;
    if (!current && writable === '') continue;

    const suffix = current ? writable.slice(current.length) : writable;
    if (!suffix) continue;
    const childName = suffix.split('/')[0];
    if (!childName) continue;
    const child = current ? `${current}${childName}/` : `${childName}/`;
    if (realSet.has(child)) continue;
    out.add(child);
  }

  return Array.from(out).sort((a, b) => a.localeCompare(b));
}

/** Folder row implied only by IAM writable-prefix inference (no ListObjects entry). */
export function isVirtualFolderPrefix(prefix: string, virtualPrefixes: Iterable<string>): boolean {
  const normalizedPrefix = normalizeFolderPrefix(prefix);
  if (!normalizedPrefix) return false;
  const virtualSet = new Set(Array.from(virtualPrefixes, normalizeFolderPrefix));
  return virtualSet.has(normalizedPrefix);
}

/**
 * Whether the UI may call the admin usage-scanner API for this prefix.
 * Requires an admin GUI session; virtual prefixes are never scanned.
 */
export function canRequestPrefixUsageScan(
  prefix: string,
  virtualPrefixes: Iterable<string>,
  hasAdminSession: boolean,
): boolean {
  if (!hasAdminSession) return false;
  return !isVirtualFolderPrefix(prefix, virtualPrefixes);
}

