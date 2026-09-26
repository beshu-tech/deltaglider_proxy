/**
 * useSectionEditor: the one apply protocol every config panel uses.
 * fetch → dirty → validate → ApplyDialog → PUT → markApplied, plus the
 * failure paths that must keep the operator's edits.
 */
import { act, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { qk } from '../queries/keys';
import { getDirtySections, requestApplyFirst } from '../useDirtySection';
import { useSectionEditor } from '../useSectionEditor';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery, testQueryClient } from '../test/render';

const SECTION = '/_/api/admin/config/section/advanced';

interface Caches {
  cache_size_mb: number;
  log_level?: string;
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

function mountEditor(opts: Partial<Parameters<typeof useSectionEditor<Caches>>[0]> = {}) {
  return renderHookWithQuery(() =>
    useSectionEditor<Caches>({
      section: 'advanced',
      dirtyKey: 'advanced/caches',
      initial: { cache_size_mb: 100, log_level: 'info' },
      ...opts,
    }),
  );
}

describe('fetch', () => {
  test('loads the section and merges it over the initial value', async () => {
    http.on('GET', SECTION, json({ cache_size_mb: 512 }));
    const { result } = mountEditor();
    expect(result.current.loading).toBe(true);
    await waitFor(() => expect(result.current.loading).toBe(false));
    // Absent fields keep their form defaults.
    expect(result.current.value).toEqual({ cache_size_mb: 512, log_level: 'info' });
    expect(result.current.isDirty).toBe(false);
    expect(result.current.error).toBeNull();
  });

  test('pick maps the wire body to the local shape', async () => {
    http.on('GET', SECTION, json({ cache_size_mb: 64, log_level: 'debug', other: 1 }));
    const { result } = mountEditor({ pick: (b) => ({ cache_size_mb: b.cache_size_mb }) });
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.value).toEqual({ cache_size_mb: 64 });
  });

  test('a 401 calls onSessionExpired and shows no error', async () => {
    http.on('GET', SECTION, json({ error: 'unauthorized' }, 401));
    const onSessionExpired = vi.fn();
    const { result } = mountEditor({ onSessionExpired });
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(onSessionExpired).toHaveBeenCalledTimes(1);
    expect(result.current.error).toBeNull();
  });

  test('a 500 sets an operator-facing error', async () => {
    http.on('GET', SECTION, json({ error: 'backend down' }, 500));
    const onSessionExpired = vi.fn();
    const { result } = mountEditor({ onSessionExpired, noun: 'caches' });
    await waitFor(() => expect(result.current.error).not.toBeNull());
    expect(result.current.error).toMatch(/^Failed to load caches section: .*backend down/);
    expect(onSessionExpired).not.toHaveBeenCalled();
  });
});

