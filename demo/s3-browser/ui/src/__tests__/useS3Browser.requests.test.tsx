/**
 * What the object browser sends while it sits on a folder (cockroach scan
 * 2026-10-10, UI findings A, C and J). Each request here costs the proxy a
 * walk of the backend: the savings chip walks the folder's whole subtree.
 *
 * - The savings chip loads once per folder: not on the 60 s auto-refresh,
 *   not after each bulk-delete batch, and a folder change aborts it.
 * - On another view (admin, upload, docs) the hook neither lists nor refreshes.
 *
 * S3 calls are stubbed at the s3client boundary; admin requests at fetch.
 */
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';

const s3 = vi.hoisted(() => ({
  /** ListObjectsV2 runs of the browsed folder. */
  lists: 0,
}));

vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  listObjects: async () => {
    s3.lists++;
    return { objects: [], folders: ['builds/nightly/'], isTruncated: false };
  },
}));

import useS3Browser from '../useS3Browser';

const SAVINGS = '/_/api/admin/deltaspace/savings';
const LIST = '/_/api/admin/objects/list';
const DELETE_JOB = '/_/api/admin/objects/delete-jobs';

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

  test('a bulk delete loads it once, when its job has started', async () => {
    http.on('GET', LIST, json({ keys: keys(1201, 'builds/nightly/'), truncated: false }));
    http.on('POST', DELETE_JOB, json({ job_id: 1 }, 202));
    const { result } = mount();
    await waitFor(() => expect(result.current.deltaSummary?.savingsPct).toBe(90));
    const before = savingsCalls().length;
    act(() => result.current.toggleKey('folder:builds/nightly/'));
    await act(async () => {
      await result.current.bulkDelete();
    });
    await waitFor(() => expect(savingsCalls()).toHaveLength(before + 1));
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
