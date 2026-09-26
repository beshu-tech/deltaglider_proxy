/**
 * Object-browser selection + bulk actions (useS3Browser). Replaces the old
 * source-grep guard (BULK-COPY-CLEAR-SELECTION) with the behaviour: a
 * successful copy / move / delete clears the selection; a failed one keeps
 * it so the user can retry; folders are expanded before anything mutates.
 *
 * S3 listing goes through the AWS SDK, so the s3client data calls are
 * stubbed at that boundary; the admin bulk API is mocked at fetch.
 */
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import type { S3Object } from '../types';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';

const listing = vi.hoisted(() => ({
  objects: [] as { key: string; size: number; lastModified: string }[],
  folders: [] as string[],
}));

vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  headObject: async () => ({ storageType: 'passthrough', storedSize: 1 }),
  listObjects: async () => ({ objects: listing.objects, folders: listing.folders, isTruncated: false }),
}));

import useS3Browser from '../useS3Browser';

const COPY = '/_/api/admin/objects/copy';
const MOVE = '/_/api/admin/objects/move';
const DELETE = '/_/api/admin/objects/delete';
const LIST = '/_/api/admin/objects/list';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  listing.objects = [
    { key: 'builds/app-1.0.zip', size: 10, lastModified: '2026-01-01T00:00:00Z' },
    { key: 'builds/app-1.1.zip', size: 10, lastModified: '2026-01-02T00:00:00Z' },
  ];
  listing.folders = ['builds/nightly/'];
  http.on('GET', LIST, json({ keys: ['builds/nightly/a.zip', 'builds/nightly/b.zip'], truncated: false }));
});
afterEach(() => {
  vi.unstubAllGlobals();
});

async function mountBrowser() {
  const view = renderHookWithQuery(() =>
    useS3Browser({ bucket: 'releases', prefix: 'builds/', q: '', object: '', navigateUrl: () => {} }),
  );
  await waitFor(() => expect(view.result.current.loading).toBe(false));
  expect(view.result.current.objects.map((o: S3Object) => o.key)).toEqual(['builds/app-1.0.zip', 'builds/app-1.1.zip']);
  return view;
}

function selectFileAndFolder(result: { current: ReturnType<typeof useS3Browser> }) {
  act(() => result.current.toggleKey('builds/app-1.0.zip'));
  act(() => result.current.toggleKey('folder:builds/nightly/'));
  expect(result.current.selectedKeys.size).toBe(2);
}

describe('selection', () => {
  test('toggleKey toggles; toggleAll selects every visible row, then none', async () => {
    const { result } = await mountBrowser();
    act(() => result.current.toggleKey('builds/app-1.0.zip'));
    act(() => result.current.toggleKey('builds/app-1.0.zip'));
    expect(result.current.selectedKeys.size).toBe(0);
    act(() => result.current.toggleAll());
    expect([...result.current.selectedKeys].sort()).toEqual([
      'builds/app-1.0.zip',
      'builds/app-1.1.zip',
      'folder:builds/nightly/',
    ]);
    act(() => result.current.toggleAll());
    expect(result.current.selectedKeys.size).toBe(0);
  });

  test('a refresh drops selected keys that no longer exist', async () => {
    const { result } = await mountBrowser();
    act(() => result.current.toggleKey('builds/app-1.0.zip'));
    act(() => result.current.toggleKey('builds/app-1.1.zip'));
    listing.objects = listing.objects.filter((o) => o.key !== 'builds/app-1.0.zip');
    act(() => result.current.mutate());
    await waitFor(() => expect([...result.current.selectedKeys]).toEqual(['builds/app-1.1.zip']));
  });
});

describe.each([
  ['bulkCopy', COPY],
  ['bulkMove', MOVE],
] as const)('%s', (op, path) => {
  test('expands folders, sends one request, then clears the selection', async () => {
    http.on('POST', path, json({ succeeded: 3, failed: 0, failures: [], deleted: 3 }));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    let outcome: { succeeded: number; failed: number } | undefined;
    await act(async () => {
      outcome = await result.current[op]('archive', 'old/');
    });
    expect(outcome).toEqual({ succeeded: 3, failed: 0 });
    expect(http.callsTo('POST', path)[0].body).toEqual({
      source_bucket: 'releases',
      dest_bucket: 'archive',
      dest_prefix: 'old/',
      items: [
        { source_key: 'builds/app-1.0.zip', relative: 'app-1.0.zip' },
        // The folder keeps its own name under the destination.
        { source_key: 'builds/nightly/a.zip', relative: 'nightly/a.zip' },
        { source_key: 'builds/nightly/b.zip', relative: 'nightly/b.zip' },
      ],
    });
    expect(result.current.selectedKeys.size).toBe(0);
  });

  test('a failed request keeps the selection for a retry', async () => {
    http.on('POST', path, json({ error: 'backend down' }, 500));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    await act(async () => {
      await expect(result.current[op]('archive', 'old/')).rejects.toThrow(/backend down/);
    });
    expect(result.current.selectedKeys.size).toBe(2);
  });

  test('a truncated folder aborts before any mutation', async () => {
    http.on('GET', LIST, json({ keys: ['builds/nightly/a.zip'], truncated: true }));
    http.on('POST', path, json({ succeeded: 1, failed: 0, failures: [] }));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    await act(async () => {
      await expect(result.current[op]('archive', '')).rejects.toThrow(/narrow the selection/);
    });
    expect(http.callsTo('POST', path)).toHaveLength(0);
    expect(result.current.selectedKeys.size).toBe(2);
  });
});

describe('bulkDelete', () => {
  test('deletes the expanded keys and clears the selection', async () => {
    http.on('POST', DELETE, json({ deleted: 3, failed: 0, failures: [] }));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    await act(() => result.current.bulkDelete());
    expect(http.callsTo('POST', DELETE)[0].body).toEqual({
      bucket: 'releases',
      keys: ['builds/app-1.0.zip', 'builds/nightly/a.zip', 'builds/nightly/b.zip'],
    });
    expect(result.current.selectedKeys.size).toBe(0);
    expect(result.current.deleting).toBe(false);
  });

  test('a failed delete keeps the selection and reports the error', async () => {
    http.on('POST', DELETE, json({ error: 'denied' }, 500));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    await act(() => result.current.bulkDelete());
    expect(result.current.selectedKeys.size).toBe(2);
    expect(result.current.error).toMatch(/denied/);
    expect(result.current.deleting).toBe(false);
  });
});
