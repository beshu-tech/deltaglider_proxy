/**
 * Review of 2026-10-31, finding 5: the bucket list refetches every 3 s
 * only while a conversion job runs. A conversion that waits for Resume
 * changes on a click (which refetches) or at its deadline, so it refetches
 * once at the deadline and not every 3 s for up to 7 days. Several mounted
 * observers share one query and one cadence.
 */
import { waitFor } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import type { ConversionView } from '../adminApi';
import { renderHookWithQuery, testQueryClient } from '../test/render';
import { bucketKeyEff } from '../test/encryption';

const getBucketOrigins = vi.fn();
vi.mock('../adminApi', () => ({
  getBackends: async () => ({ backends: [] }),
  getBucketOrigins: () => getBucketOrigins(),
  getLegacyKeyUsage: async () => ({}),
}));

import { CONVERSION_POLL_MS } from '../conversionView';
import { useBucketOrigins } from '../queries/backends';
import { qk } from '../queries/keys';

afterEach(() => {
  vi.useRealTimers();
  getBucketOrigins.mockReset();
});

function view(over: Partial<ConversionView> = {}): ConversionView {
  return {
    open: true,
    run: 'a'.repeat(32),
    adopted_after_cutoff: 0,
    job_on_this_node: null,
    serves_plaintext_deprecated: false,
    ...over,
  };
}

function list(conversion?: ConversionView) {
  return { buckets: [{ name: 'db-archive', backend_name: 'hetzner-fsn1', effective_encryption: bucketKeyEff({ conversion }) }] };
}

const RUNNING = view({ job_on_this_node: { id: 7, status: 'running', percent: 10 } });

test('a running conversion refetches every 3 s, and stops when it closes', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  getBucketOrigins.mockResolvedValueOnce(list(RUNNING)).mockResolvedValue(list());
  const { result } = renderHookWithQuery(() => useBucketOrigins());
  await waitFor(() => expect(result.current.data).toBeDefined());
  expect(getBucketOrigins).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(CONVERSION_POLL_MS + 10);
  await waitFor(() => expect(result.current.data?.buckets[0].effective_encryption?.conversion).toBeUndefined());
  expect(getBucketOrigins).toHaveBeenCalledTimes(2);
  // Closed: no more refetches.
  await vi.advanceTimersByTimeAsync(CONVERSION_POLL_MS * 5);
  expect(getBucketOrigins).toHaveBeenCalledTimes(2);
});

test('a conversion that waits for Resume does not refetch every 3 s; it refetches once at its deadline', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  const deadline = Math.floor(Date.now() / 1000) + 600;
  getBucketOrigins.mockResolvedValue(list(view({ deadline })));
  const { result } = renderHookWithQuery(() => useBucketOrigins());
  await waitFor(() => expect(result.current.data).toBeDefined());
  await vi.advanceTimersByTimeAsync(CONVERSION_POLL_MS * 10);
  expect(getBucketOrigins).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(600_000);
  await waitFor(() => expect(getBucketOrigins).toHaveBeenCalledTimes(2));
});

test('two observers of the list share one query and one 3 s cadence', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  getBucketOrigins.mockResolvedValue(list(RUNNING));
  const client = testQueryClient();
  const first = renderHookWithQuery(() => useBucketOrigins(), client);
  await waitFor(() => expect(first.result.current.data).toBeDefined());
  // A second observer mounts out of phase with the first one's interval.
  await vi.advanceTimersByTimeAsync(1_000);
  const second = renderHookWithQuery(() => useBucketOrigins(), client);
  await waitFor(() => expect(second.result.current.data).toBeDefined());
  const before = getBucketOrigins.mock.calls.length;
  await vi.advanceTimersByTimeAsync(CONVERSION_POLL_MS * 10);
  // One refetch per period, not one per observer.
  const refetches = getBucketOrigins.mock.calls.length - before;
  expect(refetches).toBeGreaterThanOrEqual(9);
  expect(refetches).toBeLessThanOrEqual(10);
  expect(client.getQueryCache().findAll({ queryKey: qk.backends.origins(), exact: true })).toHaveLength(1);
});
