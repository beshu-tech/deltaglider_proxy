/**
 * Pure, React-free helpers shared by the IAM master-detail panels
 * (UsersPanel / GroupsPanel). Extracting them out of the components lets
 * the list-label and search-filter logic be unit-tested without rendering,
 * and keeps the generic <MasterDetailPanel> presentational-only.
 *
 * IMPORTANT: these are behaviour-preserving copies of the `permissionSummary`
 * functions and the `.filter(...)` predicates that previously lived inline in
 * UsersPanel.tsx and GroupsPanel.tsx. Do not change the truth tables without a
 * matching update to src/__tests__/masterDetailFilter.test.ts.
 */

interface PermissionRule {
  actions: string[];
  resources: string[];
}

/** Minimal shape of an IAM user needed for the list-label summary. */
interface UserSummaryInput {
  permissions: PermissionRule[];
  group_ids?: number[];
  auth_source?: string;
}

/** Minimal shape of an IAM group needed for the list-label summary. */
interface GroupSummaryInput {
  permissions: PermissionRule[];
}

const hasFullAdmin = (perms: PermissionRule[]): boolean =>
  perms.some(p => p.actions.includes('*') && p.resources.includes('*'));

/**
 * Label shown under a user row in the master list.
 *
 * Returns `null` for SSO users with no direct rules — the SSO badge + detail
 * panel already convey context, and a "No access" label there is misleading
 * (Wave 11 UX-5). A user with group memberships but no direct rules surfaces
 * the inheritance instead of "No access".
 */
export function userPermissionSummary(user: UserSummaryInput): string | null {
  const groupCount = user.group_ids?.length ?? 0;
  if (user.permissions.length === 0) {
    if (user.auth_source === 'external') return null;
    if (groupCount > 0) {
      return `${groupCount} group${groupCount !== 1 ? 's' : ''} (inherited)`;
    }
    return 'No access';
  }
  if (hasFullAdmin(user.permissions)) return 'Full admin';
  const rulePart = `${user.permissions.length} rule${user.permissions.length !== 1 ? 's' : ''}`;
  if (groupCount > 0) {
    return `${rulePart} · ${groupCount} group${groupCount !== 1 ? 's' : ''}`;
  }
  return rulePart;
}

/** Label shown under a group row in the master list. */
export function groupPermissionSummary(group: GroupSummaryInput): string {
  if (group.permissions.length === 0) return 'No permissions';
  if (hasFullAdmin(group.permissions)) return 'Full access';
  return `${group.permissions.length} rule${group.permissions.length !== 1 ? 's' : ''}`;
}

/**
 * Case-insensitive search filter for the master list. `searchableFields`
 * returns the strings a given item is matched against (e.g. name +
 * access_key_id for users, name for groups). An empty/whitespace query
 * returns the list unchanged (identity), matching the prior inline behaviour.
 */
export function filterItems<T>(
  items: T[],
  search: string,
  searchableFields: (item: T) => Array<string | null | undefined>,
): T[] {
  const q = search.toLowerCase();
  if (!search) return items;
  return items.filter(item =>
    searchableFields(item).some(field => (field ?? '').toLowerCase().includes(q)),
  );
}
