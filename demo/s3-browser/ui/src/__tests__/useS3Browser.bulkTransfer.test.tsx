/**
 * Bulk copy and move run like the v2.0.4 bulk delete (cockroach scan
 * 2026-10-10, UI finding H, limits finding 3). They sent the whole selection
 * as ONE request that the proxy ran one item at a time: no progress, no
 * cancel, a front-proxy timeout showed an error while the copy went on, and
 * 50 selected folders were listed in full (up to 500,000 keys) before the
 * server refused anything over 10,000.
 *
 * Now: the folders are listed a few at a time and the listing stops once the
 * selection passes 10,000 objects; the items go in batches of at most 500 per
 * request, with progress, Cancel (after the batch in flight), a leave-page
 * warning, errors that say how many objects went before the failure, and a
 * session expiry handed to the sign-in path.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { BULK_BATCH_SIZE } from '../bulkBatches';
import type { BulkTransferOutcome } from '../bulkTransfer';
import { isSessionExpired } from '../errorHandling';
import { json, mockFetch, type RecordedRequest } from '../test/fetchMock';
import { renderHookWithQuery, renderWithQuery } from '../test/render';

const listing = vi.hoisted(() => ({ loads: 0 }));

vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  headObject: async () => ({ storageType: 'passthrough', storedSize: 1 }),
  listBuckets: async () => [{ name: 'releases' }, { name: 'archive' }],
  listObjects: async () => {
    listing.loads++;
    return { objects: [], folders: ['builds/nightly/'], isTruncated: false };
  },
}));

import useS3Browser from '../useS3Browser';
import BulkActionBar from '../components/BulkActionBar';

const LIST = '/_/api/admin/objects/list';
const PATH = { copy: '/_/api/admin/objects/copy', move: '/_/api/admin/objects/move' } as const;
const BIG = Array.from({ length: 1201 }, (_, i) => `builds/nightly/${String(i).padStart(4, '0')}.zip`);

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  listing.loads = 0;
  http = mockFetch();
  http.on('GET', LIST, json({ keys: BIG, truncated: false }));
});
afterEach(() => {
  vi.unstubAllGlobals();
});

type Item = { source_key: string; relative: string };

/** Copy / move requests that answer only when the test says so. */
function heldRequests(op: 'copy' | 'move') {
  const held: { items: Item[]; answer: (res?: Response) => void }[] = [];
  http.on('POST', PATH[op], (req: RecordedRequest) => () =>
    new Promise<Response>((resolve) => {
      const { items } = req.body as { items: Item[] };
      const n = items.length;
      held.push({
        items,
        answer: (res = json({ succeeded: n, failed: 0, failures: [], deleted: op === 'move' ? n : undefined })) =>
          resolve(res),
      });
    }),
  );
  return held;
}

async function mountBrowser() {
  const view = renderHookWithQuery(() =>
    useS3Browser({ bucket: 'releases', prefix: 'builds/', q: '', object: '', navigateUrl: () => {} }),
  );
  await waitFor(() => expect(view.result.current.loading).toBe(false));
  return view;
}

type Browser = { current: ReturnType<typeof useS3Browser> };

function start(result: Browser, op: 'copy' | 'move', destPrefix = 'old/') {
  act(() => result.current.toggleKey('folder:builds/nightly/'));
  let run!: Promise<BulkTransferOutcome>;
  act(() => {
    run = op === 'copy' ? result.current.bulkCopy('archive', destPrefix) : result.current.bulkMove('archive', destPrefix);
  });
  run.catch(() => {}); // the test awaits it; keep an early rejection from counting as unhandled
  return run;
}

const past = { copy: 'copied', move: 'moved' } as const;

