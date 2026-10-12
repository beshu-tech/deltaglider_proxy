/**
 * A single delete from the inspector drops the row and decrements the
 * bucket pill locally (P8.6): no new listing. The 60 s refresh reconciles.
 */
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';
import { qk } from '../queries/keys';

const s3 = vi.hoisted(() => ({ lists: 0 }));
vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  listObjects: async () => {
    s3.lists++;
    return {
      objects: ['a.zip', 'b.zip', 'c.zip'].map((k) => ({
        key: `builds/${k}`, size: 100, lastModified: '', etag: '', storageType: 'delta', storedSize: 10, headers: {},
      })),
      folders: [],
      isTruncated: false,
    };
  },
}));

import useS3Browser from '../useS3Browser';

beforeEach(() => {
  s3.lists = 0;
  mockFetch().on('GET', '/_/api/admin/deltaspace/savings', json({ totals: {}, savings_percentage: null }));
  Object.defineProperty(document, 'hidden', { configurable: true, value: false });
});
afterEach(() => vi.unstubAllGlobals());

test('the row goes and the pill counts one object less, with no new listing', async () => {
  const view = renderHookWithQuery(() =>
    useS3Browser({ bucket: 'releases', prefix: 'builds/', q: '', object: '', navigateUrl: () => {}, adminSession: true }),
  );
  await waitFor(() => expect(view.result.current.objects).toHaveLength(3));
  const usage = { bucket: 'releases', object_count: 3, logical_bytes: 300, stored_bytes: 130, savings_percentage: 50, last_scan_at: null, never_scanned: false };
  view.client.setQueryData(qk.bucketUsage('releases'), usage);

  act(() => view.result.current.removeObject('builds/b.zip'));

  expect(view.result.current.objects.map((o) => o.key)).toEqual(['builds/a.zip', 'builds/c.zip']);
  expect(view.result.current.headCache['builds/b.zip']).toBeUndefined();
  expect(view.client.getQueryData(qk.bucketUsage('releases'))).toMatchObject({
    object_count: 2, logical_bytes: 200, stored_bytes: 120,
  });
  await act(async () => {
    await new Promise((r) => setTimeout(r, 50));
  });
  expect(s3.lists).toBe(1);
});
