/**
 * UX harvest D16 (browse): after a bucket switch the header kept the previous
 * bucket's "26% smaller · 7.6 MB saved" pill under the new bucket's name
 * while the new summary loaded. A summary is shown only for its own folder.
 */
import { renderHook, waitFor } from '@testing-library/react';
import { expect, test, vi } from 'vitest';

const getPrefixSavings = vi.hoisted(() => vi.fn());
vi.mock('../adminApi', () => ({ getPrefixSavings }));

import { useDeltaSummary } from '../browser/useDeltaSummary';

const response = (bucket: string) => ({
  bucket,
  prefix: '',
  totals: { delta_count: 3, passthrough_count: 1, reference_count: 1, original_bytes: 1000, stored_bytes: 740, reference_bytes: 100 },
  savings_percentage: 26,
  truncated: false,
  estimated: false,
  computed_at: '',
});

test('a bucket switch never shows the previous bucket summary', async () => {
  getPrefixSavings.mockImplementation(async (bucket: string) => (bucket === 'releases' ? response('releases') : new Promise(() => {})));
  const { result, rerender } = renderHook(
    ({ bucket }) => useDeltaSummary({ bucket, prefix: '', active: true, connected: true, adminSession: true, trigger: 0 }),
    { initialProps: { bucket: 'releases' } },
  );
  await waitFor(() => expect(result.current?.savingsPct).toBe(26));
  rerender({ bucket: 'media' });
  expect(result.current).toBeNull();
});
