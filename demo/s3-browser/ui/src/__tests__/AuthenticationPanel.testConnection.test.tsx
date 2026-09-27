/**
 * H9: Test Connection tests the form as it is now (unsaved edits
 * included), also for a provider that is not saved yet, and shows a failed
 * test (a 200 with success: false) as the cause.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import AuthenticationPanel from '../components/AuthenticationPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const provider = {
  id: 7,
  name: 'corp',
  provider_type: 'oidc',
  enabled: true,
  priority: 0,
  display_name: 'Corp SSO',
  client_id: 'cid',
  client_secret: '****',
  issuer_url: 'https://idp.acme.example',
  scopes: 'openid email profile',
  extra_config: {},
  created_at: '',
  updated_at: '',
};

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui' }));
  http.on('GET', '/_/api/admin/ext-auth/providers', json([provider]));
  http.on('GET', '/_/api/admin/ext-auth/mappings', json([]));
  http.on('GET', '/_/api/admin/ext-auth/identities', json([]));
  http.on('GET', '/_/api/admin/groups', json([]));
});
afterEach(() => vi.unstubAllGlobals());

test('Test Connection sends the unsaved form of a saved provider', async () => {
  http.on(
    'POST',
    '/_/api/admin/ext-auth/providers/7/test',
    json({ success: false, error: 'OIDC discovery failed: HTTP 404' }),
  );
  const user = userEvent.setup();
  renderWithQuery(<AuthenticationPanel />);
  await user.click(await screen.findByText('Corp SSO'));
  const issuer = await screen.findByLabelText('Issuer URL');
  await user.clear(issuer);
  await user.type(issuer, 'https://new.acme.example');
  await user.click(screen.getByRole('button', { name: /Test Connection/ }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/ext-auth/providers/7/test')).toHaveLength(1));
  expect(http.callsTo('POST', '/_/api/admin/ext-auth/providers/7/test')[0].body).toEqual({
    client_id: 'cid',
    client_secret: '',
    issuer_url: 'https://new.acme.example',
    scopes: 'openid email profile',
    extra_config: {},
  });
  expect(await screen.findByText(/Failed: OIDC discovery failed: HTTP 404/)).toBeTruthy();
  expect(http.callsTo('PUT', '/_/api/admin/ext-auth/providers/7')).toHaveLength(0);
});

test('Test Connection works on the create form', async () => {
  http.on(
    'POST',
    '/_/api/admin/ext-auth/providers/test',
    json({ success: true, issuer: 'https://accounts.google.com' }),
  );
  const user = userEvent.setup();
  renderWithQuery(<AuthenticationPanel />);
  await user.click(await screen.findByRole('button', { name: /Add provider/ }));
  await user.type(await screen.findByLabelText('Client ID'), 'new-cid');
  await user.click(screen.getByRole('button', { name: /Test Connection/ }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/ext-auth/providers/test')).toHaveLength(1));
  expect(http.callsTo('POST', '/_/api/admin/ext-auth/providers/test')[0].body).toMatchObject({
    client_id: 'new-cid',
    issuer_url: 'https://accounts.google.com',
  });
  expect(await screen.findByText(/Connected\. Issuer: https:\/\/accounts\.google\.com/)).toBeTruthy();
});
