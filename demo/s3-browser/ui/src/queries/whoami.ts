/**
 * Whoami query — the caller's identity as the proxy sees it (auth mode,
 * user, and the running binary's version, which the server reports only to
 * a live session). Only `useWhoami` is exported; see queries/config.ts for
 * the one-hook-per-call-site convention.
 */
import { useQuery } from '@tanstack/react-query';
import { whoami, type WhoamiResponse } from '../adminApi';
import { qk } from './keys';

export function useWhoami() {
  return useQuery<WhoamiResponse>({
    queryKey: qk.whoami(),
    queryFn: whoami,
  });
}
