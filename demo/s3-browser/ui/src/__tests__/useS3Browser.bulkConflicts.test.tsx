/**
 * F17 (epic #146): a bulk copy or move never writes over an existing object
 * unasked. The browser lists the destination folders first, then asks: skip
 * the existing ones, overwrite them, or cancel. The SERVER keeps the promise:
 * every item that may not replace an object goes with `no_overwrite`, and the
 * fake server below judges it as the real one does. A move never removes the
 * source of an object it did not copy.
 */
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { json, mockFetch, type RecordedRequest } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';
import { folderRowKey, objectRowKey } from '../rowKey';
import type { BulkTransferOutcome } from '../bulkTransfer';
import type { ConflictResolver } from '../bulkConflicts';

/** What the destination bucket lists, per folder (direct children). Set per test. */
const lister = vi.hoisted(() => ({
  list: async (_bucket: string, _folder: string): Promise<{ keys: string[]; truncated: boolean }> => ({ keys: [], truncated: false }),
}));

vi.mock('../s3client', () => ({
  listDirectKeys: (bucket: string, folder: string) => lister.list(bucket, folder),
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  headObject: async () => ({ storageType: 'passthrough', storedSize: 1 }),
  listBuckets: async () => [{ name: 'releases' }, { name: 'archive' }],
  listObjects: async () => ({
    objects: [{ key: 'builds/top.zip', size: 1, lastModified: '2026-01-01T00:00:00Z' }],
    folders: ['builds/nightly/'],
    isTruncated: false,
  }),
}));

import useS3Browser from '../useS3Browser';

const LIST = '/_/api/admin/objects/list';
const PATH = { copy: '/_/api/admin/objects/copy', move: '/_/api/admin/objects/move' } as const;

type Item = { source_key: string; relative: string };
let http: ReturnType<typeof mockFetch>;
/** Keys that already exist in the destination bucket. */
let existing: string[];
/** Keys that exist in the destination but that the user cannot see in a listing. */
let hidden: string[];

beforeEach(() => {
  existing = [];
  hidden = [];
  lister.list = async (bucket, folder) => ({
    keys: bucket === 'archive' ? existing.filter((k) => k.startsWith(folder) && !hidden.includes(k) && !k.slice(folder.length).includes('/')) : [],
    truncated: false,
  });
  http = mockFetch();
  http.on('GET', LIST, (req: RecordedRequest) => {
    const qs = new URLSearchParams(req.path.split('?')[1]);
    if (qs.get('prefix') === 'builds/nightly/') return json({ keys: ['builds/nightly/a.zip', 'builds/nightly/b.zip'], truncated: false });
    throw new Error(`unexpected listing ${req.path}`);
  });
});
afterEach(() => vi.unstubAllGlobals());

interface TransferBody {
  items: Item[];
  dest_prefix: string;
  no_overwrite?: boolean;
}
/** The requests as sent: the items and whether the request may replace. */
interface Sent {
  items: Item[];
  noOverwrite: boolean;
}

/** A server that judges `no_overwrite` per item against `existing`, as the real one does under its lock. */
function answerTransfers(op: 'copy' | 'move') {
  const sent: Sent[] = [];
  http.on('POST', PATH[op], (req: RecordedRequest) => {
    const body = req.body as TransferBody;
    sent.push({ items: body.items, noOverwrite: body.no_overwrite === true });
    const skipped = body.no_overwrite ? body.items.filter((i) => existing.includes(body.dest_prefix + i.relative)).length : 0;
    const copied = body.items.length - skipped;
    return json({ succeeded: copied, failed: 0, skipped, failures: [], deleted: op === 'move' ? copied : undefined });
  });
  return sent;
}
const sources = (sent: Sent[]) => sent.flatMap((r) => r.items.map((i) => i.source_key)).sort();

async function mount() {
  const view = renderHookWithQuery(() =>
    useS3Browser({ bucket: 'releases', prefix: 'builds/', q: '', object: '', navigateUrl: () => {} }),
  );
  await waitFor(() => expect(view.result.current.objects).toHaveLength(1));
  await waitFor(() => expect(view.result.current.loading).toBe(false));
  return view;
}

type Browser = { current: ReturnType<typeof useS3Browser> };
type Ask = Parameters<Browser['current']['bulkCopy']>[2];
/** Check a folder and a file, then run the bulk action; resolves with its outcome. */
async function run(result: Browser, op: 'copy' | 'move', onConflicts?: Ask) {
  act(() => result.current.toggleKey(folderRowKey('builds/nightly/')));
  act(() => result.current.toggleKey(objectRowKey('builds/top.zip')));
  let outcome!: BulkTransferOutcome;
  await act(async () => {
    const start = op === 'copy' ? result.current.bulkCopy : result.current.bulkMove;
    outcome = await start('archive', 'out/', onConflicts);
  });
  return outcome;
}

