// Clearing an OIDC provider's display name must send it cleared: an absent
// `display_name` means "keep" on the server (review4 frontend-2).
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import AuthenticationPanel from '../components/AuthenticationPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const PROVIDER = {
  id: 7,
  name: 'corp-sso',
  provider_type: 'oidc',
  enabled: true,
  priority: 0,
  display_name: 'Corp SSO',
  client_id: 'dgp',
  client_secret: '****',
  issuer_url: 'https://idp.acme.example',
  scopes: 'openid email profile',
  extra_config: {},
  created_at: '2026-09-01 00:00:00',
  updated_at: '2026-09-01 00:00:00',
};

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui', env_overrides: [] }));
  http.on('GET', '/_/api/admin/ext-auth/providers', json([PROVIDER]));
  http.on('GET', '/_/api/admin/ext-auth/mappings', json([]));
  http.on('GET', '/_/api/admin/ext-auth/identities', json([]));
  http.on('GET', '/_/api/admin/groups', json([]));
});
afterEach(() => vi.unstubAllGlobals());

test('clearing the display name sends it cleared', async () => {
  http.on('PUT', '/_/api/admin/ext-auth/providers/7', json({ ...PROVIDER, display_name: null }));
  const user = userEvent.setup();
  renderWithQuery(<AuthenticationPanel />);
  await user.click(await screen.findByText('Corp SSO'));
  const displayName = screen.getByDisplayValue('Corp SSO');
  await user.clear(displayName);
  await user.click(screen.getByRole('button', { name: 'Save' }));
  await waitFor(() => expect(http.callsTo('PUT', '/_/api/admin/ext-auth/providers/7')).toHaveLength(1));
  const body = http.callsTo('PUT', '/_/api/admin/ext-auth/providers/7')[0].body as Record<string, unknown>;
  expect(body.display_name).toBe('');
});

test('every provider form field is named by its label', async () => {
  const user = userEvent.setup();
  renderWithQuery(<AuthenticationPanel />);
  await user.click(await screen.findByText('Corp SSO'));
  for (const name of ['Display Name', 'Provider Name (unique identifier)', 'Issuer URL', 'Client ID', 'Scopes', 'CA certificate file']) {
    expect(screen.getByRole('textbox', { name })).toBeInTheDocument();
  }
  expect(screen.getByLabelText('Client Secret')).toBeInTheDocument();
  expect(screen.getByRole('switch', { name: 'Enabled' })).toBeInTheDocument();
});