describe('dirty → validate → apply', () => {
  async function loaded(opts: Partial<Parameters<typeof useSectionEditor<Caches>>[0]> = {}) {
    http.on('GET', SECTION, json({ cache_size_mb: 100, log_level: 'info' }));
    const client = testQueryClient();
    const view = renderHookWithQuery(
      () =>
        useSectionEditor<Caches>({
          section: 'advanced',
          dirtyKey: 'advanced/caches',
          initial: { cache_size_mb: 100, log_level: 'info' },
          ...opts,
        }),
      client,
    );
    await waitFor(() => expect(view.result.current.loading).toBe(false));
    return view;
  }

  test('an edit is dirty under its own dirty key; discard reverts it', async () => {
    const { result } = await loaded();
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    expect(result.current.isDirty).toBe(true);
    expect(getDirtySections().has('advanced/caches')).toBe(true);
    expect(getDirtySections().has('advanced')).toBe(false);
    act(() => result.current.discard());
    expect(result.current.isDirty).toBe(false);
    expect(result.current.value.cache_size_mb).toBe(100);
    expect(getDirtySections().has('advanced/caches')).toBe(false);
  });

  test('the PUT sends the body that was validated, not later edits (§F5)', async () => {
    http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: {} }));
    http.on('PUT', SECTION, json({ ok: true, persisted_path: '/etc/dgp.yaml' }));
    const { result, client } = await loaded();
    const invalidate = vi.spyOn(client, 'invalidateQueries');

    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    await act(() => result.current.runApply());
    expect(http.callsTo('POST', `${SECTION}/validate`)[0].body).toEqual({ cache_size_mb: 256, log_level: 'info' });
    expect(result.current.applyOpen).toBe(true);
    expect(result.current.pendingBody).toEqual({ cache_size_mb: 256, log_level: 'info' });

    // The operator keeps typing under the dialog.
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 999 })));
    let ok = false;
    await act(async () => {
      ok = await result.current.confirmApply();
    });
    expect(ok).toBe(true);
    expect(http.callsTo('PUT', SECTION)[0].body).toEqual({ cache_size_mb: 256, log_level: 'info' });
    expect(result.current.applyOpen).toBe(false);
    expect(result.current.pendingBody).toBeNull();
    expect(invalidate).toHaveBeenCalledWith({ queryKey: qk.config() });
    expect(await screen.findByText('Applied + persisted to /etc/dgp.yaml')).toBeInTheDocument();
    // markApplied + refresh: the section is re-read and the panel is clean.
    await waitFor(() => expect(http.callsTo('GET', SECTION)).toHaveLength(2));
    await waitFor(() => expect(result.current.isDirty).toBe(false));
  });

  test('toPayload shapes the validated and PUT body', async () => {
    http.on('POST', `${SECTION}/validate`, json({ ok: true }));
    http.on('PUT', SECTION, json({ ok: true }));
    const { result } = await loaded({ toPayload: (v) => ({ cache_size_mb: v.cache_size_mb * 2 }) });
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 10 })));
    await act(() => result.current.runApply());
    await act(async () => {
      await result.current.confirmApply();
    });
    expect(http.callsTo('POST', `${SECTION}/validate`)[0].body).toEqual({ cache_size_mb: 20 });
    expect(http.callsTo('PUT', SECTION)[0].body).toEqual({ cache_size_mb: 20 });
  });

  test('a failed validate shows the error and opens no dialog', async () => {
    http.on('POST', `${SECTION}/validate`, json({ error: 'bad cache size' }, 400));
    const { result } = await loaded();
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: -1 })));
    await act(() => result.current.runApply());
    expect(result.current.applyOpen).toBe(false);
    expect(await screen.findByText(/Validate failed: .*bad cache size/)).toBeInTheDocument();
    expect(result.current.isDirty).toBe(true);
  });

  test('a PUT answered ok:false keeps the dialog open and the edits dirty', async () => {
    http.on('POST', `${SECTION}/validate`, json({ ok: true }));
    http.on('PUT', SECTION, json({ ok: false, error: 'engine rebuild failed' }));
    const { result } = await loaded();
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    await act(() => result.current.runApply());
    let ok = true;
    await act(async () => {
      ok = await result.current.confirmApply();
    });
    expect(ok).toBe(false);
    expect(result.current.applyOpen).toBe(true);
    expect(result.current.isDirty).toBe(true);
    expect(await screen.findByText('engine rebuild failed')).toBeInTheDocument();
  });

  test('a PUT that errors closes the dialog but never refreshes over the edits', async () => {
    http.on('POST', `${SECTION}/validate`, json({ ok: true }));
    http.on('PUT', SECTION, json({ error: 'boom' }, 500));
    const { result } = await loaded();
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    await act(() => result.current.runApply());
    let ok = true;
    await act(async () => {
      ok = await result.current.confirmApply();
    });
    expect(ok).toBe(false);
    expect(result.current.applyOpen).toBe(false);
    expect(result.current.value.cache_size_mb).toBe(256);
    expect(result.current.isDirty).toBe(true);
    expect(http.callsTo('GET', SECTION)).toHaveLength(1);
    expect(await screen.findByText(/Apply failed: .*boom/)).toBeInTheDocument();
  });

  test('confirmApply without a validated body does nothing', async () => {
    const { result } = await loaded();
    let ok = true;
    await act(async () => {
      ok = await result.current.confirmApply();
    });
    expect(ok).toBe(false);
    expect(http.callsTo('PUT', SECTION)).toHaveLength(0);
  });

  test('⌘S reaches the panel only while it is dirty', async () => {
    http.on('POST', `${SECTION}/validate`, json({ ok: true }));
    const { result } = await loaded();
    expect(requestApplyFirst(['advanced/caches'])).toBe(false);
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    await act(async () => {
      expect(requestApplyFirst(['advanced/caches'])).toBe(true);
    });
    await waitFor(() => expect(result.current.applyOpen).toBe(true));
    expect(http.callsTo('POST', `${SECTION}/validate`)).toHaveLength(1);
  });
});
