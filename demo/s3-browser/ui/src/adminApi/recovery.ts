// === Config DB Recovery ===
import { adminJson } from './core';

interface RecoverDbResponse {
  success: boolean;
  correct_hash?: string;
  correct_hash_base64?: string;
  error?: string;
}

export async function recoverDb(candidatePassword: string): Promise<RecoverDbResponse> {
  return adminJson('/api/admin/recover-db', {
    method: 'POST',
    body: { candidate_password: candidatePassword },
  });
}
