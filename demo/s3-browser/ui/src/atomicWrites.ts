/**
 * Encryption needs atomic writes (N2 of
 * docs/plan/encryption-needs-atomic-writes.md).
 *
 * The proxy converts and re-keys an encrypted bucket by rewriting objects
 * with conditional writes. A backend that does not enforce them (Backblaze
 * B2) cannot encrypt with the proxy: the server refuses the change, and an
 * encrypted bucket already on such a backend is read-only. The UI reads the
 * server's capability verdict of the backend (GET /backends `capability`)
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
  /** No verdict yet: the apply checks. */
  | { kind: 'unchecked' }
  /** Not proven: the probe said no, or could not verify. */
  | { kind: 'missing'; words: string };

/** Pure: the probe result in plain words (mirrors the server's
 *  `atomicity::verdict_words`). */
function capabilityWords(cap: BackendCapabilityVerdict): string {
  switch (cap.verdict) {
    case 'cas-verified':
      return 'the backend enforces conditional writes';
    case 'non-cas':
      return 'Backblaze-like: the backend ignored or rejected a conditional write';
    case 'unknown':
      return `could not reach the backend to verify (${cap.reason})`;
  }
}

/** Pure: the atomic-writes state of a backend from its verdict. */
export function atomicWrites(cap: BackendCapabilityVerdict | undefined): AtomicWrites {
  if (!cap) return { kind: 'unchecked' };
  if (cap.verdict === 'cas-verified') return { kind: 'atomic' };
  return { kind: 'missing', words: capabilityWords(cap) };
}

/** Pure: why proxy encryption is not offered on `backend` (the visible
 *  reason, with the doc link as a URL that LinkifiedText turns into a
 *  link), or null when it is. */
export function encryptionUnavailableReason(backend: string, state: AtomicWrites): string | null {
  if (state.kind !== 'missing') return null;
  return `Encryption needs atomic writes. ${backend} does not provide them: ${state.words}. See ${CAPABILITY_DOC_URL}`;
}

/** The note shown when the backend has no verdict yet. */
export const UNCHECKED_NOTE = 'The apply checks that the backend has atomic writes (conditional writes) before it encrypts.';

/** Pure: the read-only badge text of an encrypted bucket on `backend`. */
export function readOnlyBadge(backend: string): string {
  return `Read-only: ${backend} does not support atomic writes`;
}
