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
