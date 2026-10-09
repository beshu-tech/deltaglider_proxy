/**
 * Review A14: with no IAM user, no bootstrap pair and no
 * `authentication: none`, S3 refuses every request. The connect page then
 * offers the admin password (the only way in) and says why, instead of the
 * access-key form nobody can fill.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import ConnectPage from '../components/ConnectPage';
import ThemeProvider from '../ThemeProvider';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/whoami', json({ mode: 'deny_all', user: null }));
  http.on('POST', '/_/api/admin/login', json({ ok: true }));
});
afterEach(() => {
  vi.unstubAllGlobals();
});

test('deny-all offers the admin password and says why S3 refuses', async () => {
  const user = userEvent.setup();
  renderWithQuery(
    <ThemeProvider>
      <ConnectPage onConnect={() => {}} />
    </ThemeProvider>,
  );
  const password = await screen.findByLabelText('Admin password');
  expect(screen.queryByLabelText('Access key ID')).toBeNull();
  expect(screen.getByText(/refuses every S3 request/)).toBeTruthy();

  await user.type(password, 'pw');
  await user.keyboard('{Enter}');
  await waitFor(() =>
    expect(http.calls.some((c) => c.method === 'POST' && c.path === '/_/api/admin/login')).toBe(true),
  );
});
