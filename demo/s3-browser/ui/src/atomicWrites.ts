/**
 * Encryption needs atomic writes (N2 of
 * docs/plan/encryption-needs-atomic-writes.md).
 *
 * The proxy converts and re-keys an encrypted bucket by rewriting objects
 * with conditional writes. A backend that does not support them (Backblaze
 * B2) cannot encrypt with the proxy: the server refuses the change, and an
 * encrypted bucket already on such a backend is read-only. A backend that
 * the proxy could not verify is not read-only: its writes wait (503, which
 * clients retry), and the encryption actions are refused until a check
 * succeeds. The UI reads the server's verdict (GET /backends `capability`)
 * and the server's own sentences (`proxy_unavailable`, `capability_words`),
 * and says so before the user tries.
 *
 * Pure: no React, no fetch.
 */
import type { BackendCapabilityVerdict } from './adminApi';
import { CAPABILITY_DOC_URL } from './linkifyDocUrl';

/** What the UI knows about the atomic writes of a backend. */
export type AtomicWrites =
  /** Proven (a probe, or a filesystem backend). */
  | { kind: 'atomic' }
  /** No verdict yet: the action checks. */
  | { kind: 'unchecked' }
  /** Not proven: `non-cas` = the backend does not support them;
   *  `unknown` = the proxy could not verify them. */
  | { kind: 'missing'; verdict: 'non-cas' | 'unknown' };

/** Pure: the atomic-writes state of a backend from its verdict. */
export function atomicWrites(cap: BackendCapabilityVerdict | undefined): AtomicWrites {
  if (!cap) return { kind: 'unchecked' };
  if (cap.verdict === 'cas-verified') return { kind: 'atomic' };
  return { kind: 'missing', verdict: cap.verdict };
}

/** Pure: why proxy encryption is not offered on `backend`: the server's
 *  sentence (`proxy_unavailable`) when it sent one, else the same rule in
 *  the UI's words. Ends with the doc link, which LinkifiedText renders as
 *  a labelled link. Null when proxy encryption is offered. */
export function encryptionUnavailableReason(
  backend: string,
  state: AtomicWrites,
  serverReason?: string | null,
): string | null {
  if (state.kind !== 'missing') return null;
  if (serverReason) return serverReason;
  return state.verdict === 'non-cas'
    ? `Encryption with a proxy key is not available on backend ${backend}: it does not support atomic writes (conditional writes), which encryption needs — see ${CAPABILITY_DOC_URL}`
    : `Encryption with a proxy key is not available on backend ${backend} now: the proxy could not verify that it supports atomic writes, which encryption needs. Check again when the backend is reachable — see ${CAPABILITY_DOC_URL}`;
}

/** Pure: the short reason of a disabled option or button that needs
 *  atomic writes (shown under the option and as its accessible name). */
export function needsAtomicWrites(backend: string, state: AtomicWrites): string | null {
  if (state.kind !== 'missing') return null;
  return state.verdict === 'non-cas'
    ? `Needs atomic writes, which ${backend} does not support.`
    : `Needs atomic writes, which the proxy could not verify on ${backend}.`;
}

/** Pure: why Resume of a conversion is not offered on `backend` (a
 *  conversion keeps writing, so the server refuses it without proven
 *  atomic writes), or null. */
export function resumeBlockedReason(backend: string | undefined, capability: BackendCapabilityVerdict | undefined): string | null {
  const state = atomicWrites(capability);
  if (state.kind !== 'missing') return null;
  const where = backend ?? 'its backend';
  return state.verdict === 'non-cas'
    ? `Resume needs atomic writes, which ${where} does not support. Move the bucket to a backend with them with a migrate job.`
    : `Resume needs atomic writes, and the proxy could not verify them on ${where}. Check again when the backend is reachable.`;
}

/** The note shown when the backend has no verdict yet. */
export const UNCHECKED_NOTE =
  'Before the proxy encrypts anything, it checks that the backend supports atomic writes. If it does not, nothing changes and you see why.';

/** The short header pill of a read-only bucket (the reason is its title
 *  and the visible text of the open card). */
export const READ_ONLY_PILL = 'Read-only';

/** Pure: the read-only badge text of an encrypted bucket on `backend`. */
export function readOnlyBadge(backend: string): string {
  return `Read-only: ${backend} does not support atomic writes`;
}

/** The backend card's line while the first check runs. */
export const CHECKING_TITLE = 'Checking atomic writes…';
