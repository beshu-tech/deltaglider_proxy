/**
 * Sign-out flow, end to end through <App>. Replaces the old source-grep
 * (logout-order): the server-side S3 credential clear goes out BEFORE the
 * logout, and no session-bound request follows it (the session is gone, so
 * any later admin request is a 401 the browser logs as a failed resource).
 *
 * S3 data calls go through the AWS SDK, so those s3client functions are
 * stubbed; session, identity and logout requests are real fetches.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
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
import { hasCredentials } from '../s3client';

const CREDS = '/_/api/admin/session/s3-credentials';
const LOGOUT = '/_/api/admin/logout';
/** Endpoints that answer without a session (src/api/admin/auth.rs whoami). */
const PUBLIC = new Set(['/_/api/whoami']);

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  window.history.replaceState(null, '', '/_/browse/releases/');
  http = mockFetch();
  // Anything the shell reads that this test does not care about.
  for (const m of ['GET', 'POST', 'PUT', 'DELETE']) http.on(m, /./, json({}, 404));
  http.on('GET', CREDS, json({
    endpoint: 'http://localhost:9000',
    region: 'us-east-1',
    bucket: 'releases',
    access_key_id: 'AKIA-CI-UPLOADER',
    secret_access_key: 'secret',
  }));
  http.on('DELETE', CREDS, new Response(null, { status: 204 }));
  http.on('GET', '/_/api/admin/session', json({ valid: true, admin_gui: false }));
  http.on('GET', '/_/api/whoami', json({ mode: 'open', user: null }));
  http.on('POST', LOGOUT, new Response(null, { status: 204 }));
});
afterEach(() => {
  vi.unstubAllGlobals();
});

test('sign-out clears the server credentials, then logs out, then sends no session request', async () => {
  const user = userEvent.setup();
  renderWithQuery(
    <ThemeProvider>
      <Root />
    </ThemeProvider>,
  );
  const trigger = await screen.findByRole('button', { name: /^Account menu/ });
  expect(hasCredentials()).toBe(true);

  await user.click(trigger);
  await user.click(await screen.findByRole('menuitem', { name: 'Sign out' }));
  // The confirmation is a dialog (confirmDialog), not window.confirm.
  await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Sign out' }));
  await waitFor(() => expect(http.callsTo('POST', LOGOUT)).toHaveLength(1));

  const clearAt = http.calls.findIndex((c) => c.method === 'DELETE' && c.path === CREDS);
  const logoutAt = http.calls.findIndex((c) => c.method === 'POST' && c.path === LOGOUT);
  expect(clearAt).toBeGreaterThanOrEqual(0);
  expect(clearAt).toBeLessThan(logoutAt);

  // Back on the sign-in screen, with the local credentials gone...
  await waitFor(() => expect(hasCredentials()).toBe(false));
  // ...and give any stray effect a chance to fire before checking silence.
  await new Promise((r) => setTimeout(r, 300));
  // The sign-in screen may read the public whoami (mode, OAuth buttons);
  // nothing that needs the ended session may follow the logout.
  const after = http.calls.slice(logoutAt + 1).filter((c) => !PUBLIC.has(c.path));
  expect(after).toEqual([]);
});
