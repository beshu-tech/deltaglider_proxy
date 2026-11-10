/**
 * UX harvest C17: the header's "Bucket total … N objects" stayed stale after
 * a bulk copy into another bucket. The end of every bulk run reloads the
 * totals of each bucket it wrote to.
 */
import { act } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { qk } from '../queries/keys';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  listDirectKeys: async () => ({ keys: [], truncated: false }),
}));

import { useBulkActions } from '../browser/useBulkActions';
import { objectRowKey } from '../rowKey';

afterEach(() => vi.unstubAllGlobals());

test('a copy reloads the totals of the source and the destination bucket', async () => {
  const http = mockFetch();
  // The destination holds nothing yet: the run checks it before it writes.
  http.on('GET', '/_/api/admin/objects/list', json({ keys: [], truncated: false }));
  http.on('POST', '/_/api/admin/objects/copy', json({ succeeded: 1, failed: 0, failures: [], deleted: 0 }));
  const { result, client } = renderHookWithQuery(() =>
    useBulkActions({ selectedKeys: new Set([objectRowKey('builds/app.zip')]), clearSelection: () => {}, refresh: () => {}, mutate: () => {} }),
  );
  const invalidate = vi.spyOn(client, 'invalidateQueries');
  await act(async () => {
    await result.current.bulkCopy('archive', 'old/');
  });
  const keys = invalidate.mock.calls.map(([f]) => JSON.stringify(f?.queryKey));
  expect(keys).toContain(JSON.stringify(qk.bucketUsage('releases')));
  expect(keys).toContain(JSON.stringify(qk.bucketUsage('archive')));
});
