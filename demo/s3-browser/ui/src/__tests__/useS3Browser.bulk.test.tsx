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
import type { BulkDeleteOutcome } from '../bulkDelete';
import { DELETE_BATCH_SIZE } from '../bulkDelete';
import { isSessionExpired } from '../errorHandling';
import { json, mockFetch, type RecordedRequest } from '../test/fetchMock';
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

  test('a failed delete keeps the selection and rejects with the error and the count', async () => {
    http.on('POST', DELETE, json({ error: 'denied' }, 500));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    await act(async () => {
      await expect(result.current.bulkDelete()).rejects.toThrow(/denied\. 0 of 3 objects were deleted before the failure\.$/);
    });
    expect(result.current.selectedKeys.size).toBe(2);
    expect(result.current.deleting).toBe(false);
  });

  test('a truncated folder aborts before any delete request', async () => {
    http.on('GET', LIST, json({ keys: ['builds/nightly/a.zip'], truncated: true }));
    const { result } = await mountBrowser();
    selectFileAndFolder(result);
    await act(async () => {
      await expect(result.current.bulkDelete()).rejects.toThrow(/narrow the selection\. Nothing was deleted\.$/);
    });
    expect(http.callsTo('POST', DELETE)).toHaveLength(0);
    expect(result.current.selectedKeys.size).toBe(2);
  });
});

