/**
 * Explore finding 12: while the per-IP limiter locks sign-in (429), the
 * forms showed "Login failed: Login failed", so an operator with the right
 * password kept retrying. They now name the lockout and the wait.
 */
import { act, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { AdminLoginGate } from '../components/admin/AdminLoginGate';
import ReloginModal from '../components/ReloginModal';
import { requestRelogin } from '../sessionRelogin';
import { mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

const lockedOut = () =>
  new Response(JSON.stringify({ ok: false }), {
    status: 429,
    headers: { 'content-type': 'application/json', 'retry-after': '540' },
  });

test('the admin password form says the sign-in is locked, and for how long', async () => {
  http.on('POST', '/_/api/admin/login', lockedOut);
  const user = userEvent.setup();
  renderWithQuery(
    <AdminLoginGate externalProviders={[]} s3BrowserSessionOnly={false} onAuthed={() => {}} onBack={() => {}} />,
  );
  await user.type(screen.getByPlaceholderText('Admin password'), 'right-password');
  await user.click(screen.getByRole('button', { name: /Sign in/i }));
  expect(await screen.findByText('Too many sign-in attempts. Try again in 9 min.')).toBeInTheDocument();
  expect(screen.queryByText(/Login failed/)).toBeNull();
});

test('the IAM re-login modal names the lockout too', async () => {
  http.on('GET', '/_/api/whoami', new Response(JSON.stringify({ mode: 'iam', user: null }), { status: 200 }));
  http.on('POST', '/_/api/admin/login-as', lockedOut);
  const user = userEvent.setup();
  renderWithQuery(<ReloginModal />);
  act(() => { void requestRelogin(); });
  const dialog = await screen.findByRole('dialog', { name: /Your session expired/ });
  await user.type(await within(dialog).findByLabelText('Access key ID'), 'AK');
  await user.type(within(dialog).getByLabelText('Secret access key'), 'SK');
  await user.click(within(dialog).getByRole('button', { name: 'Sign in' }));
  expect(await within(dialog).findByText(/Too many sign-in attempts\. Try again in 9 min\./)).toBeInTheDocument();
});
