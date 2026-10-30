/**
 * The access-key sign-in form (harvest B5, B44, B45): the reveal control is
 * a real button, Sign in stays enabled and validates on submit, and a wrong
 * key says so in one sentence, with the request id kept out of it.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import ThemeProvider from '../ThemeProvider';

const testConnection = vi.fn();
vi.mock('../s3client', async (orig) => ({
  ...(await orig<typeof import('../s3client')>()),
  testConnection: (...a: unknown[]) => testConnection(...a),
}));

import ConnectPage from '../components/ConnectPage';

beforeEach(() => {
  const http = mockFetch();
  http.on('GET', '/_/api/whoami', json({ mode: 'iam', user: null }));
});
afterEach(() => vi.unstubAllGlobals());

function mount() {
  renderWithQuery(
    <ThemeProvider>
      <ConnectPage onConnect={() => {}} />
    </ThemeProvider>,
  );
}

test('the reveal control is a button that toggles the secret', async () => {
  const user = userEvent.setup();
  mount();
  const secret = await screen.findByLabelText('Secret access key');
  expect(secret).toHaveAttribute('type', 'password');
  const reveal = screen.getByRole('button', { name: 'Show' });
  await user.click(reveal);
  expect(secret).toHaveAttribute('type', 'text');
  expect(screen.getByRole('button', { name: 'Hide' })).toHaveAttribute('aria-pressed', 'true');
});

test('Sign in is enabled while empty and says what is missing', async () => {
  const user = userEvent.setup();
  mount();
  const signIn = await screen.findByRole('button', { name: 'Sign in' });
  expect(signIn).toBeEnabled();
  await user.click(signIn);
  expect(await screen.findByText('Enter the access key ID and the secret access key.')).toBeInTheDocument();
  expect(testConnection).not.toHaveBeenCalled();
});

test('a wrong key is one plain sentence; the request id is only in the hover text', async () => {
  testConnection.mockResolvedValue({
    ok: false,
    error: 'The access key ID or the secret access key is wrong.',
    requestId: '4657c220-aaaa',
  });
  const user = userEvent.setup();
  mount();
  await user.type(await screen.findByLabelText('Access key ID'), 'AKIA');
  await user.type(screen.getByLabelText('Secret access key'), 'nope');
  await user.click(screen.getByRole('button', { name: 'Sign in' }));
  const text = await screen.findByText('The access key ID or the secret access key is wrong.');
  expect(text).toHaveAttribute('title', 'Request ID: 4657c220-aaaa');
  expect(screen.queryByText(/request-id|403|Connection failed/)).toBeNull();
});
