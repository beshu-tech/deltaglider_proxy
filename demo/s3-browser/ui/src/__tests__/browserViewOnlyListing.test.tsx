/**
 * Cockroach scan 2026-10-10, UI finding J: App mounts the browser hook on
 * every view, so a tab left on Settings, Upload or the docs listed the last
 * bucket's root (up to 10 LIST pages) every 60 s. The hook now lists and
 * refreshes only while the browser view is shown.
 */
import { act, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const s3 = vi.hoisted(() => ({ lists: 0 }));

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    listObjects: async () => {
      s3.lists++;
      return { objects: [], folders: [], isTruncated: false };
    },
    listBuckets: async () => [{ name: 'releases', creationDate: '' }],
    headObject: async () => ({ storageType: 'passthrough', storedSize: 0 }),
  };
});

import Root from '../Root';
import ThemeProvider from '../ThemeProvider';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  s3.lists = 0;
  http = mockFetch();
  for (const m of ['GET', 'POST', 'PUT', 'DELETE']) http.on(m, /./, json({}, 404));
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://localhost:9000', region: 'us-east-1', bucket: 'releases', access_key_id: 'AK', secret_access_key: 'secret',
  }));
  http.on('GET', '/_/api/admin/session', json({ valid: true, admin_gui: true }));
  http.on('GET', '/_/api/whoami', json({ mode: 'bootstrap', user: { name: 'admin', is_admin: true, permissions: [] } }));
  Object.defineProperty(document, 'hidden', { configurable: true, value: false });
  Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' });
  // Only the intervals are fake, and they follow real time too (waitFor polls
  // with one); a test jumps a minute ahead with advanceTimersByTimeAsync.
  vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'], shouldAdvanceTime: true });
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

// A loaded machine (CI, parallel builds) takes seconds, not the default 1 s.
const SLOW = { timeout: 10_000 };

async function openAt(path: string) {
  window.history.replaceState(null, '', path);
  renderWithQuery(<ThemeProvider><Root /></ThemeProvider>);
  await waitFor(() => expect(http.callsTo('GET', '/_/api/whoami').length).toBeGreaterThan(0), SLOW);
}

/** One act per minute: two ticks inside one act would render (and list) once. */
async function twoMinutesPass() {
  for (let i = 0; i < 2; i++) {
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
  }
}

test.each(['/_/admin/users', '/_/upload', '/_/docs'])('%s sends no ListObjectsV2, also after two minutes', async (path) => {
  await openAt(path);
  await twoMinutesPass();
  expect(s3.lists).toBe(0);
});

// The control: the same count does see the browser's listing and its refresh.
test('the browser view lists the folder and refreshes it every 60 s', async () => {
  await openAt('/_/browse/releases/');
  await waitFor(() => expect(s3.lists).toBeGreaterThan(0), SLOW);
  const atStart = s3.lists;
  await twoMinutesPass();
  await waitFor(() => expect(s3.lists).toBe(atStart + 2), SLOW);
});
