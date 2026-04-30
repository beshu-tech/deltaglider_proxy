import type { IamPermission, WhoamiResponse } from './adminApi';

export type UiAction = 'read' | 'write' | 'delete' | 'list' | 'admin';

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

