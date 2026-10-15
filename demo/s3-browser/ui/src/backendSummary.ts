import type { BackendInfo } from './adminApi';

/** One labelled fact of a backend card. `mono` values are identifiers. */
export interface BackendFact {
  label: string;
  value: string;
  mono: boolean;
}

type BackendFields = Pick<BackendInfo, 'backend_type' | 'path' | 'endpoint' | 'region' | 'access_key_id'>;

/** The kind of backend, for the line under its name. */
export function backendKind(b: Pick<BackendInfo, 'backend_type' | 'endpoint'>): string {
  if (b.backend_type === 'filesystem') return 'Local filesystem';
  return b.endpoint ? 'S3-compatible' : 'Amazon S3';
}

/** Where a backend stores its data, one fact per row of the card. The
 *  access key id is an identifier (the server never returns the secret),
 *  shown so the operator sees which key a backend uses. */
export function backendFacts(b: BackendFields): BackendFact[] {
  if (b.backend_type === 'filesystem') {
    return [{ label: 'Path', value: b.path || '(not set)', mono: true }];
  }
  const facts: BackendFact[] = [
    { label: 'Endpoint', value: b.endpoint || 'AWS default', mono: !!b.endpoint },
  ];
  if (b.region) facts.push({ label: 'Region', value: b.region, mono: true });
  if (b.access_key_id) facts.push({ label: 'Access key', value: b.access_key_id, mono: true });
  return facts;
}