describe.each(['copy', 'move'] as const)('bulk %s onto existing objects', (op) => {
  beforeEach(() => {
    existing = ['out/nightly/a.zip', 'out/top.zip', 'out/nightly/zzz-other.zip'];
  });

  test('asks with the exact existing keys, and "skip" sends every item conditionally', async () => {
    const sent = answerTransfers(op);
    const { result } = await mount();
    const ask = vi.fn<ConflictResolver>(async () => 'skip');
    const outcome = await run(result, op, ask);
    expect(ask).toHaveBeenCalledTimes(1);
    expect(ask.mock.calls[0][0]).toMatchObject({ action: op, total: 3 });
    expect(ask.mock.calls[0][0].conflicts.map((c) => c.dest).sort()).toEqual([
      'out/nightly/a.zip',
      'out/top.zip',
    ]);
    // Nothing may replace: every request says so, and the server leaves the two.
    expect(sent.every((r) => r.noOverwrite)).toBe(true);
    expect(sources(sent)).toEqual(['builds/nightly/a.zip', 'builds/nightly/b.zip', 'builds/top.zip']);
    expect(outcome).toMatchObject({ total: 3, succeeded: 1, skipped: 2, cancelled: false });
    // A move deletes only what it copied.
    expect(outcome.deleted).toBe(op === 'move' ? 1 : 0);
  });

  test('"overwrite" lets only the listed conflicts replace; the others stay conditional', async () => {
    // An object the user cannot list at the destination: it is not in the dialog.
    existing = [...existing, 'out/nightly/b.zip'];
    hidden = ['out/nightly/b.zip'];
    const sent = answerTransfers(op);
    const { result } = await mount();
    const outcome = await run(result, op, async () => 'overwrite');
    const unconditional = sent.filter((r) => !r.noOverwrite).flatMap((r) => r.items.map((i) => i.source_key));
    const conditional = sent.filter((r) => r.noOverwrite).flatMap((r) => r.items.map((i) => i.source_key));
    expect(unconditional.sort()).toEqual(['builds/nightly/a.zip', 'builds/top.zip']);
    expect(conditional).toEqual(['builds/nightly/b.zip']);
    // The hidden object is skipped by the server, never overwritten.
    expect(outcome).toMatchObject({ total: 3, succeeded: 2, skipped: 1 });
  });

  test('"cancel" sends nothing and keeps the selection', async () => {
    const sent = answerTransfers(op);
    const { result } = await mount();
    const outcome = await run(result, op, async () => 'cancel');
    expect(sent).toEqual([]);
    expect(outcome.cancelled).toBe(true);
    expect(result.current.selectedKeys.size).toBe(2);
  });

  test('without a way to ask, an existing destination stops the run (fail closed)', async () => {
    const sent = answerTransfers(op);
    const { result } = await mount();
    const outcome = await run(result, op);
    expect(sent).toEqual([]);
    expect(outcome.cancelled).toBe(true);
  });

  test('no conflict: no question, every item sent conditionally', async () => {
    existing = ['out/nightly/zzz-other.zip'];
    const sent = answerTransfers(op);
    const { result } = await mount();
    const ask = vi.fn<ConflictResolver>(async () => 'cancel');
    const outcome = await run(result, op, ask);
    expect(ask).not.toHaveBeenCalled();
    expect(sent.every((r) => r.noOverwrite)).toBe(true);
    expect(sources(sent)).toHaveLength(3);
    expect(outcome).toMatchObject({ total: 3, succeeded: 3 });
    expect(outcome.skipped).toBeUndefined();
  });

  test('an object that appears after the check is skipped by the server, not overwritten', async () => {
    // The listing sees an empty destination; the object lands before the write.
    existing = [];
    lister.list = async () => ({ keys: [], truncated: false });
    const sent = answerTransfers(op);
    const { result } = await mount();
    existing = ['out/top.zip'];
    const outcome = await run(result, op, async () => 'cancel');
    expect(sent.every((r) => r.noOverwrite)).toBe(true);
    expect(outcome).toMatchObject({ total: 3, succeeded: 2, skipped: 1 });
  });

  test('every item exists and the user skips: the server skips them all, nothing is removed', async () => {
    existing = ['out/nightly/a.zip', 'out/nightly/b.zip', 'out/top.zip'];
    const sent = answerTransfers(op);
    const { result } = await mount();
    const outcome = await run(result, op, async () => 'skip');
    expect(sent.every((r) => r.noOverwrite)).toBe(true);
    expect(outcome).toMatchObject({ total: 3, succeeded: 0, skipped: 3, deleted: 0, cancelled: false });
  });

  test('a destination the user cannot list does not fail the run: the copy goes conditionally', async () => {
    lister.list = async () => {
      throw new Error('AccessDenied');
    };
    const sent = answerTransfers(op);
    const { result } = await mount();
    const ask = vi.fn<ConflictResolver>(async () => 'overwrite');
    const outcome = await run(result, op, ask);
    expect(ask).not.toHaveBeenCalled();
    expect(sent.every((r) => r.noOverwrite)).toBe(true);
    // The server judges per item: the two existing objects are skipped, the other is copied.
    expect(outcome).toMatchObject({ total: 3, succeeded: 1, skipped: 2, cancelled: false });
  });
});
