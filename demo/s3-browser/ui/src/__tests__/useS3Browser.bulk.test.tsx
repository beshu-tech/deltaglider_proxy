/**
 * Object-browser selection + bulk actions (useS3Browser). Replaces the old
 * source-grep guard (BULK-COPY-CLEAR-SELECTION) with the behaviour: a
 * successful copy / move / delete clears the selection; a failed one keeps
 * it so the user can retry; folders are expanded before anything mutates.
 *
 * S3 listing goes through the AWS SDK, so the s3client data calls are
 * stubbed at that boundary; the admin bulk API is mocked at fetch.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import type { S3Object } from '../types';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery, renderWithQuery } from '../test/render';

const listing = vi.hoisted(() => ({
  objects: [] as { key: string; size: number; lastModified: string }[],
  folders: [] as string[],
  /** How many times the browser listed the folder (each refresh lists it again). */
  loads: 0,
}));

vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  headObject: async () => ({ storageType: 'passthrough', storedSize: 1 }),
  listBuckets: async () => [{ name: 'releases' }],
  listObjects: async () => {
    listing.loads++;
    return { objects: listing.objects, folders: listing.folders, isTruncated: false };
  },
}));

import useS3Browser from '../useS3Browser';
import BulkActionBar from '../components/BulkActionBar';

const COPY = '/_/api/admin/objects/copy';
const MOVE = '/_/api/admin/objects/move';
const DELETE_JOB = '/_/api/admin/objects/delete-jobs';
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
    let outcome: unknown;
    await act(async () => {
      outcome = await result.current[op]('archive', 'old/');
    });
    expect(outcome).toEqual({
      action: op === 'bulkCopy' ? 'copy' : 'move',
      total: 3,
      succeeded: 3,
      failed: 0,
      deleted: op === 'bulkMove' ? 3 : 0,
      failures: [],
      cancelled: false,
    });
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


// A delete counts the selection in the browser, asks with the exact number,
// then starts ONE server job with the selected folders and objects. The job
// deletes page by page on the server: closing the tab does not stop it, and
// several run at once (src/maintenance/delete.rs).
describe('bulkDelete', () => {
  test('starts a delete job with the folders and the objects, then clears the selection', async () => {
    http.on('POST', DELETE_JOB, json({ job_id: 3 }, 202));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    const outcome = await act(() => result.current.bulkDelete());
    expect(http.callsTo('POST', DELETE_JOB).map((c) => c.body)).toEqual([
      { bucket: 'releases', prefixes: ['builds/nightly/'], keys: ['builds/app-1.0.zip'], expected: 3 },
    ]);
    expect(outcome).toEqual({ total: 3, cancelled: false, failed: 0, jobId: 3 });
    expect(result.current.selectedKeys.size).toBe(0);
    expect(result.current.bulkRunning).toBe(false);
  });

  test('the confirmation gets the counted objects; declining starts nothing', async () => {
    http.on('POST', DELETE_JOB, json({ job_id: 3 }, 202));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    const confirm = vi.fn(async () => false);
    const outcome = await act(() => result.current.bulkDelete(confirm));
    expect(confirm).toHaveBeenCalledWith({ objects: 3, folders: 1, files: 1 });
    expect(outcome).toMatchObject({ cancelled: true });
    expect(http.callsTo('POST', DELETE_JOB)).toHaveLength(0);
    expect(result.current.selectedKeys.size).toBe(2);
  });

  test('a folder of more than one listing is counted page by page; the job gets the folder', async () => {
    const keys = Array.from({ length: 10_001 }, (_, i) => `builds/nightly/${String(i).padStart(5, '0')}`);
    http.on('GET', LIST, (req) => {
      const after = new URL(req.path, 'http://x').searchParams.get('start_after');
      const page = after === null ? keys.slice(0, 10_000) : keys.slice(keys.indexOf(after) + 1);
      const truncated = after === null;
      return json({ keys: page, truncated, ...(truncated ? { next_start_after: page[page.length - 1] } : {}) });
    });
    http.on('POST', DELETE_JOB, json({ job_id: 9 }, 202));
    const { result } = await mountBrowser();
    act(() => result.current.toggleKey('folder:builds/nightly/'));
    const confirm = vi.fn(async () => true);
    const outcome = await act(() => result.current.bulkDelete(confirm));
    expect(confirm).toHaveBeenCalledWith({ objects: 10_001, folders: 1, files: 0 });
    expect(http.callsTo('POST', DELETE_JOB)[0].body).toEqual({
      bucket: 'releases',
      prefixes: ['builds/nightly/'],
      keys: [],
      expected: 10_001,
    });
    expect(outcome).toMatchObject({ total: 10_001, jobId: 9 });
  }, 30_000);

  test('a refused start keeps the selection and rejects with the error', async () => {
    http.on('POST', DELETE_JOB, json({ error: 'AccessDenied: dana may not list releases/builds/nightly/' }, 403));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    await act(async () => {
      await expect(result.current.bulkDelete()).rejects.toThrow(/AccessDenied/);
    });
    expect(result.current.selectedKeys.size).toBe(2);
    expect(result.current.bulkRunning).toBe(false);
  });

  test('a truncated folder that cannot be paged aborts before the job starts', async () => {
    http.on('GET', LIST, json({ keys: ['builds/nightly/a.zip'], truncated: true }));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    await act(async () => {
      await expect(result.current.bulkDelete()).rejects.toThrow(/narrow the selection\. Nothing was deleted\.$/);
    });
    expect(http.callsTo('POST', DELETE_JOB)).toHaveLength(0);
    expect(result.current.selectedKeys.size).toBe(2);
  });

  test('wired to the action bar: count, confirm, start, and the message says it runs on the server', async () => {
    http.on('POST', DELETE_JOB, json({ job_id: 5 }, 202));
    function Harness() {
      const s3 = useS3Browser({ bucket: 'releases', prefix: 'builds/', q: '', object: '', navigateUrl: () => {} });
      return (
        <>
          <button onClick={() => s3.toggleKey('folder:builds/nightly/')}>pick</button>
          {(s3.selectedKeys.size > 0 || s3.bulkRunning) && (
            <BulkActionBar
              selectedCount={s3.selectedKeys.size}
              onDelete={s3.bulkDelete}
              progress={s3.bulkProgress}
              onCancel={s3.cancelBulk}
            />
          )}
        </>
      );
    }
    const user = userEvent.setup();
    renderWithQuery(<Harness />);
    await user.click(await screen.findByRole('button', { name: 'pick' }));
    await user.click(screen.getByRole('button', { name: 'Delete 1 selected object' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getAllByText('Delete 2 objects permanently?').length).toBeGreaterThan(0);
    await user.click(within(dialog).getByRole('button', { name: 'Delete' }));
    expect(
      await screen.findByText('Deleting 2 objects on the server. You can close this tab: the bucket shows the progress.'),
    ).toBeInTheDocument();
    expect(http.callsTo('POST', DELETE_JOB)).toHaveLength(1);
  });
});