describe.each(['copy', 'move'] as const)('bulk %s', (op) => {
  test(`sends batches of ${BULK_BATCH_SIZE} with progress, then clears the selection`, async () => {
    const held = heldRequests(op);
    const { result } = await mountBrowser();
    const run = start(result, op);
    await waitFor(() => expect(held).toHaveLength(1));
    expect(held[0].items).toHaveLength(BULK_BATCH_SIZE);
    expect(held[0].items[0]).toEqual({ source_key: BIG[0], relative: `nightly/${BIG[0].split('/').pop()}` });
    expect(result.current.bulkProgress).toEqual({ action: op, phase: 'sending', done: 0, total: 1201, stopping: false });

    held[0].answer();
    await waitFor(() => expect(held).toHaveLength(2));
    await waitFor(() => expect(result.current.bulkProgress).toMatchObject({ done: 500, total: 1201 }));
    held[1].answer();
    await waitFor(() => expect(held).toHaveLength(3));
    expect(held.map((h) => h.items.length)).toEqual([500, 500, 201]);
    expect(held.flatMap((h) => h.items.map((i) => i.source_key))).toEqual(BIG);
    held[2].answer();

    await expect(run).resolves.toEqual({
      action: op,
      total: 1201,
      succeeded: 1201,
      failed: 0,
      deleted: op === 'move' ? 1201 : 0,
      failures: [],
      cancelled: false,
    });
    await waitFor(() => expect(result.current.bulkRunning).toBe(false));
    expect(result.current.bulkProgress).toBeNull();
    expect(result.current.selectedKeys.size).toBe(0);
  });

  test('cancel lets the batch in flight finish, then sends nothing more and keeps the selection', async () => {
    const held = heldRequests(op);
    const { result } = await mountBrowser();
    const run = start(result, op);
    await waitFor(() => expect(held).toHaveLength(1));
    act(() => result.current.cancelBulk());
    expect(result.current.bulkProgress).toMatchObject({ action: op, phase: 'sending', stopping: true });
    held[0].answer();
    await expect(run).resolves.toMatchObject({ total: 1201, succeeded: 500, cancelled: true });
    expect(http.callsTo('POST', PATH[op])).toHaveLength(1);
    await waitFor(() => expect(result.current.bulkRunning).toBe(false));
    expect([...result.current.selectedKeys]).toEqual(['folder:builds/nightly/']);
  });

  test('a failure mid-run says how many objects went before it', async () => {
    const held = heldRequests(op);
    const { result } = await mountBrowser();
    const run = start(result, op);
    await waitFor(() => expect(held).toHaveLength(1));
    held[0].answer();
    await waitFor(() => expect(held).toHaveLength(2));
    held[1].answer(json({ error: 'disk full' }, 500));
    const label = op === 'copy' ? 'Bulk copy' : 'Bulk move';
    await expect(run).rejects.toThrow(
      `${label} failed (500): disk full. 500 of 1,201 items were ${past[op]} before the failure.`,
    );
    expect(http.callsTo('POST', PATH[op])).toHaveLength(2);
    await waitFor(() => expect(result.current.bulkRunning).toBe(false));
    expect(result.current.selectedKeys.size).toBe(1);
  });

  test('an expired session mid-run rejects with the session error itself (sign-in path)', async () => {
    const held = heldRequests(op);
    const { result } = await mountBrowser();
    const run = start(result, op);
    await waitFor(() => expect(held).toHaveLength(1));
    held[0].answer(json({ error: 'unauthorized' }, 401));
    const err = await run.then(
      () => null,
      (e: unknown) => e,
    );
    expect(isSessionExpired(err)).toBe(true);
    await waitFor(() => expect(result.current.bulkRunning).toBe(false));
  });

  test('leaving the page asks first while it runs', async () => {
    const held = heldRequests(op);
    const { result } = await mountBrowser();
    const leave = () => {
      const ev = new Event('beforeunload', { cancelable: true });
      window.dispatchEvent(ev);
      return ev.defaultPrevented;
    };
    expect(leave()).toBe(false);
    const run = start(result, op);
    await waitFor(() => expect(held).toHaveLength(1));
    expect(leave()).toBe(true);
    act(() => result.current.cancelBulk());
    held[0].answer();
    await run;
    await waitFor(() => expect(result.current.bulkRunning).toBe(false));
    expect(leave()).toBe(false);
  });

  test('a selection over 10,000 objects stops listing more folders and sends nothing', async () => {
    const folders = Array.from({ length: 30 }, (_, i) => `builds/f${String(i).padStart(2, '0')}/`);
    http.on('GET', LIST, (req: RecordedRequest) => {
      const prefix = new URLSearchParams(req.path.split('?')[1]).get('prefix') ?? '';
      return json({ keys: Array.from({ length: 400 }, (_, i) => `${prefix}${i}.zip`), truncated: false });
    });
    http.on('POST', PATH[op], json({ succeeded: 0, failed: 0, failures: [], deleted: 0 }));
    const { result } = await mountBrowser();
    for (const f of folders) act(() => result.current.toggleKey(`folder:${f}`));
    await act(async () => {
      await expect(
        op === 'copy' ? result.current.bulkCopy('archive', 'old/') : result.current.bulkMove('archive', 'old/'),
      ).rejects.toThrow(/more than 10,000 objects.*Nothing was (copied|moved)\.$/);
    });
    // 26 folders hold 10,400 objects; the four requests in flight may finish, no more start.
    expect(http.callsTo('GET', LIST).length).toBeLessThanOrEqual(26 + 3);
    expect(http.callsTo('POST', PATH[op])).toHaveLength(0);
  });

  test('a destination that is another selected object is refused before the first request', async () => {
    // Same bucket; the folder's own subfolder is the destination, and one of
    // its keys (in the third batch) is where an object of the first batch lands.
    http.on('GET', LIST, json({ keys: [...BIG, 'builds/nightly/x/nightly/0000.zip'], truncated: false }));
    http.on('POST', PATH[op], json({ succeeded: 0, failed: 0, failures: [], deleted: 0 }));
    const { result } = await mountBrowser();
    act(() => result.current.toggleKey('folder:builds/nightly/'));
    await act(async () => {
      const run = op === 'copy' ? result.current.bulkCopy('releases', 'builds/nightly/x/') : result.current.bulkMove('releases', 'builds/nightly/x/');
      await expect(run).rejects.toThrow(/builds\/nightly\/x\/nightly\/0000\.zip/);
    });
    expect(http.callsTo('POST', PATH[op])).toHaveLength(0);
  });
});

