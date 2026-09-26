/**
 * The OIDC provider form exposes the provider's network policy:
 * `extra_config.allow_local` (http:// and private addresses) and
 * `extra_config.ca_cert_path` (a private CA). Other `extra_config` keys
 * survive a save, and the server's save-time 422 reason (for example an
 * issuer URL that the policy refuses) shows inline in the form.
 */
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
  extra_config: { hd: 'acme.example' },
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

async function openProvider() {
  const user = userEvent.setup();
  renderWithQuery(<AuthenticationPanel />);
  await user.click(await screen.findByText('Corp SSO'));
  return user;
}

test('the form shows the network policy with the SSRF caveat', async () => {
  await openProvider();
  expect(screen.getByRole('switch', { name: /Allow http:\/\/ and private addresses/ })).not.toBeChecked();
  expect(screen.getByText(/cloud metadata addresses stay refused/i)).toBeInTheDocument();
  expect(screen.getByLabelText('CA certificate file')).toHaveValue('');
});

test('save sends allow_local and ca_cert_path in extra_config and keeps the other keys', async () => {
  http.on('PUT', '/_/api/admin/ext-auth/providers/7', json(PROVIDER));
  const user = await openProvider();
  await user.click(screen.getByRole('switch', { name: /Allow http:\/\/ and private addresses/ }));
  await user.type(screen.getByLabelText('CA certificate file'), '/etc/dgp/idp-ca.pem');
  await user.click(screen.getByRole('button', { name: 'Save' }));
  await waitFor(() => expect(http.callsTo('PUT', '/_/api/admin/ext-auth/providers/7')).toHaveLength(1));
  const body = http.callsTo('PUT', '/_/api/admin/ext-auth/providers/7')[0].body as { extra_config: unknown };
  expect(body.extra_config).toEqual({ hd: 'acme.example', allow_local: true, ca_cert_path: '/etc/dgp/idp-ca.pem' });
});

test('turning the policy off removes the keys', async () => {
  http.on('GET', '/_/api/admin/ext-auth/providers', json([
    { ...PROVIDER, extra_config: { allow_local: true, ca_cert_path: '/etc/dgp/idp-ca.pem' } },
  ]));
  http.on('PUT', '/_/api/admin/ext-auth/providers/7', json(PROVIDER));
  const user = await openProvider();
  const toggle = screen.getByRole('switch', { name: /Allow http:\/\/ and private addresses/ });
  expect(toggle).toBeChecked();
  await user.click(toggle);
  await user.clear(screen.getByLabelText('CA certificate file'));
  await user.click(screen.getByRole('button', { name: 'Save' }));
  await waitFor(() => expect(http.callsTo('PUT', '/_/api/admin/ext-auth/providers/7')).toHaveLength(1));
  const body = http.callsTo('PUT', '/_/api/admin/ext-auth/providers/7')[0].body as { extra_config: unknown };
  expect(body.extra_config).toEqual({});
});

test('a 422 from save shows the server reason inline', async () => {
  const reason =
    "issuer_url 'http://10.0.0.5': private address. For an identity provider on http:// or a private address, set extra_config.allow_local: true";
  http.on('PUT', '/_/api/admin/ext-auth/providers/7', json({ error: reason }, 422));
  const user = await openProvider();
  await user.click(screen.getByRole('button', { name: 'Save' }));
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('The provider was not saved');
  expect(alert).toHaveTextContent(reason);
});
