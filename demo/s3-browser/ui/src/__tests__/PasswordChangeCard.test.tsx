// A refused password change shows the server's reason and stops the spinner.
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import PasswordChangeCard from '../components/PasswordChangeCard';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

async function submit() {
  const user = userEvent.setup();
  await user.type(screen.getByLabelText('Current admin password'), 'old');
  await user.type(screen.getByLabelText('New admin password'), 'new');
  await user.click(screen.getByRole('button', { name: /Change admin password/ }));
}

test('a 409 shows the server sentence and the button is usable again', async () => {
  http.on('PUT', '/_/api/admin/password', json({ ok: false, error: 'A change here would be lost at the next start.' }, 409));
  renderWithQuery(<PasswordChangeCard />);
  await submit();
  expect(await screen.findByText(/would be lost at the next start/)).toBeTruthy();
  expect(screen.getByRole('button', { name: /Change admin password/ }).className).not.toContain('ant-btn-loading');
});

test('a failure without a result body shows the error too', async () => {
  http.on('PUT', '/_/api/admin/password', new Response('gateway down', { status: 502 }));
  renderWithQuery(<PasswordChangeCard />);
  await submit();
  expect(await screen.findByText(/gateway down/)).toBeTruthy();
});