// A page of ~50 selected folders went to the server as ONE delete request:
// the page showed nothing for minutes, more than 10,000 keys failed at the
// end, and a closed tab stopped the server loop partway. The delete now runs
// in batches with progress, a per-batch reload and a cancel.
describe('bulkDelete in batches', () => {
  const BIG = Array.from({ length: 1201 }, (_, i) => `builds/nightly/${String(i).padStart(4, '0')}.zip`);

  /** Delete requests that answer only when the test says so. */
  function heldDeletes() {
    const held: { keys: string[]; answer: (res?: Response) => void }[] = [];
    http.on('POST', DELETE, (req: RecordedRequest) => () =>
      new Promise<Response>((resolve) => {
        const keys = (req.body as { keys: string[] }).keys;
        held.push({ keys, answer: (res = json({ deleted: keys.length, failed: 0, failures: [] })) => resolve(res) });
      }),
    );
    return held;
  }

  /** Select the big folder and start the delete; the caller awaits the returned run. */
  function startDelete(result: { current: ReturnType<typeof useS3Browser> }) {
    act(() => result.current.toggleKey('folder:builds/nightly/'));
    let run!: Promise<BulkDeleteOutcome>;
    act(() => {
      run = result.current.bulkDelete();
    });
    run.catch(() => {}); // the test awaits it; keep an early rejection from counting as unhandled
    return run;
  }

  beforeEach(() => {
    http.on('GET', LIST, json({ keys: BIG, truncated: false }));
  });

  test(`sends batches of ${DELETE_BATCH_SIZE}, reports progress and reloads the listing after each batch`, async () => {
    const held = heldDeletes();
    const { result } = await mountBrowser();
    const run = startDelete(result);
    await waitFor(() => expect(held).toHaveLength(1));
    expect(held[0].keys).toEqual(BIG.slice(0, 500));
    expect(result.current.deleteProgress).toEqual({ phase: 'deleting', done: 0, total: 1201, stopping: false });
    const loads = listing.loads;

    held[0].answer();
    await waitFor(() => expect(held).toHaveLength(2));
    expect(held[1].keys).toEqual(BIG.slice(500, 1000));
    await waitFor(() => expect(result.current.deleteProgress).toMatchObject({ done: 500, total: 1201 }));
    // The listing reloads after the first batch, while the second one runs.
    await waitFor(() => expect(listing.loads).toBe(loads + 1));

    held[1].answer();
    await waitFor(() => expect(held).toHaveLength(3));
    expect(held[2].keys).toEqual(BIG.slice(1000));
    await waitFor(() => expect(listing.loads).toBe(loads + 2));

    held[2].answer();
    await expect(run).resolves.toEqual({ total: 1201, deleted: 1201, failed: 0, failures: [], cancelled: false });
    await waitFor(() => expect(result.current.deleting).toBe(false));
    expect(result.current.deleteProgress).toBeNull();
    expect(result.current.selectedKeys.size).toBe(0);
    expect(listing.loads).toBeGreaterThanOrEqual(loads + 3);
  });

  test('cancel lets the batch in flight finish, then sends nothing more and keeps the selection', async () => {
    const held = heldDeletes();
    const { result } = await mountBrowser();
    const run = startDelete(result);
    await waitFor(() => expect(held).toHaveLength(1));
    act(() => result.current.cancelBulkDelete());
    expect(result.current.deleteProgress).toMatchObject({ phase: 'deleting', stopping: true });

    held[0].answer();
    await expect(run).resolves.toEqual({ total: 1201, deleted: 500, failed: 0, failures: [], cancelled: true });
    expect(http.callsTo('POST', DELETE)).toHaveLength(1);
    await waitFor(() => expect(result.current.deleting).toBe(false));
    // The folder still holds 701 keys: it stays selected for a retry.
    expect([...result.current.selectedKeys]).toEqual(['folder:builds/nightly/']);
  });

  test('a failure mid-run says how many keys were deleted before it', async () => {
    const held = heldDeletes();
    const { result } = await mountBrowser();
    const run = startDelete(result);
    await waitFor(() => expect(held).toHaveLength(1));
    held[0].answer();
    await waitFor(() => expect(held).toHaveLength(2));
    held[1].answer(json({ error: 'disk full' }, 500));
    await expect(run).rejects.toThrow(
      'Bulk delete failed (500): disk full. 500 of 1,201 objects were deleted before the failure.',
    );
    expect(http.callsTo('POST', DELETE)).toHaveLength(2);
    await waitFor(() => expect(result.current.deleting).toBe(false));
  });

  test('an expired session mid-run rejects with the session error itself (sign-in path)', async () => {
    const held = heldDeletes();
    const { result } = await mountBrowser();
    const run = startDelete(result);
    await waitFor(() => expect(held).toHaveLength(1));
    held[0].answer(json({ error: 'unauthorized' }, 401));
    const err = await run.then(
      () => null,
      (e: unknown) => e,
    );
    expect(isSessionExpired(err)).toBe(true);
    await waitFor(() => expect(result.current.deleting).toBe(false));
  });

  test('leaving the page asks first while a delete runs', async () => {
    const held = heldDeletes();
    const { result } = await mountBrowser();
    const leave = () => {
      const ev = new Event('beforeunload', { cancelable: true });
      window.dispatchEvent(ev);
      return ev.defaultPrevented;
    };
    expect(leave()).toBe(false);
    const run = startDelete(result);
    await waitFor(() => expect(held).toHaveLength(1));
    expect(leave()).toBe(true);
    act(() => result.current.cancelBulkDelete());
    held[0].answer();
    await run;
    await waitFor(() => expect(result.current.deleting).toBe(false));
    expect(leave()).toBe(false);
  });

  test('wired to the action bar: the text moves between batches, and Cancel stops the run', async () => {
    const held = heldDeletes();
    function Harness() {
      const s3 = useS3Browser({ bucket: 'releases', prefix: 'builds/', q: '', object: '', navigateUrl: () => {} });
      return (
        <>
          <button onClick={() => s3.toggleKey('folder:builds/nightly/')}>pick</button>
          {/* As in App: the bar stays while a delete runs, even when the selection empties. */}
          {(s3.selectedKeys.size > 0 || s3.deleting) && (
            <BulkActionBar
              selectedCount={s3.selectedKeys.size}
              selectedFolderCount={1}
              onDelete={s3.bulkDelete}
              deleteProgress={s3.deleteProgress}
              onCancelDelete={s3.cancelBulkDelete}
            />
          )}
        </>
      );
    }
    const user = userEvent.setup();
    renderWithQuery(<Harness />);
    await user.click(await screen.findByRole('button', { name: 'pick' }));
    await user.click(screen.getByRole('button', { name: 'Delete 1 selected items' }));
    await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Delete' }));

    const toolbar = screen.getByRole('toolbar', { name: 'Selection actions' });
    await waitFor(() => expect(held).toHaveLength(1));
    await waitFor(() => expect(within(toolbar).getByRole('status')).toHaveTextContent('Deleting 0 of 1,201…'));
    expect(within(toolbar).getByRole('progressbar', { name: 'Bulk delete progress' })).toBeInTheDocument();

    held[0].answer();
    await waitFor(() => expect(within(toolbar).getByRole('status')).toHaveTextContent('Deleting 500 of 1,201…'));
    await waitFor(() => expect(held).toHaveLength(2));

    await user.click(within(toolbar).getByRole('button', { name: 'Cancel delete' }));
    expect(within(toolbar).getByRole('status')).toHaveTextContent('Stopping after this batch…');
    held[1].answer();
    expect(await screen.findByText('Delete stopped. 1,000 of 1,201 objects deleted.')).toBeInTheDocument();
    expect(http.callsTo('POST', DELETE)).toHaveLength(2);
    await waitFor(() => expect(screen.getByRole('toolbar', { name: 'Selection actions' })).toHaveTextContent('1 selected'));
  });
});
