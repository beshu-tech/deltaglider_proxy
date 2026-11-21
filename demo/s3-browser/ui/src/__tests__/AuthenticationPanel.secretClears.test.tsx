/**
 * The server clears a stored client secret when the issuer URL or the network
 * policy changes and no new secret comes with it. The provider form says so
 * next to the issuer field before the save, and the note goes when the issuer
 * is back or a secret is typed.
 */
import { screen } from '@testing-library/react';
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

const NOTE = /clears the stored client secret/i;

beforeEach(() => {
  const http = mockFetch();
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

test('an untouched form shows no note', async () => {
  await openProvider();
  expect(screen.queryByText(NOTE)).not.toBeInTheDocument();
});

test('a moved issuer with a blank secret shows the note, a typed secret hides it', async () => {
  const user = await openProvider();
  const issuer = screen.getByDisplayValue('https://idp.acme.example');
  await user.clear(issuer);
  await user.type(issuer, 'https://login.other.example');
  expect((await screen.findAllByText(NOTE)).length).toBeGreaterThan(0);
  await user.type(screen.getByPlaceholderText('Unchanged'), 'typed-again');
  expect(screen.queryByText(NOTE)).not.toBeInTheDocument();
});
