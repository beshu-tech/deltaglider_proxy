// review4 frontend-4: a thrown connection test (network error, 5xx) must
// show an error, not stop the spinner silently with an unhandled rejection.
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import SetupWizard from '../components/SetupWizard';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ backends: [], default_backend: null, env_overrides: [] }));
  for (const s of ['admission', 'access', 'storage', 'advanced']) http.on('GET', `/_/api/admin/config/section/${s}`, json({}));
});
afterEach(() => vi.unstubAllGlobals());

test('a failed connection test shows its error', async () => {
  http.on('POST', '/_/api/admin/test-s3', json({ error: 'upstream exploded' }, 502));
  const user = userEvent.setup();
  renderWithQuery(<SetupWizard onComplete={() => {}} onCancel={() => {}} />);
  await user.click(await screen.findByText(/S3-compatible/, { selector: 'div' }));
  await user.click(screen.getByRole('button', { name: /Next/ }));
  await user.type(await screen.findByRole('textbox', { name: 'Access key ID' }), 'AKIAEXAMPLE');
  await user.type(screen.getByLabelText('Secret access key'), 'secret');
  await user.click(screen.getByRole('button', { name: /Test connection/ }));
  // D17: the plain sentence first; the server text stays behind Details.
  expect(await screen.findByText(/A gateway between you and the proxy dropped the request/)).toBeInTheDocument();
  expect(screen.getByText(/upstream exploded/)).toBeInTheDocument();
  // C46: Next stays usable; a click says why the step is not done and stays.
  await user.click(screen.getByRole('button', { name: /Next/ }));
  expect(await screen.findByText('Test the connection. Next needs a test that passes.')).toBeInTheDocument();
  expect(screen.getByText('Step 2 of 5')).toBeInTheDocument();
});

// C46: the admin step's Next marks the missing fields instead of being grey.
test('Next on an empty admin step marks each missing field', async () => {
  const user = userEvent.setup();
  renderWithQuery(<SetupWizard onComplete={() => {}} onCancel={() => {}} />);
  await user.click(await screen.findByRole('button', { name: /Next/ }));
  await user.type(screen.getByRole('textbox', { name: 'Data directory' }), '/var/lib/deltaglider');
  await user.click(screen.getByRole('button', { name: /Next/ }));
  expect(await screen.findByText('Step 3 of 5')).toBeInTheDocument();
  const next = screen.getByRole('button', { name: /Next/ });
  expect(next).toBeEnabled();
  await user.click(next);
  expect(await screen.findByText('Enter an access key ID.')).toBeInTheDocument();
  expect(screen.getByText('Enter a secret access key.')).toBeInTheDocument();
  expect(screen.getByRole('textbox', { name: 'Access key ID' })).toHaveAttribute('aria-invalid', 'true');
  expect(screen.getByText('Step 3 of 5')).toBeInTheDocument();
});