// The same cap guards the ZIP, which used to list every folder first and
// then refuse the selection over 10,000 files.
test('a ZIP of more than 10,000 objects stops listing more folders', async () => {
  http.on('GET', LIST, (req: RecordedRequest) => {
    const prefix = new URLSearchParams(req.path.split('?')[1]).get('prefix') ?? '';
    return json({ keys: Array.from({ length: 400 }, (_, i) => `${prefix}${i}.zip`), truncated: false });
  });
  const { result } = await mountBrowser();
  for (let i = 0; i < 30; i++) act(() => result.current.toggleKey(`folder:builds/f${String(i).padStart(2, '0')}/`));
  await act(async () => {
    await expect(result.current.downloadZip()).rejects.toThrow(/more than 10,000 objects/);
  });
  expect(http.callsTo('GET', LIST).length).toBeLessThanOrEqual(26 + 3);
});

test('a move reloads the listing after each batch: moved sources leave the page', async () => {
  const held = heldRequests('move');
  const { result } = await mountBrowser();
  start(result, 'move');
  await waitFor(() => expect(held).toHaveLength(1));
  const loads = listing.loads;
  held[0].answer();
  await waitFor(() => expect(held).toHaveLength(2));
  await waitFor(() => expect(listing.loads).toBe(loads + 1));
  act(() => result.current.cancelBulk());
  held[1].answer();
});

test('wired to the action bar: the copy closes the picker, shows its progress, and Cancel stops it', async () => {
  const held = heldRequests('copy');
  function Harness() {
    const s3 = useS3Browser({ bucket: 'releases', prefix: 'builds/', q: '', object: '', navigateUrl: () => {} });
    return (
      <>
        <button onClick={() => s3.toggleKey('folder:builds/nightly/')}>pick</button>
        {(s3.selectedKeys.size > 0 || s3.bulkRunning) && (
          <BulkActionBar
            selectedCount={s3.selectedKeys.size}
            selectedFolderCount={1}
            onCopy={s3.bulkCopy}
            progress={s3.bulkProgress}
            onCancel={s3.cancelBulk}
            currentPrefix={s3.prefix}
            selectionKeys={s3.selectedKeys}
          />
        )}
      </>
    );
  }
  const user = userEvent.setup();
  renderWithQuery(<Harness />);
  await user.click(await screen.findByRole('button', { name: 'pick' }));
  await user.click(screen.getByRole('button', { name: 'Copy 1 selected items' }));
  const dialog = await screen.findByRole('dialog');
  const path = within(dialog).getByPlaceholderText('/ (bucket root)');
  await user.clear(path);
  await user.type(path, 'old');
  await user.click(within(dialog).getByRole('button', { name: 'Copy 1 item' }));

  const toolbar = screen.getByRole('toolbar', { name: 'Selection actions' });
  await waitFor(() => expect(held).toHaveLength(1));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  await waitFor(() => expect(within(toolbar).getByRole('status')).toHaveTextContent('Copying 0 of 1,201…'));
  expect(within(toolbar).getByRole('progressbar', { name: 'Bulk copy progress' })).toBeInTheDocument();

  held[0].answer();
  await waitFor(() => expect(within(toolbar).getByRole('status')).toHaveTextContent('Copying 500 of 1,201…'));
  await waitFor(() => expect(held).toHaveLength(2));
  await user.click(within(toolbar).getByRole('button', { name: 'Cancel copy' }));
  expect(within(toolbar).getByRole('status')).toHaveTextContent('Stopping after this batch…');
  held[1].answer();
  expect(await screen.findByText('Copy stopped. 1,000 of 1,201 items copied.')).toBeInTheDocument();
  expect(http.callsTo('POST', PATH.copy)).toHaveLength(2);
});
