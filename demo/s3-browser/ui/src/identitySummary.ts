/**
 * The account-menu header: who is signed in and how. React-free so
 * src/__tests__/identitySummary.test.ts can check it.
 */
import type { WhoamiResponse } from './adminApi';

export interface IdentitySummary {
  name: string;
  detail: string;
}

export function identitySummary(
  identity: WhoamiResponse | null,
  accessKeyId: string | undefined,
): IdentitySummary {
  if (!identity) return { name: accessKeyId || 'Signed in', detail: 'Reading your account…' };
  if (identity.mode === 'open') {
    return { name: 'Open access', detail: 'Authentication is off: every request has full access.' };
  }
  const user = identity.user;
  if (identity.mode === 'bootstrap') {
    return {
      name: user?.name || 'Administrator',
      detail: 'Bootstrap mode: one shared administrator credential, no IAM users yet.',
    };
  }
  // A bootstrap-password session in IAM mode: whoami reports a synthetic
  // user (access key 'bootstrap'), so decide on the session's auth method.
  if (identity.auth_method === 'bootstrap') {
    return {
      name: 'Administrator',
      detail: 'Signed in with the bootstrap password, not as an IAM user.',
    };
  }
  const key = user?.access_key_id || accessKeyId;
  return {
    name: user?.name || key || 'IAM user',
    detail: `${user?.is_admin ? 'Administrator' : 'User'} (IAM)${key ? ` · key ${key}` : ''}`,
  };
}
