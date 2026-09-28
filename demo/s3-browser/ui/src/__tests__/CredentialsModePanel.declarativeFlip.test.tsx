/**
 * The proxy refuses a gui → declarative flip whose YAML holds no IAM
 * (EMPTY_DECLARATIVE_FLIP in transition.rs), and the section PUT of this
 * page never sends users. So the option is disabled, with the reason and the
 * doc to follow, until the access section carries users or groups.
 */
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import CredentialsModePanel from '../components/CredentialsModePanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ access_key_id: 'AKIA-BOOTSTRAP', auth_enabled: true, env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('with no IAM in the YAML, Declarative is disabled and says why', async () => {
  http.on('GET', '/_/api/admin/config/section/access', json({ iam_mode: 'gui' }));
  renderWithQuery(<CredentialsModePanel />);
  const radio = await screen.findByRole('radio', { name: /Declarative/ });
  expect(radio).toBeDisabled();
  expect(screen.getByText(/access\.iam_users/)).toBeInTheDocument();
  expect(screen.getByRole('link', { name: 'How to manage IAM as code' })).toHaveAttribute(
    'href',
    '/_/docs/how-to-manage-iam-as-code',
  );
});

test('with users in the YAML, Declarative asks for confirmation', async () => {
  http.on(
    'GET',
    '/_/api/admin/config/section/access',
    json({ iam_mode: 'gui', iam_users: [{ name: 'ci-uploader', access_key_id: 'K', secret_access_key: 'S' }] }),
  );
  const user = userEvent.setup();
  renderWithQuery(<CredentialsModePanel />);
  const radio = await screen.findByRole('radio', { name: /Declarative/ });
  expect(radio).toBeEnabled();
  await user.click(radio);
  const dialog = await screen.findByRole('dialog');
  expect(within(dialog).getAllByText('Switch to Declarative IAM mode?').length).toBeGreaterThan(0);
});
