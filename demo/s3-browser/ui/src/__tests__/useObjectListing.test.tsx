// useObjectListing: lists the folder, feeds the selection, ignores stale
// responses, and lists nothing off the browser view.
import { act, waitFor } from '@testing-library/react';
import { beforeEach, expect, test, vi } from 'vitest';
import { renderHookWithQuery } from '../test/render';
import type { S3Object } from '../types';

type Listing = { objects: S3Object[]; folders: string[]; isTruncated: boolean };
const s3 = vi.hoisted(() => ({
  calls: [] as string[],
  /** Per prefix: the pending listing's resolver. */
  pending: new Map<string, (l: Listing) => void>(),
}));
vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  listObjects: (prefix: string) => {
    s3.calls.push(prefix);
    return new Promise<Listing>((resolve) => s3.pending.set(prefix, resolve));
  },
}));

import { useObjectListing } from '../browser/useObjectListing';

const obj = (key: string) => ({ key, size: 5, lastModified: '' }) as S3Object;
const NO_PREFIXES: string[] = [];

function mount(initial: { prefix: string; active?: boolean; writablePrefixes?: string[] }) {
  let props = initial;
  const reconcile = vi.fn();
  const clearSelection = vi.fn();
  const view = renderHookWithQuery(() =>
    useObjectListing({
      bucket: 'releases',
      prefix: props.prefix,
      active: props.active ?? true,
      writablePrefixes: props.writablePrefixes ?? NO_PREFIXES,
      reconcile,
      clearSelection,
    }),
  );
  return {
    ...view,
    reconcile,
    clearSelection,
    setProps(next: typeof initial) {
      props = next;
      view.rerender();
    },
  };
}

async function answer(prefix: string, listing: Listing) {
  await act(async () => {
    s3.pending.get(prefix)?.(listing);
  });
}

beforeEach(() => {
  s3.calls = [];
  s3.pending.clear();
});

test('lists the folder, then hands the rows to the selection', async () => {
  const { result, reconcile } = mount({ prefix: 'builds/' });
  expect(result.current.loading).toBe(true);
  await answer('builds/', { objects: [obj('builds/a.zip')], folders: ['builds/x/'], isTruncated: false });
  await waitFor(() => expect(result.current.loading).toBe(false));
  expect(result.current.objects.map((o) => o.key)).toEqual(['builds/a.zip']);
  expect(result.current.folders).toEqual(['builds/x/']);
  expect(result.current.headCache['builds/a.zip']).toEqual({ storageType: undefined, storedSize: undefined });
  expect(reconcile).toHaveBeenCalledWith([obj('builds/a.zip')], ['builds/x/']);
});

test('a writable prefix below the folder shows as a virtual folder', async () => {
  const { result } = mount({ prefix: '', writablePrefixes: ['ci/nightly/'] });
  await answer('', { objects: [], folders: [], isTruncated: false });
  await waitFor(() => expect(result.current.virtualFolders).toEqual(['ci/']));
  expect(result.current.folders).toEqual(['ci/']);
});

test('a folder change clears the selection, and the older answer is dropped', async () => {
  const { result, clearSelection, setProps } = mount({ prefix: 'builds/' });
  const clearsBefore = clearSelection.mock.calls.length;
  setProps({ prefix: 'nightly/' });
  expect(clearSelection.mock.calls.length).toBeGreaterThan(clearsBefore);
  await answer('nightly/', { objects: [obj('nightly/n.zip')], folders: [], isTruncated: false });
  // The first folder's listing settles late: it must not replace the rows.
  await answer('builds/', { objects: [obj('builds/a.zip')], folders: [], isTruncated: false });
  await waitFor(() => expect(result.current.loading).toBe(false));
  expect(result.current.objects.map((o) => o.key)).toEqual(['nightly/n.zip']);
});

test('off the browser view it lists nothing', () => {
  const { result } = mount({ prefix: 'builds/', active: false });
  expect(s3.calls).toEqual([]);
  expect(result.current.loading).toBe(false);
});

test('removeObject drops the row without a new listing', async () => {
  const { result } = mount({ prefix: 'builds/' });
  await answer('builds/', { objects: [obj('builds/a.zip'), obj('builds/b.zip')], folders: [], isTruncated: false });
  await waitFor(() => expect(result.current.objects).toHaveLength(2));
  const lists = s3.calls.length;
  act(() => result.current.removeObject('builds/a.zip'));
  expect(result.current.objects.map((o) => o.key)).toEqual(['builds/b.zip']);
  expect(s3.calls.length).toBe(lists);
});
