/**
 * Storage-backend list read query.
 *
 * BackendsPanel keeps its create/delete/encryption mutations inline (they're
 * compound section PUTs with bespoke result messaging), but the READ path moves
 * here so the list is keyed by `qk.backends.list()` and shared with anything
 * else that reads backends. Mutations invalidate this key + `qk.config()` after
 * a write so the list and the cached config both refresh.
 */
import { useQuery } from '@tanstack/react-query';
import { getBackends } from '../adminApi';
import { qk } from './keys';

export function useBackends() {
  return useQuery({
    queryKey: qk.backends.list(),
    queryFn: getBackends,
  });
}
