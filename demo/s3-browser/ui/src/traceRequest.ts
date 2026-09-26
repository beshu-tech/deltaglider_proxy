/**
 * Pure payload builder for the admission-trace diagnostic (TracePanel).
 *
 * React-free so it can be unit-tested in Node (see
 * src/__tests__/traceRequest.test.ts). The wire contract is
 * load-bearing: `query` / `source_ip` are only emitted when non-empty
 * after trimming, matching what `POST /_/api/admin/config/trace`
 * expects. Keep this byte-identical to the prior inline builder.
 */

export interface TraceRequestInput {
  method: string;
  path: string;
  query: string;
  sourceIp: string;
  authenticated: boolean;
}

export interface TraceRequestBody {
  method: string;
  path: string;
  authenticated: boolean;
  query?: string;
  source_ip?: string;
}

export function buildTraceBody(input: TraceRequestInput): TraceRequestBody {
  const body: TraceRequestBody = {
    method: input.method,
    path: input.path,
    authenticated: input.authenticated,
  };
  if (input.query.trim()) body.query = input.query.trim();
  if (input.sourceIp.trim()) body.source_ip = input.sourceIp.trim();
  return body;
}

/** `anonymous_grant` of the trace response (serde tag `action`). */
export type AnonymousGrant =
  | { action: 'read'; bucket: string; key: string }
  | { action: 'list'; bucket: string; prefix: string }
  | { action: 'public-prefixes'; bucket: string };

/**
 * One sentence on what an `allow-anonymous` decision lets the anonymous
 * caller do. `null` grant = the request is a write (never granted), so it
 * goes on without credentials and authorization refuses it.
 */
export function describeAnonymousGrant(grant: AnonymousGrant | null | undefined): string {
  if (!grant) {
    return 'The rule grants nothing to an anonymous caller for this request. Only reads (GET, HEAD, list) are granted, so this request continues without credentials and is refused with 403 AccessDenied.';
  }
  switch (grant.action) {
    case 'read':
      return `An anonymous caller may read the object ${grant.bucket}/${grant.key} (GET and HEAD), and nothing else.`;
    case 'list':
      return grant.prefix
        ? `An anonymous caller may list the bucket ${grant.bucket} with the prefix ${grant.prefix}, and nothing else.`
        : `An anonymous caller may list the bucket ${grant.bucket} without a prefix, and nothing else.`;
    case 'public-prefixes':
      return `An anonymous caller gets the public prefixes of the bucket ${grant.bucket}: reads of the keys under them, and listings scoped to them.`;
  }
}
