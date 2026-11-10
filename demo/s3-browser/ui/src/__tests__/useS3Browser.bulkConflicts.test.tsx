/**
 * F17 (epic #146): a bulk copy or move never writes over an existing object
 * unasked. The browser looks for the destination keys first, then asks:
 * skip the existing ones, overwrite them, or cancel. A move never removes
 * the source of an object it did not copy.
 */
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { json, mockFetch, type RecordedRequest } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';
import { folderRowKey, objectRowKey } from '../rowKey';
import type { BulkTransferOutcome } from '../bulkTransfer';
import type { ConflictResolver } from '../bulkConflicts';

vi.mock('../s3client', () => ({
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
/** Keys that already exist in the destination bucket, by listing prefix. */
let existing: string[];

beforeEach(() => {
  existing = [];
  http = mockFetch();
  http.on('GET', LIST, (req: RecordedRequest) => {
    const qs = new URLSearchParams(req.path.split('?')[1]);
    const bucket = qs.get('bucket');
    const prefix = qs.get('prefix') ?? '';
    if (bucket === 'archive') return json({ keys: existing.filter((k) => k.startsWith(prefix)), truncated: false });
    if (prefix === 'builds/nightly/') return json({ keys: ['builds/nightly/a.zip', 'builds/nightly/b.zip'], truncated: false });
    throw new Error(`unexpected listing ${req.path}`);
  });
});
afterEach(() => vi.unstubAllGlobals());

function answerTransfers(op: 'copy' | 'move') {
  const sent: Item[][] = [];
  http.on('POST', PATH[op], (req: RecordedRequest) => {
    const { items } = req.body as { items: Item[] };
    sent.push(items);
    return json({ succeeded: items.length, failed: 0, failures: [], deleted: op === 'move' ? items.length : undefined });
  });
  return sent;
}

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

  test('asks with the exact existing keys, and "skip" sends only the others', async () => {
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
    expect(sent.flat()).toEqual([{ source_key: 'builds/nightly/b.zip', relative: 'nightly/b.zip' }]);
    expect(outcome).toMatchObject({ total: 1, skipped: 2, cancelled: false });
    // A move deletes only what it copied.
    expect(outcome.deleted).toBe(op === 'move' ? 1 : 0);
  });

  test('"overwrite" sends every item', async () => {
    const sent = answerTransfers(op);
    const { result } = await mount();
    const outcome = await run(result, op, async () => 'overwrite');
    expect(sent.flat().map((i) => i.source_key).sort()).toEqual([
      'builds/nightly/a.zip',
      'builds/nightly/b.zip',
      'builds/top.zip',
    ]);
    expect(outcome).toMatchObject({ total: 3 });
    expect(outcome.skipped).toBeUndefined();
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

  test('no conflict: no question, every item sent', async () => {
    existing = ['out/nightly/zzz-other.zip'];
    const sent = answerTransfers(op);
    const { result } = await mount();
    const ask = vi.fn<ConflictResolver>(async () => 'cancel');
    const outcome = await run(result, op, ask);
    expect(ask).not.toHaveBeenCalled();
    expect(sent.flat()).toHaveLength(3);
    expect(outcome).toMatchObject({ total: 3 });
    expect(outcome.skipped).toBeUndefined();
  });

  test('every item exists and the user skips: nothing is sent, nothing is removed', async () => {
    existing = ['out/nightly/a.zip', 'out/nightly/b.zip', 'out/top.zip'];
    const sent = answerTransfers(op);
    const { result } = await mount();
    const outcome = await run(result, op, async () => 'skip');
    expect(sent).toEqual([]);
    expect(outcome).toMatchObject({ total: 0, skipped: 3, deleted: 0, cancelled: false });
  });

  test('a destination listing that fails stops the run before any write', async () => {
    const sent = answerTransfers(op);
    http.on('GET', LIST, (req: RecordedRequest) =>
      req.path.includes('bucket=archive') ? json({ error: 'boom' }, 500) : json({ keys: ['builds/nightly/a.zip'], truncated: false }),
    );
    const { result } = await mount();
    await expect(run(result, op, async () => 'overwrite')).rejects.toThrow();
    expect(sent).toEqual([]);
  });
});
