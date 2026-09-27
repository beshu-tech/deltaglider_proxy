// After a successful apply, useSectionEditor re-reads the section. That read
// must not overwrite an edit typed while it is in flight (review4 frontend-1).
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { useSectionEditor } from '../useSectionEditor';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/advanced';
interface Caches {
  cache_size_mb: number;
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

test('an edit made during the post-apply re-read survives and reads dirty', async () => {
  http.on('GET', SECTION, json({ cache_size_mb: 100 }));
  http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: { advanced: { cache_size_mb: { before: 100, after: 256 } } } }));
  http.on('PUT', SECTION, json({ ok: true }));
  const { result } = renderHookWithQuery(() =>
    useSectionEditor<Caches>({ section: 'advanced', dirtyKey: 'advanced/caches', initial: { cache_size_mb: 100 } }),
  );
  await waitFor(() => expect(result.current.loading).toBe(false));

  act(() => result.current.setValue({ cache_size_mb: 256 }));
  await act(() => result.current.runApply());

  // The refetch after the PUT hangs until we release it (a slow link).
  let release: (r: Response) => void = () => {};
  const gate = new Promise<Response>((res) => {
    release = res;
  });
  http.on('GET', SECTION, () => gate);

  await act(async () => {
    expect(await result.current.confirmApply()).toBe(true);
  });
  expect(http.callsTo('GET', SECTION)).toHaveLength(2);

  // The operator keeps editing the (still rendered) form.
  act(() => result.current.setValue({ cache_size_mb: 999 }));
  expect(result.current.isDirty).toBe(true);
  expect(result.current.value.cache_size_mb).toBe(999);

  // The refetch lands: server truth is 256 (what was applied).
  await act(async () => {
    release(json({ cache_size_mb: 256 }));
    await gate;
  });
  // The post-apply read never flips `loading` (that unmounts gated forms).
  expect(result.current.loading).toBe(false);

  expect(result.current.value.cache_size_mb).toBe(999);
  expect(result.current.isDirty).toBe(true);
});

test('a clean form adopts the server state the post-apply re-read returns', async () => {
  http.on('GET', SECTION, json({ cache_size_mb: 100 }));
  http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: {} }));
  http.on('PUT', SECTION, json({ ok: true }));
  const { result } = renderHookWithQuery(() =>
    useSectionEditor<Caches>({ section: 'advanced', dirtyKey: 'advanced/caches', initial: { cache_size_mb: 100 } }),
  );
  await waitFor(() => expect(result.current.loading).toBe(false));
  act(() => result.current.setValue({ cache_size_mb: 256 }));
  await act(() => result.current.runApply());
  // The server normalises the applied value.
  http.on('GET', SECTION, json({ cache_size_mb: 300 }));
  await act(async () => {
    await result.current.confirmApply();
  });
  await waitFor(() => expect(result.current.value.cache_size_mb).toBe(300));
  expect(result.current.isDirty).toBe(false);
});
