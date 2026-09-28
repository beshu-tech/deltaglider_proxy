/**
 * "Add Rule" used to POST an empty email_glob rule at once, so a page left
 * without saving kept an empty rule on the server. Now the new rule is a
 * local draft: nothing is sent until Save Rules, and Save Rules refuses a
 * draft with no pattern.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import AuthenticationPanel from '../components/AuthenticationPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const provider = {
  id: 7, name: 'corp', provider_type: 'oidc', enabled: true, priority: 0, display_name: 'Corp SSO',
  client_id: 'cid', client_secret: '****', issuer_url: 'https://idp.acme.example', scopes: 'openid email',
  extra_config: {}, created_at: '', updated_at: '',
};
const engineering = { id: 3, name: 'Engineering', description: '', permissions: [], member_ids: [], created_at: '', updated_at: '' };

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui' }));
  http.on('GET', '/_/api/admin/ext-auth/providers', json([provider]));
  http.on('GET', '/_/api/admin/ext-auth/mappings', json([]));
  http.on('GET', '/_/api/admin/ext-auth/identities', json([]));
  http.on('GET', '/_/api/admin/groups', json([engineering]));
});
afterEach(() => vi.unstubAllGlobals());

test('Add Rule creates nothing on the server until Save Rules', async () => {
  http.on('POST', '/_/api/admin/ext-auth/mappings', json({ id: 11, provider_id: null, priority: 0, match_type: 'email_glob', match_field: 'email', match_value: '*@acme.example', group_id: 3, created_at: '' }));
  const user = userEvent.setup();
  renderWithQuery(<AuthenticationPanel />);
  await user.click(await screen.findByRole('button', { name: /Add Rule/ }));
  const pattern = await screen.findByRole('textbox', { name: /Match value|Pattern/i });
  expect(http.callsTo('POST', '/_/api/admin/ext-auth/mappings')).toHaveLength(0);

  // An empty draft is refused, not saved.
  await user.click(screen.getByRole('button', { name: 'Save Rules' }));
  expect(http.callsTo('POST', '/_/api/admin/ext-auth/mappings')).toHaveLength(0);

  await user.type(pattern, '*@acme.example');
  await user.click(screen.getByRole('button', { name: 'Save Rules' }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/ext-auth/mappings')).toHaveLength(1));
  expect(http.callsTo('POST', '/_/api/admin/ext-auth/mappings')[0].body).toMatchObject({
    match_type: 'email_glob',
    match_value: '*@acme.example',
    group_id: 3,
  });
});
