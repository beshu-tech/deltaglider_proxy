// === Product docs (session-gated, embedded in the binary) ===
import { adminJson } from './core';
import type { DocsPayload } from '../docsBundle';

/** `GET /_/api/docs`: every product doc plus the manifest, one payload. */
export function getDocs(): Promise<DocsPayload> {
  return adminJson<DocsPayload>('/api/docs', { context: 'Docs' });
}
