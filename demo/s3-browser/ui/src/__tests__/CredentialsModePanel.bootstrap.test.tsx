/**
 * Explore finding 11: the Credentials page showed an empty access key
 * field while a bootstrap key was configured (the section GET redacts it),
 * and its help said "to remove them, clear both fields", which the server
 * reads as "keep". The field now shows the configured key id, and removal
 * is an explicit action.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import CredentialsModePanel from '../components/CredentialsModePanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config/section/access', json({ iam_mode: 'gui' }));
  http.on('GET', '/_/api/admin/config', json({ access_key_id: 'AKIA-BOOTSTRAP', auth_enabled: true, env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('the access key field shows the configured bootstrap access key id', async () => {
  renderWithQuery(<CredentialsModePanel />);
  await waitFor(() => expect(screen.getByPlaceholderText('AKIAIOSFODNN7EXAMPLE')).toHaveValue('AKIA-BOOTSTRAP'));
  expect(screen.queryByText(/clear both fields/)).toBeNull();
});

test('a refusal (409: no IAM users yet) shows the server reason', async () => {
  http.on(
    'DELETE',
    '/_/api/admin/config/bootstrap-credentials',
    json({ error: 'no IAM users exist: removing the bootstrap SigV4 pair would leave the proxy without authentication' }, 409),
  );
  const user = userEvent.setup();
  renderWithQuery(<CredentialsModePanel />);
  await user.click(await screen.findByRole('button', { name: 'Remove bootstrap credentials' }));
  await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Remove' }));
  expect(await screen.findByText(/no IAM users exist/)).toBeInTheDocument();
});

test('"Remove bootstrap credentials" asks, then calls the removal endpoint', async () => {
  http.on(
    'DELETE',
    '/_/api/admin/config/bootstrap-credentials',
    json({ removed: true, warnings: ["the removed access key id is also IAM user 'legacy-admin'"] }),
  );
  const user = userEvent.setup();
  renderWithQuery(<CredentialsModePanel />);
  await user.click(await screen.findByRole('button', { name: 'Remove bootstrap credentials' }));
  const dialog = await screen.findByRole('dialog');
  await user.click(within(dialog).getByRole('button', { name: 'Remove' }));
  await waitFor(() => expect(http.callsTo('DELETE', '/_/api/admin/config/bootstrap-credentials')).toHaveLength(1));
  // The server's warning (the same key still signs as an IAM user) is shown.
  expect(await screen.findByText(/also IAM user 'legacy-admin'/)).toBeInTheDocument();
});
