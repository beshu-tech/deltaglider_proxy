/**
 * Lead follow-up to browser-review item 18: EVERY admin request, not only
 * the section editors, answers an expired session by asking to sign in again
 * (ReloginModal) and retrying the same request. A react-query CRUD mutation
 * used to fail with 401, and the panel's onSessionExpired signed the user out.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import ReloginModal from '../components/ReloginModal';
import { adminFetch, adminJson, deleteUser } from '../adminApi';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/whoami', json({ mode: 'bootstrap', user: null }));
  http.on('POST', '/_/api/admin/login', json({ ok: true }));
});
afterEach(() => vi.unstubAllGlobals());

async function signInThroughModal() {
  const user = userEvent.setup();
  const dialog = await screen.findByRole('dialog', { name: /Your session expired/ });
  await user.type(within(dialog).getByLabelText('Admin password'), 'testpass');
  await user.click(within(dialog).getByRole('button', { name: 'Sign in' }));
}

test('a 401 on a CRUD mutation asks to sign in, then sends the same request again', async () => {
  let deletes = 0;
  http.on('DELETE', '/_/api/admin/users/4', () => (++deletes === 1 ? json({ error: 'unauthorized' }, 401) : new Response(null, { status: 204 })));
  renderWithQuery(<ReloginModal />);
  let done: Promise<void> = Promise.resolve();
  act(() => { done = deleteUser(4); });
  await signInThroughModal();
  await act(() => done);
  expect(http.callsTo('DELETE', '/_/api/admin/users/4')).toHaveLength(2);
});

test('a 403 admin_session_required is an expired session too; the JSON body is re-sent', async () => {
  let puts = 0;
  http.on('PUT', '/_/api/admin/users/4', () => (++puts === 1 ? json({ error: 'admin_session_required' }, 403) : json({ id: 4 })));
  renderWithQuery(<ReloginModal />);
  let done: Promise<unknown> = Promise.resolve();
  act(() => { done = adminJson('/api/admin/users/4', { method: 'PUT', body: { name: 'dana' } }); });
  await signInThroughModal();
  expect(await done).toEqual({ id: 4 });
  expect(http.callsTo('PUT', '/_/api/admin/users/4').map((c) => c.body)).toEqual([{ name: 'dana' }, { name: 'dana' }]);
});

test('a cancelled sign-in lets the 401 through to the caller', async () => {
  http.on('GET', '/_/api/admin/users', json({ error: 'unauthorized' }, 401));
  const user = userEvent.setup();
  renderWithQuery(<ReloginModal />);
  let res: Promise<Response> = Promise.resolve(new Response());
  act(() => { res = adminFetch('/api/admin/users'); });
  const dialog = await screen.findByRole('dialog', { name: /Your session expired/ });
  await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
  expect((await res).status).toBe(401);
  expect(http.callsTo('GET', '/_/api/admin/users')).toHaveLength(1);
});

test('sign-in and session-probe endpoints never prompt', async () => {
  http.on('POST', '/_/api/admin/login-as', json({ error: 'bad' }, 401));
  http.on('GET', '/_/api/admin/session', json({ error: 'unauthorized' }, 401));
  http.on('POST', '/_/api/admin/recover-db', json({ error: 'wrong key' }, 401));
  renderWithQuery(<ReloginModal />);
  for (const [m, p] of [['POST', '/api/admin/login-as'], ['GET', '/api/admin/session'], ['POST', '/api/admin/recover-db']] as const) {
    expect((await adminFetch(p, m)).status).toBe(401);
  }
  await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
});

test('after a cancel, background reads stop asking; an action still asks', async () => {
  http.on('GET', '/_/api/admin/jobs', json({ error: 'unauthorized' }, 401));
  http.on('POST', '/_/api/admin/jobs/x/run-now', json({ error: 'unauthorized' }, 401));
  const user = userEvent.setup();
  renderWithQuery(<ReloginModal />);
  let first: Promise<Response> = Promise.resolve(new Response());
  act(() => { first = adminFetch('/api/admin/jobs/x/run-now', 'POST'); });
  await user.click(within(await screen.findByRole('dialog', { name: /Your session expired/ })).getByRole('button', { name: 'Cancel' }));
  expect((await first).status).toBe(401);
  await waitFor(() => expect(screen.queryByRole('dialog', { name: /Your session expired/ })).not.toBeInTheDocument());
  // A poll answers at once, with no dialog.
  expect((await adminFetch('/api/admin/jobs')).status).toBe(401);
  expect(screen.queryByRole('dialog', { name: /Your session expired/ })).not.toBeInTheDocument();
  // A button press asks again.
  act(() => { void adminFetch('/api/admin/jobs/x/run-now', 'POST'); });
  await signInThroughModal();
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/jobs/x/run-now')).toHaveLength(3));
});
