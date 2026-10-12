/**
 * The 60 s auto-refresh lists the first page only and keeps the later
 * pages already loaded (P8.7); a manual Refresh (mutate) lists them all.
 */
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';

const s3 = vi.hoisted(() => ({
  /** maxPages of each listing (undefined = all). */
  calls: [] as (number | undefined)[],
  /** The folder: page 1 (`a-*`) then page 2 (`z-*`). */
  page1: [] as string[],
  page2: [] as string[],
}));
const row = (key: string) => ({ key, size: 1, lastModified: '', etag: '', headers: {} });
vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  listObjects: async (_prefix: string, maxPages?: number) => {
    s3.calls.push(maxPages);
    if (maxPages === 1) {
      return { objects: s3.page1.map(row), folders: [], isTruncated: false, continuesAfter: s3.page1[s3.page1.length - 1], pages: 1 };
    }
    return { objects: [...s3.page1, ...s3.page2].map(row), folders: [], isTruncated: false, pages: s3.page2.length ? 2 : 1 };
  },
}));

import useS3Browser from '../useS3Browser';

beforeEach(() => {
  s3.calls = [];
  s3.page1 = ['b/a-1', 'b/a-2'];
  s3.page2 = ['b/z-1', 'b/z-2'];
  mockFetch().on('GET', '/_/api/admin/deltaspace/savings', json({ totals: {}, savings_percentage: null }));
  Object.defineProperty(document, 'hidden', { configurable: true, value: false });
  vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'], shouldAdvanceTime: true });
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const keys = (r: { current: { objects: { key: string }[] } }) => r.current.objects.map((o) => o.key);

test('the tick lists page 1 and keeps page 2; Refresh lists all', async () => {
  const { result } = renderHookWithQuery(() =>
    useS3Browser({ bucket: 'releases', prefix: 'b/', q: '', object: '', navigateUrl: () => {}, adminSession: true }),
  );
  await waitFor(() => expect(keys(result)).toHaveLength(4));
  expect(s3.calls).toEqual([undefined]);
  expect(result.current.restListedAt).not.toBeNull();

  // Elsewhere: a-2 deleted, a-3 added (page 1), z-2 deleted (page 2).
  s3.page1 = ['b/a-1', 'b/a-3'];
  s3.page2 = ['b/z-1'];
  await act(async () => {
    await vi.advanceTimersByTimeAsync(60_000);
  });
  await waitFor(() => expect(keys(result)).toEqual(['b/a-1', 'b/a-3', 'b/z-1', 'b/z-2']));
  expect(s3.calls).toEqual([undefined, 1]);

  act(() => result.current.mutate());
  await waitFor(() => expect(keys(result)).toEqual(['b/a-1', 'b/a-3', 'b/z-1']));
  expect(s3.calls).toEqual([undefined, 1, undefined]);
});

test('a folder that fits one page: the tick replaces it whole', async () => {
  s3.page2 = [];
  const { result } = renderHookWithQuery(() =>
    useS3Browser({ bucket: 'releases', prefix: 'b/', q: '', object: '', navigateUrl: () => {}, adminSession: true }),
  );
  await waitFor(() => expect(keys(result)).toHaveLength(2));
  expect(result.current.restListedAt).toBeNull();
});
