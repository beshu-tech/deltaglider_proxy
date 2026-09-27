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
  expect(await screen.findByText(/upstream exploded/)).toBeInTheDocument();
  expect(screen.getByRole('button', { name: /Next/ })).toBeDisabled();
});
