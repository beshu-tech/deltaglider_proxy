/**
 * The Backends page shows live health: the list refetches on its own, so a
 * backend that the server marks unhealthy turns red without a reload.
 */
import { waitFor } from '@testing-library/react';
import { afterEach, describe, expect, test, vi } from 'vitest';
import { renderHookWithQuery } from '../test/render';

const getBackends = vi.fn();
vi.mock('../adminApi', () => ({
  getBackends: () => getBackends(),
  getBucketOrigins: async () => ({ buckets: [] }),
}));

import { BACKENDS_REFRESH_MS, useBackends } from '../queries/backends';

afterEach(() => vi.useRealTimers());

describe('useBackends', () => {
  test('refetches every BACKENDS_REFRESH_MS', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getBackends
      .mockResolvedValueOnce({ backends: [{ name: 'hetzner-fsn1', health: { status: 'healthy' } }] })
      .mockResolvedValue({ backends: [{ name: 'hetzner-fsn1', health: { status: 'unreachable' } }] });
    const { result } = renderHookWithQuery(() => useBackends());
    await waitFor(() => expect(result.current.data).toBeDefined());
    expect(getBackends).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(BACKENDS_REFRESH_MS + 10);
    await waitFor(() =>
      expect(result.current.data?.backends[0].health?.status).toBe('unreachable'),
    );
  });
});
