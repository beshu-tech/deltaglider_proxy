/**
 * Lead follow-up: in the object browser too, an expired admin session asks
 * to sign in again instead of signing the user out (an admin config
 * read got a 401, and the whole app went back to the sign-in screen).
 */
import { screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    listObjects: async () => ({ objects: [], folders: [], isTruncated: false }),
    listBuckets: async () => [{ name: 'releases', creationDate: '' }],
    headObject: async () => ({ storageType: 'passthrough', storedSize: 0 }),
  };
});

import Root from '../Root';
import ThemeProvider from '../ThemeProvider';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  window.history.replaceState(null, '', '/_/browse/releases/');
  http = mockFetch();
  for (const m of ['GET', 'POST', 'PUT', 'DELETE']) http.on(m, /./, json({}, 404));
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://localhost:9000', region: 'us-east-1', bucket: 'releases', access_key_id: 'AK', secret_access_key: 'secret',
  }));
  http.on('GET', '/_/api/admin/session', json({ valid: true, admin_gui: true }));
  http.on('GET', '/_/api/whoami', json({ mode: 'bootstrap', user: { name: 'admin', is_admin: true, permissions: [] } }));
  http.on('GET', '/_/api/admin/config', json({ error: 'unauthorized' }, 401));
  http.on('POST', '/_/api/admin/logout', new Response(null, { status: 204 }));
});
afterEach(() => vi.unstubAllGlobals());

test('a 401 from an admin read in the browser opens the sign-in dialog, not a sign-out', async () => {
  renderWithQuery(<ThemeProvider><Root /></ThemeProvider>);
  await waitFor(() => expect(http.callsTo('GET', '/_/api/admin/config').length).toBeGreaterThan(0));
  expect(await screen.findByRole('dialog', { name: /Your session expired/ })).toBeInTheDocument();
  expect(http.callsTo('POST', '/_/api/admin/logout')).toHaveLength(0);
});
