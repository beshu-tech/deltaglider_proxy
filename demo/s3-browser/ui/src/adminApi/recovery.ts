// === Config DB Recovery ===
import { adminJson } from './core';

interface RecoverDbResponse {
  success: boolean;
  /** 'config_db_key' or 'bootstrap_hash' (a database from before the DB key). */
  key_kind?: string;
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
