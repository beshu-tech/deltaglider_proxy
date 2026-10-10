/**
 * A session that expires while the page sits open (overnight: the TTL is 4 h)
 * must send the user to sign in again. The page used to stay on screen and
 * keep listing files (the S3 keys live in memory), but the bulk actions, the
 * folder sizes and the admin reads disappeared without a word.
 */
import { act, screen, waitFor } from '@testing-library/react';
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
let sessionValid: boolean;
beforeEach(() => {
  window.history.replaceState(null, '', '/_/browse/releases/');
  sessionValid = true;
  http = mockFetch();
  for (const m of ['GET', 'POST', 'PUT', 'DELETE']) http.on(m, /./, json({}, 404));
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://localhost:9000', region: 'us-east-1', bucket: 'releases', access_key_id: 'AK', secret_access_key: 'secret',
  }));
  http.on('GET', '/_/api/admin/session', () => json({ valid: sessionValid, admin_gui: sessionValid }));
  http.on('GET', '/_/api/whoami', json({ mode: 'bootstrap', user: { name: 'admin', is_admin: true, permissions: [] } }));
  http.on('POST', '/_/api/admin/logout', new Response(null, { status: 204 }));
  Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' });
});
afterEach(() => vi.unstubAllGlobals());

test('a session found expired when the tab comes back sends the user to sign in, and says why', async () => {
  renderWithQuery(<ThemeProvider><Root /></ThemeProvider>);
  await screen.findByRole('navigation', { name: 'Breadcrumb' });
  await waitFor(() => expect(http.callsTo('GET', '/_/api/whoami').length).toBeGreaterThan(0));

  sessionValid = false;
  act(() => {
    document.dispatchEvent(new Event('visibilitychange'));
  });

  expect(await screen.findByText(/Your session expired/)).toBeInTheDocument();
  expect(await screen.findByPlaceholderText('Admin password')).toBeInTheDocument();
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/logout')).toHaveLength(1));
});
