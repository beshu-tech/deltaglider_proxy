// === Config DB Recovery ===
import { adminFetch, refusalBody, safeJson } from './core';

interface RecoverDbResponse {
  success: boolean;
  /** 'config_db_key' or 'bootstrap_hash' (a database from before the DB key). */
  key_kind?: string;
  correct_hash?: string;
  correct_hash_base64?: string;
  error?: string;
}

/** A wrong key answers 401 (404/400 for other refusals) with
 *  `{success: false, error}`: the caller shows that sentence as it is. */
export async function recoverDb(candidatePassword: string): Promise<RecoverDbResponse> {
  const res = await adminFetch('/api/admin/recover-db', 'POST', { candidate_password: candidatePassword });
  const refused = await refusalBody<RecoverDbResponse>(res, (b) => b.success === false);
  if (refused) return refused;
  return safeJson<RecoverDbResponse>(res);
}
