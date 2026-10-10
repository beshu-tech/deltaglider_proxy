/**
 * What the object browser sends while it sits on a folder (cockroach scan
 * 2026-10-10, UI findings A, C and J). Each request here costs the proxy a
 * walk of the backend: the savings chip walks the folder's whole subtree, and
 * a table HEAD is two backend HEADs.
 *
 * - The savings chip loads once per folder: not on the 60 s auto-refresh,
 *   not after each bulk-delete batch, and a folder change aborts it.
 * - The table's HEAD enrichment keeps at most 6 requests in flight; a folder
 *   change aborts them and drops the queued ones.
 * - On another view (admin, upload, docs) the hook neither lists nor refreshes.
 *
 * S3 calls are stubbed at the s3client boundary; admin requests at fetch.
 */
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';

interface HeldHead {
  key: string;
  signal?: AbortSignal;
  settled: boolean;
  answer: () => void;
}

const s3 = vi.hoisted(() => ({
  /** ListObjectsV2 runs of the browsed folder. */
  lists: 0,
  /** Every table HEAD, in start order; each answers when the test says so. */
  heads: [] as HeldHead[],
}));

vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  listObjects: async () => {
    s3.lists++;
    return { objects: [], folders: ['builds/nightly/'], isTruncated: false };
  },
  headObject: (key: string, _bucket?: string, signal?: AbortSignal) =>
    new Promise((resolve, reject) => {
      const head: HeldHead = {
        key,
        signal,
        settled: false,
        answer: () => {
          head.settled = true;
          resolve({ storageType: 'delta', storedSize: 1 });
        },
      };
      signal?.addEventListener('abort', () => {
        head.settled = true;
        reject(signal.reason);
      });
      s3.heads.push(head);
    }),
}));

import useS3Browser from '../useS3Browser';

const SAVINGS = '/_/api/admin/deltaspace/savings';
const LIST = '/_/api/admin/objects/list';
const DELETE = '/_/api/admin/objects/delete';

