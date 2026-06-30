// === Config DB Recovery ===
import { adminFetch, safeJson } from './core';

interface RecoverDbResponse {
  success: boolean;
  correct_hash?: string;
  correct_hash_base64?: string;
  error?: string;
  /** True if the recovered hash was persisted; a restart now comes up unlocked. */
  persisted_for_restart?: boolean;
}

export async function recoverDb(candidatePassword: string): Promise<RecoverDbResponse> {
  const res = await adminFetch('/api/admin/recover-db', 'POST', {
    candidate_password: candidatePassword,
  });
  return safeJson(res);
}
