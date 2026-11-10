/**
 * F18 (epic #146): the checked rows are always rows the user can see. After the
 * filter box hides a checked row, the toolbar count, the bulk actions and
 * "select all" no longer count it.
 */
import { act, waitFor } from '@testing-library/react';
import { beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';
import { folderRowKey, objectRowKey, rowKeyOf, parseRowKey } from '../rowKey';

const listing = vi.hoisted(() => ({
  objects: [] as { key: string; size: number; lastModified: string }[],
  folders: [] as string[],
}));

vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  headObject: async () => ({ storageType: 'passthrough', storedSize: 1 }),
  listBuckets: async () => [{ name: 'releases' }],
  listObjects: async () => ({ objects: listing.objects, folders: listing.folders, isTruncated: false }),
}));

import useS3Browser from '../useS3Browser';

beforeEach(() => {
  mockFetch().on('GET', '/_/api/admin/objects/list', json({ keys: [], truncated: false }));
  listing.objects = [
    { key: 'builds/app-1.0.zip', size: 10, lastModified: '2026-01-01T00:00:00Z' },
    { key: 'builds/app-1.1.zip', size: 10, lastModified: '2026-01-02T00:00:00Z' },
    { key: 'builds/notes.txt', size: 1, lastModified: '2026-01-02T00:00:00Z' },
  ];
  listing.folders = ['builds/nightly/', 'builds/app-old/'];
});

async function mount() {
  const view = renderHookWithQuery(() =>
    useS3Browser({ bucket: 'releases', prefix: 'builds/', q: '', object: '', navigateUrl: () => {} }),
  );
  await waitFor(() => expect(view.result.current.loading).toBe(false));
  return view;
}

/** The rows the table shows now, as row keys. */
function visibleRowKeys(b: ReturnType<typeof useS3Browser>) {
  return new Set([...b.folders.map(folderRowKey), ...b.objects.map((o) => objectRowKey(o.key))]);
}

test('the checked rows are a subset of the visible rows after a filter', async () => {
  const { result } = await mount();
  act(() => result.current.toggleAll());
  expect(result.current.selectedKeys.size).toBe(5);
  act(() => result.current.setSearchQuery('app-1.0'));
  await waitFor(() => expect(result.current.objects.map((o) => o.key)).toEqual(['builds/app-1.0.zip']));
  const visible = visibleRowKeys(result.current);
  await waitFor(() => {
    for (const k of result.current.selectedKeys) expect(visible.has(k)).toBe(true);
  });
  expect([...result.current.selectedKeys].map((k) => parseRowKey(k))).toEqual([
    { kind: 'object', key: 'builds/app-1.0.zip' },
  ]);
});

test('a hidden folder is not counted, and "select all" checks the visible rows only', async () => {
  const { result } = await mount();
  act(() => result.current.toggleKey(folderRowKey('builds/nightly/')));
  act(() => result.current.toggleKey(objectRowKey('builds/notes.txt')));
  act(() => result.current.setSearchQuery('notes'));
  await waitFor(() => expect(result.current.selectedKeys.size).toBe(1));
  expect(result.current.selectedKeys.has(rowKeyOf({ kind: 'object', key: 'builds/notes.txt' }))).toBe(true);
  // The one visible row is checked: "select all" now unchecks it.
  act(() => result.current.toggleAll());
  expect(result.current.selectedKeys.size).toBe(0);
  act(() => result.current.toggleAll());
  expect([...result.current.selectedKeys]).toEqual([objectRowKey('builds/notes.txt')]);
});