const savings = json({
  bucket: 'releases',
  prefix: 'builds/',
  totals: {
    original_bytes: 100,
    stored_bytes: 10,
    reference_bytes: 5,
    delta_stored_bytes: 5,
    passthrough_bytes: 0,
    reference_count: 1,
    delta_count: 2,
    passthrough_count: 0,
  },
  savings_percentage: 90,
  truncated: false,
  computed_at: '2026-10-10T00:00:00Z',
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  s3.lists = 0;
  s3.heads = [];
  http = mockFetch();
  http.on('GET', SAVINGS, savings);
  Object.defineProperty(document, 'hidden', { configurable: true, value: false });
  // Only the intervals are fake, and they follow real time too (waitFor polls
  // with one); a test jumps a minute ahead with advanceTimersByTimeAsync.
  vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'], shouldAdvanceTime: true });
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

type Props = { prefix: string; active?: boolean };

/** The hook on bucket `releases`, with an admin session; `setProps` re-renders it with new props. */
function mount(initial: Props = { prefix: 'builds/' }) {
  let props = initial;
  const view = renderHookWithQuery(() =>
    useS3Browser({
      bucket: 'releases',
      prefix: props.prefix,
      q: '',
      object: '',
      navigateUrl: () => {},
      adminSession: true,
      active: props.active,
    }),
  );
  return {
    ...view,
    setProps(next: Props) {
      props = next;
      view.rerender();
    },
  };
}

/** One act per minute: two ticks inside one act would render (and list) once. */
async function minutesPass(minutes: number) {
  for (let i = 0; i < minutes; i++) {
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
  }
}

const savingsCalls = () => http.callsTo('GET', SAVINGS);
const keys = (n: number, folder = 'builds/') => Array.from({ length: n }, (_, i) => `${folder}app-${i}.zip`);
const inFlight = () => s3.heads.filter((h) => !h.settled);

describe('savings chip', () => {
  test('loads once per folder, not on the 60 s auto-refresh', async () => {
    const { result, setProps } = mount();
    await waitFor(() => expect(result.current.deltaSummary?.savingsPct).toBe(90));
    expect(savingsCalls()).toHaveLength(1);

    await minutesPass(2);
    // The listing did refresh twice; the savings walk did not.
    await waitFor(() => expect(s3.lists).toBe(3));
    expect(savingsCalls()).toHaveLength(1);

    setProps({ prefix: 'builds/nightly/' });
    await waitFor(() => expect(savingsCalls()).toHaveLength(2));
    expect(new URLSearchParams(savingsCalls()[1].path.split('?')[1]).get('prefix')).toBe('builds/nightly/');
  });

  test('a folder change aborts the savings request still in flight', async () => {
    http.on('GET', SAVINGS, () => () => new Promise<Response>(() => {}));
    const { setProps } = mount();
    await waitFor(() => expect(savingsCalls()).toHaveLength(1));
    const first = savingsCalls()[0];
    expect(first.signal?.aborted).toBe(false);

    setProps({ prefix: 'builds/nightly/' });
    await waitFor(() => expect(savingsCalls()).toHaveLength(2));
    expect(first.signal?.aborted).toBe(true);
    expect(savingsCalls()[1].signal?.aborted).toBe(false);
  });

  test('a bulk delete loads it once when the run ends, not after each batch', async () => {
    http.on('GET', LIST, json({ keys: keys(1201, 'builds/nightly/'), truncated: false }));
    // Each batch answers when the test says so, after the reload of the batch before it rendered.
    const held: (() => void)[] = [];
    http.on('POST', DELETE, (req) => () =>
      new Promise<Response>((resolve) => {
        const n = (req.body as { keys: string[] }).keys.length;
        held.push(() => resolve(json({ deleted: n, failed: 0, failures: [] })));
      }),
    );
    const { result } = mount();
    await waitFor(() => expect(result.current.deltaSummary?.savingsPct).toBe(90));
    const before = savingsCalls().length;

    act(() => result.current.toggleKey('folder:builds/nightly/'));
    let run!: Promise<unknown>;
    act(() => {
      run = result.current.bulkDelete();
    });
    for (let batch = 1; batch <= 3; batch++) {
      await waitFor(() => expect(held).toHaveLength(batch));
      const lists = s3.lists;
      await act(async () => held[batch - 1]());
      // The listing reloads after each batch, as before.
      await waitFor(() => expect(s3.lists).toBeGreaterThan(lists));
    }
    await act(async () => {
      await run;
    });
    await waitFor(() => expect(savingsCalls()).toHaveLength(before + 1));
    // Settle any late effect, then check that no other walk started.
    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(savingsCalls()).toHaveLength(before + 1);
  });
});

describe('table HEAD enrichment', () => {
  test('keeps at most 6 HEADs in flight and queues the rest', async () => {
    const { result } = mount();
    await waitFor(() => expect(result.current.loading).toBe(false));
    const page = keys(50);
    act(() => result.current.enrichKeys(page));
    expect(s3.heads).toHaveLength(6);

    let peak = 0;
    while (s3.heads.length < 50 || inFlight().length > 0) {
      peak = Math.max(peak, inFlight().length);
      const next = inFlight()[0];
      await act(async () => {
        next.answer();
      });
      await waitFor(() => expect(inFlight().length).toBe(Math.min(6, 50 - s3.heads.filter((h) => h.settled).length)));
    }
    expect(peak).toBe(6);
    expect(s3.heads.map((h) => h.key)).toEqual(page);
    await waitFor(() => expect(Object.keys(result.current.headCache)).toHaveLength(50));
    expect(result.current.headCache[page[49]]).toEqual({ storageType: 'delta', storedSize: 1, error: false });
  });

  test('a folder change aborts the HEADs in flight and drops the queued ones', async () => {
    const { result, setProps } = mount();
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => result.current.enrichKeys(keys(50)));
    expect(s3.heads).toHaveLength(6);
    expect(s3.heads.every((h) => h.signal && !h.signal.aborted)).toBe(true);

    setProps({ prefix: 'other/' });
    await waitFor(() => expect(s3.heads.every((h) => h.signal?.aborted)).toBe(true));
    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(s3.heads).toHaveLength(6);
    expect(result.current.headCache).toEqual({});
  });

  test('a newer page replaces the keys still queued for the older one', async () => {
    const { result } = mount();
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => result.current.enrichKeys(keys(50)));
    const page2 = keys(10, 'builds/page2/');
    act(() => result.current.enrichKeys(page2));
    // The six HEADs of page 1 already run; page 1's other 44 keys are no
    // longer on screen, so page 2's keys go next.
    while (inFlight().length > 0) {
      const next = inFlight()[0];
      await act(async () => {
        next.answer();
      });
    }
    await waitFor(() => expect(s3.heads).toHaveLength(16));
    expect(s3.heads.slice(6).map((h) => h.key)).toEqual(page2);
  });
});

describe('another view', () => {
  test('an inactive browser neither lists nor refreshes, and loads the savings chip only once shown', async () => {
    const { result, setProps } = mount({ prefix: '', active: false });
    await minutesPass(2);
    expect(s3.lists).toBe(0);
    expect(savingsCalls()).toHaveLength(0);
    expect(result.current.loading).toBe(false);

    setProps({ prefix: '', active: true });
    await waitFor(() => expect(s3.lists).toBe(1));
    await waitFor(() => expect(savingsCalls()).toHaveLength(1));
    await minutesPass(1);
    await waitFor(() => expect(s3.lists).toBe(2));
  });
});
