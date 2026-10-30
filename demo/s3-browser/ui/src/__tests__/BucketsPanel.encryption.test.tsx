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
let ghostRemoved = false;
beforeEach(() => {
  applied = false;
  ghostRemoved = false;
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
    json({
      buckets: {
        ...(applied ? { 'db-archive': { backend: 'hetzner-fsn1', encryption: { mode: 'aes256-gcm-proxy', key_id: 'db-archive-2026-10' } } } : {}),
        // Settings with a key for a bucket that does not exist.
        ...(ghostRemoved ? {} : { ghost: { backend: 'hetzner-fsn1', encryption: { mode: 'aes256-gcm-proxy', key_id: 'ghost-2026-10' } } }),
      },
    }),
  );
  http.on('GET', '/_/api/admin/config/section/advanced', json({}));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', '/_/api/admin/encryption-witness', json(null));
  http.on('POST', '/_/api/admin/config/section/storage/validate', json({ ok: true, diff: { storage: { 'buckets.db-archive.encryption': { before: null, after: 'x' } } } }));
  http.on('PUT', SECTION, (req) => {
    const b = (req.body as { buckets: Record<string, unknown> }).buckets;
    if ('ghost' in b && b.ghost === null) ghostRemoved = true;
    else applied = true;
    return json({ ok: true });
  });
});
afterEach(() => vi.unstubAllGlobals());

async function expand(name: string) {
  const user = userEvent.setup();
  await user.click(await screen.findByRole('button', { name: new RegExp(`^${name} `) }));
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
  expect(await screen.findByText(/Apply or discard the unsaved changes of db-archive first/)).toBeInTheDocument();
  const group = screen.getByTestId('bucket-encryption-group');
  for (const r of within(group).getAllByRole('radio')) expect(r).toBeDisabled();
});

// UX harvest B26: a staged edit of ANOTHER bucket no longer disables the
// encryption group, and it survives the encryption change, which does not
// send back a stale copy of the changed bucket.
test("another bucket's staged edit does not block an encryption change, and survives it", async () => {
  renderWithQuery(<BucketsPanel />);
  const user = await expand('downloads');
  await user.click(await screen.findByText('Entire bucket public'));
  await user.click(screen.getByRole('button', { name: /^downloads / })); // collapse
  await user.click(screen.getByRole('button', { name: /^db-archive / }));
  const group = await screen.findByTestId('bucket-encryption-group');
  for (const r of within(group).getAllByRole('radio')) expect(r).toBeEnabled();

  await user.click(within(group).getByText('Encrypt with a bucket key'));
  const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
  await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
  await user.click(within(group).getByRole('button', { name: 'Review & apply' }));
  const dialog = await screen.findByRole('dialog');
  await user.click(within(dialog).getByRole('button', { name: /Apply/ }));
  await screen.findByText('Encrypt existing objects?');
  await waitFor(() => expect(http.callsTo('GET', SECTION).length).toBeGreaterThan(1));

  // The page's staged apply sends the downloads edit, and db-archive's
  // server copy keeps the backend that its new key needs. A stale copy of
  // db-archive (no settings) cleared the route (`backend: null`).
  await user.click(await screen.findByRole('button', { name: 'Review & apply' }));
  await waitFor(() => expect(http.callsTo('POST', `${SECTION}/validate`)).toHaveLength(2));
  const body = http.callsTo('POST', `${SECTION}/validate`)[1].body as { buckets: Record<string, { backend?: string | null; public_prefixes?: string[] | null }> };
  expect(body.buckets.downloads.public_prefixes).toEqual(['']);
  expect(body.buckets['db-archive'].backend).toBe('hetzner-fsn1');
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

/** Review P2, defect 4: an open editor cannot apply while staged edits wait. */
test('review & apply waits for staged edits', async () => {
  renderWithQuery(<BucketsPanel />);
  const user = await expand('db-archive');
  await user.click(await screen.findByText('Encrypt with a bucket key'));
  const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
  await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
  const review = screen.getByRole('button', { name: 'Review & apply' });
  await waitFor(() => expect(review).toBeEnabled());
  await user.click(screen.getByText('Entire bucket public'));
  await waitFor(() => expect(review).toBeDisabled());
});

/** Review P2, defect 2: settings with a key for a bucket that does not
 *  exist can be removed, at once, with a confirmation that says so; the
 *  name cannot change (a rename would move the key). */
test('settings with a key for a missing bucket can be removed', async () => {
  renderWithQuery(<BucketsPanel />);
  const user = await expand('ghost');
  expect(screen.queryByRole('combobox', { name: 'Bucket name' })).toBeNull();
  // A text button at the card's foot (UX harvest B25: not a one-item "⋯" menu).
  await user.click(screen.getByRole('button', { name: 'Remove these settings…' }));
  const dialogs = await screen.findAllByRole('dialog');
  const dialog = dialogs[dialogs.length - 1];
  expect(dialog).toHaveTextContent(/No bucket with this name exists, so no object uses its key/);
  await user.click(within(dialog).getByRole('button', { name: 'Remove settings and key' }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  const put = http.callsTo('PUT', SECTION)[0];
  expect(put.body).toEqual({ buckets: { ghost: null } });
  expect(put.headers['x-dgp-confirm-encryption-downgrade']).toBe('ghost');
  expect(put.headers['x-dgp-confirm-key-removal']).toBeUndefined();
});
