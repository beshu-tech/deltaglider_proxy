/**
 * Issue #116 P2: the Buckets page shows each bucket's encryption from the
 * server (`effective_encryption`), with an Encryption group between Public
 * access and Placement. An encryption change applies at once, so it waits
 * while the page has unsaved staged edits; after it, the page offers the
 * re-encrypt job for that bucket.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BucketsPanel from '../components/BucketsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff, eff } from '../test/encryption';

const SECTION = '/_/api/admin/config/section/storage';
const BACKEND = {
  name: 'hetzner-fsn1', backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null,
  has_credentials: true, encryption: { mode: 'none', has_key: false, shim_active: false },
};

let http: ReturnType<typeof mockFetch>;
let applied = false;
beforeEach(() => {
  applied = false;
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75, backends: [BACKEND], default_backend: 'hetzner-fsn1' }));
  http.on('GET', '/_/api/admin/backends', json({ backends: [BACKEND], default_backend: 'hetzner-fsn1' }));
  http.on('GET', '/_/api/admin/buckets', () =>
    json({
      buckets: [
        { name: 'db-archive', backend_name: 'hetzner-fsn1', effective_encryption: applied ? bucketKeyEff() : eff() },
        { name: 'downloads', backend_name: 'hetzner-fsn1', effective_encryption: eff() },
      ],
    }),
  );
  http.on('GET', SECTION, () =>
    json(applied ? { buckets: { 'db-archive': { backend: 'hetzner-fsn1', encryption: { mode: 'aes256-gcm-proxy', key_id: 'db-archive-2026-10' } } } } : { buckets: {} }),
  );
  http.on('GET', '/_/api/admin/config/section/advanced', json({}));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('POST', '/_/api/admin/config/section/storage/validate', json({ ok: true, diff: { storage: { 'buckets.db-archive.encryption': { before: null, after: 'x' } } } }));
  http.on('PUT', SECTION, () => {
    applied = true;
    return json({ ok: true });
  });
});
afterEach(() => vi.unstubAllGlobals());

async function expand(name: string) {
  const user = userEvent.setup();
  await user.click(await screen.findByRole('button', { name: new RegExp(`^${name} — click to edit`) }));
  return user;
}

test('the Encryption group sits between Public access and Placement', async () => {
  renderWithQuery(<BucketsPanel />);
  await expand('db-archive');
  const group = await screen.findByTestId('bucket-encryption-group');
  const order = [screen.getByText('Public access'), group, screen.getByText('Backend')];
  for (let i = 1; i < order.length; i++) {
    expect(order[i - 1].compareDocumentPosition(order[i]) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  }
});

test('unsaved staged edits block the immediate encryption change', async () => {
  renderWithQuery(<BucketsPanel />);
  const user = await expand('db-archive');
  await user.click(await screen.findByText('Entire bucket public'));
  expect(await screen.findByText(/Apply or discard the unsaved bucket changes first/)).toBeInTheDocument();
  const group = screen.getByTestId('bucket-encryption-group');
  for (const r of within(group).getAllByRole('radio')) expect(r).toBeDisabled();
});

test('after enabling a bucket key, the page offers the re-encrypt job for that bucket', async () => {
  renderWithQuery(<BucketsPanel />);
  const user = await expand('db-archive');
  await user.click(await screen.findByText('Encrypt with a bucket key'));
  const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
  await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
  await user.click(screen.getByRole('button', { name: 'Review & apply' }));
  const dialog = await screen.findByRole('dialog');
  await user.click(within(dialog).getByRole('button', { name: /Apply/ }));
  const proposal = await screen.findByText('Encrypt existing objects?');
  const modal = proposal.closest('.ant-modal') as HTMLElement;
  expect(within(modal).getByRole('checkbox', { name: 'db-archive' })).toBeChecked();
  expect(within(modal).queryByRole('checkbox', { name: 'downloads' })).toBeNull();
  // The card shows the server's new answer.
  await waitFor(() => expect(screen.getAllByText('Encrypted · bucket key db-archive-2026-10').length).toBeGreaterThan(0));
});
