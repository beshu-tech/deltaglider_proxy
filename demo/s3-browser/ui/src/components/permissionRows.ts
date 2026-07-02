import type { IamPermission } from '../adminApi';
import { normalizeResourcePattern } from '../storagePath';

export interface PermissionRow {
  /**
   * Stable UI-only identity for React keys + expanded-state tracking.
   * Survives array reordering/deletion so a row's DOM (focus, IME
   * composition, expanded conditions) never re-associates to a sibling.
   * Assigned by `permissionsToRows` / `freshPermissionRowId`; NEVER sent to
   * the server (`rowsToPermissions` drops it). Optional so literals/presets
   * compile — PermissionEditor lazily backfills any row missing it.
   */
  _uiId?: string;
  effect: string;
  actions: string[];
  /** Resource patterns as an ARRAY — never a comma-joined string (a pattern
   *  may legitimately contain a comma). Edited via ResourcePatternInput. */
  resources: string[];
  conditions?: Record<string, Record<string, string | string[]>>;
}

let permissionRowIdCounter = 0;

/** Monotonic, collision-free UI row id (stable React key; never reused). */
export function freshPermissionRowId(): string {
  permissionRowIdCounter += 1;
  return `perm-${permissionRowIdCounter}`;
}

export function permissionsToRows(perms: IamPermission[]): PermissionRow[] {
  return perms.map(p => ({
    _uiId: freshPermissionRowId(),
    effect: p.effect || 'Allow',
    actions: [...p.actions],
    resources: [...p.resources],
    conditions: p.conditions,
  }));
}

export function rowsToPermissions(rows: PermissionRow[]): IamPermission[] {
  return rows
    .filter(r => r.actions.length > 0 && r.resources.some(res => res.trim() !== ''))
    .map(r => {
      const perm: IamPermission = {
        id: 0,
        effect: r.effect || 'Allow',
        actions: r.actions,
        resources: r.resources.map(s => normalizeResourcePattern(s)).filter(Boolean),
      };
      // Only include conditions if at least one is non-empty
      if (r.conditions && Object.keys(r.conditions).length > 0) {
        const cleaned: Record<string, Record<string, string | string[]>> = {};
        for (const [op, kv] of Object.entries(r.conditions)) {
          const cleanedKv: Record<string, string | string[]> = {};
          for (const [k, v] of Object.entries(kv)) {
            if (typeof v === 'string' ? v.trim() : v.length > 0) {
              cleanedKv[k] = v;
            }
          }
          if (Object.keys(cleanedKv).length > 0) {
            cleaned[op] = cleanedKv;
          }
        }
        if (Object.keys(cleaned).length > 0) {
          perm.conditions = cleaned;
        }
      }
      return perm;
    });
}
