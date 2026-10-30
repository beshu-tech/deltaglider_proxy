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
});
afterEach(() => vi.unstubAllGlobals());

const lastDialog = async () => {
  const dialogs = await screen.findAllByRole('dialog');
  return dialogs[dialogs.length - 1];
};

test('lists the live sessions and labels the caller’s own', async () => {
  http.on('GET', LIST, json({ sessions: [session('s-mine', 'dana', true), session('s-ci', 'ci-uploader')] }));
  renderWithQuery(<SessionsPanel />);
  expect(await screen.findByText('2 live sessions')).toBeInTheDocument();
  expect(screen.getByText('you')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'More actions for session s-ci' })).toBeInTheDocument();
  // B32: every row keeps its menu, and the last column has a header.
  expect(screen.getByRole('button', { name: 'More actions for session s-mine' })).toBeInTheDocument();
  expect(screen.getByRole('columnheader', { name: 'Actions' })).toBeInTheDocument();
});

test('the caller’s own session cannot be revoked from the menu, and it says why', async () => {
  http.on('GET', LIST, json({ sessions: [session('s-mine', 'dana', true)] }));
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await user.click(await screen.findByRole('button', { name: 'More actions for session s-mine' }));
  const item = await screen.findByRole('menuitem', { name: /use Sign out in the account menu/ });
  expect(item).toHaveAttribute('aria-disabled', 'true');
});

test('no sessions reads as zero', async () => {
  http.on('GET', LIST, json({ sessions: [] }));
  renderWithQuery(<SessionsPanel />);
  expect(await screen.findByText('0 live sessions')).toBeInTheDocument();
});

test('a load failure is reported', async () => {
  http.on('GET', LIST, json({ error: 'boom' }, 500));
  renderWithQuery(<SessionsPanel />);
  expect(await screen.findByText(/boom/)).toBeInTheDocument();
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
  await user.click(await screen.findByRole('button', { name: 'More actions for session s-ci' }));
  await user.click(await screen.findByText('Sign out this session…'));
  await user.click(within(await lastDialog()).getByRole('button', { name: 'Sign out' }));
  await waitFor(() => expect(http.callsTo('DELETE', '/_/api/admin/sessions/s-ci')).toHaveLength(1));
  expect(await screen.findByText('1 live session')).toBeInTheDocument();
  expect(http.callsTo('GET', LIST)).toHaveLength(2);
});

test('revoke-user sends the identity after confirmation and refetches', async () => {
  http.on('GET', LIST, json({ sessions: [session('s-ci', 'ci-uploader')] }));
  http.on('POST', REVOKE_USER, json({
    revoked: 2, revoked_local: 2, persisted: true, pushed: true, propagation_bound_secs: 300,
  }));
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await screen.findByText('1 live session');
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
  http.on('GET', LIST, json({ sessions: [] }));
  http.on('POST', REVOKE_USER, json({
    revoked: 1, revoked_local: 1, persisted: false, pushed: false, propagation_bound_secs: null,
  }));
  const user = userEvent.setup();
  renderWithQuery(<SessionsPanel />);
  await screen.findByText('0 live sessions');
  await user.type(screen.getByRole('textbox', { name: 'Sign out every session of an identity' }), 'backup-bot');
  await user.click(screen.getByRole('button', { name: 'Revoke key' }));
  await user.click(within(await lastDialog()).getByRole('button', { name: 'Sign out all sessions' }));
  expect(await screen.findByText(/could NOT be persisted/)).toBeInTheDocument();
});
