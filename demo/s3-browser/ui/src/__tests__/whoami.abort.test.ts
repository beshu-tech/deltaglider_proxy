// C45: a whoami that its caller cancels (the view went away) logs nothing.
import { afterEach, expect, test, vi } from 'vitest';
import { whoami } from '../adminApi/whoami';

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

test('a cancelled whoami is silent and falls back to no user', async () => {
  vi.stubGlobal('fetch', (_url: string, init: RequestInit) => new Promise((_, reject) => {
    init.signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
  }));
  const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
  const controller = new AbortController();
  const pending = whoami(controller.signal);
  controller.abort();
  await expect(pending).resolves.toEqual({ mode: 'bootstrap', user: null });
  expect(warn).not.toHaveBeenCalled();
});

test('a real network failure is still logged', async () => {
  vi.stubGlobal('fetch', () => Promise.reject(new TypeError('Failed to fetch')));
  const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
  await whoami();
  expect(warn).toHaveBeenCalledOnce();
});
