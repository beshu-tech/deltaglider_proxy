/**
 * Sign in again without leaving the page. When an admin request answers 401
 * mid-edit, a caller awaits `requestRelogin()`: the mounted ReloginModal asks
 * for the credentials and answers true once a new session exists, so the
 * caller retries the same request with the same edits. With no modal mounted,
 * or when the admin cancels, it answers false and the caller fails as before.
 */
type Handler = () => Promise<boolean>;

let handler: Handler | null = null;
let pending: Promise<boolean> | null = null;

/** The modal registers itself; returns the unregister function. */
export function registerReloginHandler(fn: Handler): () => void {
  handler = fn;
  return () => {
    if (handler === fn) handler = null;
  };
}

/** One prompt for any number of concurrent 401s. */
export function requestRelogin(): Promise<boolean> {
  if (!handler) return Promise.resolve(false);
  if (!pending) {
    pending = handler().finally(() => {
      pending = null;
    });
  }
  return pending;
}
