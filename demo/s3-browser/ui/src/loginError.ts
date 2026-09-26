/**
 * Sign-in failure text, shared by every sign-in form (password, IAM keys,
 * re-login). Pure so src/__tests__/loginError.test.ts can check it.
 *
 * A 429 is a lockout (the per-IP limiter in `rate_limiter.rs`): it says
 * nothing about the password, and the right password is refused too until
 * it ends. So it must read as a lockout, with the wait when the server
 * sends one (`Retry-After`, or `retry_after_secs` in the JSON body).
 */

/** Seconds to wait, from `Retry-After` (delta-seconds or HTTP date) or the body. */
export function retryAfterSeconds(
  header: string | null,
  body?: { retry_after_secs?: unknown },
  nowMs: number = Date.now(),
): number | null {
  if (header) {
    const trimmed = header.trim();
    if (/^\d+$/.test(trimmed)) return Number(trimmed);
    const at = Date.parse(trimmed);
    if (!Number.isNaN(at)) return Math.max(0, Math.round((at - nowMs) / 1000));
  }
  const fromBody = body?.retry_after_secs;
  return typeof fromBody === 'number' && Number.isFinite(fromBody) && fromBody >= 0 ? fromBody : null;
}

export function loginFailureMessage(f: {
  status: number;
  error?: string;
  retryAfterSecs?: number | null;
}): string {
  if (f.status === 429) {
    const secs = f.retryAfterSecs;
    if (secs === null || secs === undefined) {
      return 'Too many sign-in attempts. Wait a few minutes, then try again.';
    }
    const wait = secs < 60 ? `${Math.max(1, secs)} s` : `${Math.ceil(secs / 60)} min`;
    return `Too many sign-in attempts. Try again in ${wait}.`;
  }
  const detail = f.error && f.error !== 'Login failed' ? f.error : 'wrong password.';
  return `Login failed: ${detail}`;
}
