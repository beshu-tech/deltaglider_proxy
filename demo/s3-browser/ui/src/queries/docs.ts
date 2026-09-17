/**
 * Product docs query — fetched once per session from `GET /_/api/docs`
 * (session-gated; the markdown is embedded in the proxy binary, not in this
 * bundle — see docsBundle.ts for why). Only `useDocs` is exported.
 */
import { useQuery } from '@tanstack/react-query';
import { adminFetch, safeJson } from '../adminApi/core';
import { buildDocsBundle, type DocsBundle, type DocsPayload } from '../docsBundle';
import { qk } from './keys';

async function fetchDocs(): Promise<DocsBundle> {
  const res = await adminFetch('/api/docs');
  if (!res.ok) throw new Error(`docs: HTTP ${res.status}`);
  return buildDocsBundle((await safeJson(res)) as DocsPayload);
}

export function useDocs() {
  return useQuery<DocsBundle>({
    queryKey: qk.docs(),
    queryFn: fetchDocs,
    // Docs change only with a proxy upgrade; never refetch on focus.
    staleTime: Infinity,
  });
}
