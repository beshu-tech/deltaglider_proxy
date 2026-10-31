/**
 * SessionsPanel lists live sessions and force-logs them out: one session
 * (DELETE /sessions/:id) or every session of an identity (revoke-user).
 * Each revoke refetches the list.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import SessionsPanel from '../components/SessionsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const LIST = '/_/api/admin/sessions';
const REVOKE_USER = '/_/api/admin/sessions/revoke-user';
const session = (id: string, identity: string | null, current = false) => ({
  id, auth: 'iam', admin_gui: true, identity, ip: '10.0.0.7', age_secs: 90, current,
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ config_sync_bucket: 'dgp-sync' }));
});
afterEach(() => vi.unstubAllGlobals());

const lastDialog = async () => {
  const dialogs = await screen.findAllByRole('dialog');
  return dialogs[dialogs.length - 1];
};

test('lists the live sessions grouped by principal and labels the caller’s own', async () => {
  http.on('GET', LIST, json({ sessions: [session('s-mine', 'dana', true), session('s-ci', 'ci-uploader')] }));
  renderWithQuery(<SessionsPanel />);
  expect(await screen.findByText('2 live sessions · 2 signed-in principals')).toBeInTheDocument();
  expect(screen.getByRole('region', { name: 'Sessions of dana' })).toHaveTextContent('This browser');
  // D55: a direct "Sign out" button, not a one-item menu.
  expect(screen.getByRole('button', { name: 'Sign out session s-ci' })).toHaveTextContent('Sign out');
  // The caller's own session has no sign-out button here (the server refuses it).
  expect(screen.queryByRole('button', { name: 'Sign out session s-mine' })).toBeNull();
});

// D55: 22 sign-ins of one principal are one group, not 22 rows.
test('sessions of one principal form one group with one sign-out for the others', async () => {
  let rows = [
    { ...session('s-mine', null, true), auth: 'bootstrap' },
    ...Array.from({ length: 5 }, (_, i) => ({ ...session(`s-b${i}`, null), auth: 'bootstrap', age_secs: 100 + i })),
  ];
  http.on('GET', LIST, () => json({ sessions: rows }));
  http.on('DELETE', /\/_\/api\/admin\/sessions\/s-b\d$/, (req) => {
    rows = rows.filter((r) => !req.path.endsWith(r.id));
    return new Response(null, { status: 204 });
  });
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  const group = await screen.findByRole('region', { name: 'Sessions of Bootstrap admin' });
  expect(screen.getAllByRole('region')).toHaveLength(1);
  expect(within(group).getAllByRole('listitem')).toHaveLength(3);
  await user.click(within(group).getByRole('button', { name: 'Show 3 more' }));
  expect(within(group).getAllByRole('listitem')).toHaveLength(6);
  await user.click(within(group).getByRole('button', { name: /Sign out 5 others/ }));
  const dialog = await lastDialog();
  expect(dialog).toHaveTextContent('Sign out your 5 other sessions?');
  await user.click(within(dialog).getByRole('button', { name: 'Sign out' }));
  await waitFor(() => expect(http.calls.filter((c) => c.method === 'DELETE')).toHaveLength(5));
  expect(http.calls.some((c) => c.method === 'DELETE' && c.path.endsWith('s-mine'))).toBe(false);
  expect(await screen.findByText('1 live session · 1 signed-in principal')).toBeInTheDocument();
});

// D71: sign-out is neutral, as in the account menu.
test('the sign-out confirm is not a danger button', async () => {
  http.on('GET', LIST, json({ sessions: [session('s-ci', 'ci-uploader')] }));
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await user.click(await screen.findByRole('button', { name: 'Sign out session s-ci' }));
  const ok = within(await lastDialog()).getByRole('button', { name: 'Sign out' });
  expect(ok.className).not.toMatch(/dangerous/);
});

test('no sessions reads as zero', async () => {
  http.on('GET', LIST, json({ sessions: [] }));
  renderWithQuery(<SessionsPanel />);
  expect(await screen.findByText('0 live sessions')).toBeInTheDocument();
  expect(screen.getByText('Nobody is signed in')).toBeInTheDocument();
});

test('a load failure is one error state with Retry, not raw text', async () => {
  http.on('GET', LIST, json({ error: 'boom' }, 500));
  renderWithQuery(<SessionsPanel />);
  expect(await screen.findByText('Could not load the sessions')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: /Retry/ })).toBeInTheDocument();
});

test('an expired session sends the user to sign in', async () => {
  http.on('GET', LIST, json({ error: 'unauthorized' }, 401));
  const onSessionExpired = vi.fn();
  renderWithQuery(<SessionsPanel onSessionExpired={onSessionExpired} />);
  await waitFor(() => expect(onSessionExpired).toHaveBeenCalled());
});

test('force-logout of one session deletes it and refetches the list', async () => {
  let rows = [session('s-mine', 'dana', true), session('s-ci', 'ci-uploader')];
  http.on('GET', LIST, () => json({ sessions: rows }));
  http.on('DELETE', '/_/api/admin/sessions/s-ci', () => {
    rows = rows.filter((r) => r.id !== 's-ci');
    return new Response(null, { status: 204 });
  });
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await user.click(await screen.findByRole('button', { name: 'Sign out session s-ci' }));
  await user.click(within(await lastDialog()).getByRole('button', { name: 'Sign out' }));
  await waitFor(() => expect(http.callsTo('DELETE', '/_/api/admin/sessions/s-ci')).toHaveLength(1));
  expect(await screen.findByText('1 live session · 1 signed-in principal')).toBeInTheDocument();
  expect(http.callsTo('GET', LIST)).toHaveLength(2);
});

test('revoke-user sends the identity after confirmation and refetches', async () => {
  http.on('GET', LIST, json({ sessions: [session('s-ci', 'ci-uploader')] }));
  http.on('POST', REVOKE_USER, json({
    revoked: 2, revoked_local: 2, persisted: true, pushed: true, propagation_bound_secs: 300,
  }));
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await screen.findByText(/^1 live session/);
  await user.type(screen.getByRole('textbox', { name: 'Sign out every session of an identity' }), ' ci-uploader ');
  await user.click(screen.getByRole('button', { name: 'Revoke key' }));
  await user.click(within(await lastDialog()).getByRole('button', { name: 'Sign out all sessions' }));
  await waitFor(() => expect(http.callsTo('POST', REVOKE_USER)).toHaveLength(1));
  expect(http.callsTo('POST', REVOKE_USER)[0].body).toEqual({ identity: 'ci-uploader' });
  expect(await screen.findByText(/Revoked 2 sessions locally; peers converge within ~5 min/)).toBeInTheDocument();
  await waitFor(() => expect(http.callsTo('GET', LIST)).toHaveLength(2));
  expect(screen.getByRole('textbox', { name: 'Sign out every session of an identity' })).toHaveValue('');
});

test('revoke-user warns when the revocation was not persisted', async () => {
  http.on('GET', LIST, json({ sessions: [session('s-bb', 'backup-bot')] }));
  http.on('POST', REVOKE_USER, json({
    revoked: 1, revoked_local: 1, persisted: false, pushed: false, propagation_bound_secs: null,
  }));
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await screen.findByText(/^1 live session/);
  await user.type(screen.getByRole('textbox', { name: 'Sign out every session of an identity' }), 'backup-bot');
  await user.click(screen.getByRole('button', { name: 'Revoke key' }));
  await user.click(within(await lastDialog()).getByRole('button', { name: 'Sign out all sessions' }));
  expect(await screen.findByText(/could NOT be persisted/)).toBeInTheDocument();
});

// C39: an identity that no listed session uses is said so before anything is sent.
test('revoke-user of an unknown identity says no live session here uses it', async () => {
  http.on('GET', LIST, json({ sessions: [session('s-ci', 'ci-uploader')] }));
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await screen.findByText(/^1 live session/);
  await user.type(screen.getByRole('textbox', { name: 'Sign out every session of an identity' }), 'nonexistent-key');
  await user.click(screen.getByRole('button', { name: 'Revoke key' }));
  const dialog = await lastDialog();
  expect(dialog).toHaveTextContent('No live session on this instance uses "nonexistent-key"');
  // The remote sign-out is the secondary action, not a call to action.
  expect(within(dialog).getByRole('button', { name: 'Sign out on other instances' }).className).not.toMatch(/btn-primary/);
  await user.click(within(dialog).getByRole('button', { name: 'Close' }));
  expect(http.callsTo('POST', REVOKE_USER)).toHaveLength(0);
});

// D11: one instance (no sync bucket) has no other instances to sign out on.
test('revoke-user of an unknown identity on a single instance offers only Close', async () => {
  http.on('GET', '/_/api/admin/config', json({ config_sync_bucket: null }));
  http.on('GET', LIST, json({ sessions: [session('s-ci', 'ci-uploader')] }));
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await screen.findByText(/^1 live session/);
  await waitFor(() => expect(http.callsTo('GET', '/_/api/admin/config')).toHaveLength(1));
  await user.type(screen.getByRole('textbox', { name: 'Sign out every session of an identity' }), 'nonexistent-key');
  await user.click(screen.getByRole('button', { name: 'Revoke key' }));
  const dialog = await lastDialog();
  expect(dialog).toHaveTextContent('No live session uses "nonexistent-key"');
  expect(within(dialog).queryByRole('button', { name: /other instances/ })).toBeNull();
  expect(within(dialog).getAllByRole('button').map((b) => b.textContent)).toEqual(['Close']);
});

// C40: a session with no identity (bootstrap password) is named by its role.
test('a session without an identity is named by how it signed in', async () => {
  http.on('GET', LIST, json({ sessions: [{ ...session('s-boot', null), auth: 'open' }] }));
  renderWithQuery(<SessionsPanel />);
  await screen.findByText(/^1 live session/);
  expect(screen.getByRole('region', { name: 'Sessions of Anonymous (open access)' })).toBeInTheDocument();
});
