/**
 * useSectionEditor: the one apply protocol every config panel uses.
 * fetch → dirty → validate → ApplyDialog → PUT → markApplied, plus the
 * failure paths that must keep the operator's edits.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
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

describe('optimistic concurrency (browser review #4)', () => {
  function withEtag(body: unknown, etag: string, status = 200): Response {
    return new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json', etag },
    });
  }

  test('the PUT carries the loaded version as If-Match', async () => {
    http.on('GET', SECTION, withEtag({ cache_size_mb: 100 }, '"v1"'));
    http.on('POST', `${SECTION}/validate`, json({ ok: true }));
    http.on('PUT', SECTION, withEtag({ ok: true }, '"v2"'));
    const { result } = mountEditor();
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    await act(() => result.current.runApply());
    await act(async () => {
      await result.current.confirmApply();
    });
    expect(http.callsTo('PUT', SECTION)[0].headers['if-match']).toBe('"v1"');
  });

  test('a 409 keeps the edits and offers reload or review', async () => {
    http.on('GET', SECTION, withEtag({ cache_size_mb: 100 }, '"v1"'));
    http.on('POST', `${SECTION}/validate`, json({ ok: true }));
    http.on('PUT', SECTION, withEtag({ ok: false, error: 'config_conflict' }, '"v2"', 409));
    const { result } = mountEditor();
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    await act(() => result.current.runApply());
    let ok = true;
    await act(async () => {
      ok = await result.current.confirmApply();
    });
    expect(ok).toBe(false);
    // The dialog renders outside the hook's tree: await it, then act inside it.
    const dialog = await screen.findByRole('dialog', { name: /changed in another tab or by another admin/ });
    expect(result.current.isDirty).toBe(true);
    expect(result.current.value.cache_size_mb).toBe(256);

    // Review: the editor takes the new version, keeps the edits, and
    // validates again (the ApplyDialog then shows the diff from v2).
    http.on('GET', SECTION, withEtag({ cache_size_mb: 300 }, '"v2"'));
    http.on('PUT', SECTION, withEtag({ ok: true }, '"v3"'));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Review my edits against the new version' }));
    await waitFor(() => expect(result.current.applyOpen).toBe(true));
    expect(result.current.value.cache_size_mb).toBe(256);
    await act(async () => {
      await result.current.confirmApply();
    });
    expect(http.callsTo('PUT', SECTION)[1].headers['if-match']).toBe('"v2"');
  });

  test('a sibling editor of the section follows this tab\'s own apply', async () => {
    http.on('GET', SECTION, withEtag({ cache_size_mb: 100 }, '"v1"'));
    http.on('POST', `${SECTION}/validate`, json({ ok: true }));
    http.on('PUT', SECTION, withEtag({ ok: true }, '"v2"'));
    const a = mountEditor();
    const b = mountEditor({ dirtyKey: 'advanced/logging' });
    await waitFor(() => expect(a.result.current.loading || b.result.current.loading).toBe(false));
    act(() => a.result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    await act(() => a.result.current.runApply());
    await act(async () => {
      await a.result.current.confirmApply();
    });
    act(() => b.result.current.setValue((v) => ({ ...v, log_level: 'debug' })));
    await act(() => b.result.current.runApply());
    await act(async () => {
      await b.result.current.confirmApply();
    });
    const puts = http.callsTo('PUT', SECTION);
    expect(puts[puts.length - 1].headers['if-match']).toBe('"v2"');
  });

  test('reload discards the edits', async () => {
    http.on('GET', SECTION, withEtag({ cache_size_mb: 100 }, '"v1"'));
    http.on('POST', `${SECTION}/validate`, json({ ok: true }));
    http.on('PUT', SECTION, withEtag({ ok: false }, '"v2"', 409));
    const { result } = mountEditor();
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => result.current.setValue((v) => ({ ...v, cache_size_mb: 256 })));
    await act(() => result.current.runApply());
    await act(async () => {
      await result.current.confirmApply();
    });
    http.on('GET', SECTION, withEtag({ cache_size_mb: 300 }, '"v2"'));
    const dialog = await screen.findByRole('dialog', { name: /changed in another tab/ });
    await userEvent.click(within(dialog).getByRole('button', { name: 'Reload (discard my edits)' }));
    await waitFor(() => expect(result.current.value.cache_size_mb).toBe(300));
    expect(result.current.isDirty).toBe(false);
  });
});
