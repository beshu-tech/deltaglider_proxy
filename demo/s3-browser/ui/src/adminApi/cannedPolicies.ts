// === Canned Policies ===
import { adminJson } from './core';
import type { IamPermission } from './users';

export interface CannedPolicy {
  name: string;
  description: string;
  permissions: IamPermission[];
}

export async function getCannedPolicies(): Promise<CannedPolicy[]> {
  try {
    return await adminJson<CannedPolicy[]>('/api/admin/policies');
  } catch {
    return [];
  }
}
