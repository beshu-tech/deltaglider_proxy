/**
 * Issue #116 P2: the Encryption group of the bucket card. It shows the
 * server's `effective_encryption`, offers the modes that the server accepts
 * on a bucket, and applies one bucket merge-patch with the confirmation
 * headers that the server needs.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BucketEncryptionGroup from '../components/BucketEncryptionGroup';
import type { EffectiveEncryption } from '../adminApi';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff, eff } from '../test/encryption';

const SECTION = '/_/api/admin/config/section/storage';
const VALIDATE = '/_/api/admin/config/section/storage/validate';
const USAGE = '/_/api/admin/backends/hetzner-fsn1/legacy-key-usage';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', SECTION, json({ buckets: {} }));
  http.on('POST', VALIDATE, json({ ok: true, warnings: [], diff: { storage: { 'buckets.db-archive.encryption.mode': { before: 'x', after: 'y' } } } }));
  http.on('PUT', SECTION, json({ ok: true, warnings: [] }));
});
afterEach(() => vi.unstubAllGlobals());

function renderGroup(e: EffectiveEncryption, onApplied = vi.fn(), blockedReason: string | null = null) {
  renderWithQuery(<BucketEncryptionGroup bucket="db-archive" eff={e} onApplied={onApplied} blockedReason={blockedReason} />);
  return onApplied;
}

test('the radio names the backend mode and offers the modes the server accepts', () => {
  renderGroup(eff());
  const group = screen.getByRole('radiogroup', { name: 'Encryption of this bucket' });
  expect(within(group).getByText('Inherit — hetzner-fsn1: plaintext')).toBeInTheDocument();
  expect(within(group).getByText('Encrypt with a bucket key')).toBeInTheDocument();
  expect(within(group).getByText('Off for this bucket')).toBeInTheDocument();
  expect(within(group).queryByText('SSE-KMS on this bucket')).toBeNull();
  expect(screen.getByText('Now: Not encrypted.')).toBeInTheDocument();
});

test('a native mode appears when the server accepts it on a bucket', () => {
  renderGroup(eff({ bucket_modes: ['none', 'aes256-gcm-proxy', 'sse-kms'] }));
  expect(screen.getByText('SSE-KMS on this bucket')).toBeInTheDocument();
});

test('enabling a bucket key: the last 6 characters gate the apply; the body pins the backend', async () => {
  const user = userEvent.setup();
  const onApplied = renderGroup(eff({ pinned: false }));
  await user.click(screen.getByText('Encrypt with a bucket key'));
  const review = screen.getByRole('button', { name: 'Review & apply' });
  expect(review).toBeDisabled();
  const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
  expect(key).toMatch(/^[0-9a-f]{64}$/);
  expect((screen.getByLabelText('Key id') as HTMLInputElement).value).toMatch(/^db-archive-\d{4}-\d{2}$/);
  expect(screen.getByRole('button', { name: /Download key file/ })).toBeInTheDocument();
  expect(screen.getByTestId('bucket-key-yaml')).toHaveTextContent('key: ${env:DB_ARCHIVE_ENCRYPTION_KEY}');
  await user.type(screen.getByLabelText('Last 6 characters of the key'), '000000');
  expect(review).toBeDisabled();
  await user.clear(screen.getByLabelText('Last 6 characters of the key'));
  await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
  await waitFor(() => expect(review).toBeEnabled());
  await user.click(review);

  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent(/Only new writes change/);
  expect(dialog).toHaveTextContent(/uploads and deletes get a temporary 503/);
  const [validate] = http.callsTo('POST', VALIDATE);
  const body = validate.body as { buckets: Record<string, { backend?: string; encryption: Record<string, string> }> };
  expect(body.buckets['db-archive'].backend).toBe('hetzner-fsn1');
  expect(body.buckets['db-archive'].encryption).toMatchObject({ mode: 'aes256-gcm-proxy', key, key_id: expect.stringMatching(/^db-archive-/) });
  expect(validate.headers['x-dgp-confirm-encryption-downgrade']).toBeUndefined();

  await user.click(within(dialog).getByRole('button', { name: /Apply/ }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  expect(http.callsTo('PUT', SECTION)[0].body).toEqual(validate.body);
  await waitFor(() => expect(onApplied).toHaveBeenCalledTimes(1));
});

test('off on an encrypted bucket is confirmed and carries the downgrade header', async () => {
  const user = userEvent.setup();
  renderGroup(bucketKeyEff());
  await user.click(screen.getByText('Off for this bucket'));
  expect(screen.getByText('New writes to this bucket will be plaintext')).toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: 'Review & apply' }));
  const confirm = await screen.findByRole('dialog');
  expect(confirm).toHaveTextContent(/Store new writes to db-archive as plaintext/);
  expect(confirm).toHaveTextContent(/decrypt-only legacy key/);
  await user.click(within(confirm).getByRole('button', { name: 'Store as plaintext' }));
  await waitFor(() => expect(http.callsTo('POST', VALIDATE)).toHaveLength(1));
  const [validate] = http.callsTo('POST', VALIDATE);
  expect(validate.headers['x-dgp-confirm-encryption-downgrade']).toBe('db-archive');
  expect(validate.body).toEqual({ buckets: { 'db-archive': { encryption: { mode: 'none' } } } });
  const apply = await screen.findByText('Review changes before applying');
  await user.click(within(apply.closest('.ant-modal') as HTMLElement).getByRole('button', { name: /Apply/ }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  expect(http.callsTo('PUT', SECTION)[0].headers['x-dgp-confirm-encryption-downgrade']).toBe('db-archive');
});

test('a declined downgrade sends nothing', async () => {
  const user = userEvent.setup();
  renderGroup(bucketKeyEff());
  await user.click(screen.getByText('Off for this bucket'));
  await user.click(screen.getByRole('button', { name: 'Review & apply' }));
  const confirm = await screen.findByRole('dialog');
  await user.click(within(confirm).getByRole('button', { name: 'Cancel' }));
  expect(http.callsTo('POST', VALIDATE)).toHaveLength(0);
});

test('inherit is blocked while the bucket holds its own key, with the way out', () => {
  renderGroup(bucketKeyEff());
  const inherit = screen.getByRole('radio', { name: /Inherit/ });
  expect(inherit).toBeDisabled();
  expect(screen.getByText(/Choose "Off for this bucket" first/)).toBeInTheDocument();
});

test('rotate needs a free legacy slot', async () => {
  const user = userEvent.setup();
  renderGroup(bucketKeyEff({ legacy_key_id: 'db-archive-2026-04' }));
  expect(screen.getByRole('button', { name: /Rotate key/ })).toBeDisabled();
  vi.unstubAllGlobals();
  http = mockFetch();
  renderGroup(bucketKeyEff());
  const rotate = screen.getAllByRole('button', { name: /Rotate key/ }).find((b) => !(b as HTMLButtonElement).disabled)!;
  await user.click(rotate);
  // The current key id cannot be reused for the new key.
  await user.clear(screen.getByLabelText('Key id'));
  await user.type(screen.getByLabelText('Key id'), 'db-archive-2026-10');
  expect(screen.getByText(/already uses key id db-archive-2026-10/)).toBeInTheDocument();
});

test('an env-sourced key is read-only with a from env badge', () => {
  renderGroup(bucketKeyEff({ key_env: 'DB_ARCHIVE_KEY' }));
  expect(screen.getByText('from env DB_ARCHIVE_KEY')).toBeInTheDocument();
  for (const r of screen.getAllByRole('radio')) expect(r).toBeDisabled();
  expect(screen.queryByRole('button', { name: /Rotate key/ })).toBeNull();
});

test('unsaved staged edits block an immediate encryption change', () => {
  renderGroup(eff(), vi.fn(), 'Apply or discard the unsaved bucket changes first.');
  expect(screen.getByText('Apply or discard the unsaved bucket changes first.')).toBeInTheDocument();
  for (const r of screen.getAllByRole('radio')) expect(r).toBeDisabled();
});

test('clear legacy key: a per-bucket scan, then the removal header', async () => {
  const user = userEvent.setup();
  http.on(
    'GET',
    USAGE,
    json({
      backend: 'hetzner-fsn1',
      bucket: 'db-archive',
      legacy_key_id: 'db-archive-2026-04',
      buckets: ['db-archive', 'downloads'],
      objects_scanned: 10,
      objects_under_legacy_key: 0,
      references_scanned: 1,
      references_under_legacy_key: 0,
      examples: [],
      errors: [],
      complete: true,
      limit: 50000,
      safe_to_clear: true,
    }),
  );
  const onApplied = renderGroup(eff({ source: 'bucket', legacy_key_id: 'db-archive-2026-04', reencrypt: true }));
  const clear = screen.getByRole('button', { name: 'Clear legacy key' });
  expect(clear).toBeDisabled();
  await user.click(screen.getByRole('button', { name: 'Check usage' }));
  await waitFor(() => expect(clear).toBeEnabled());
  expect(http.calls.find((c) => c.path.startsWith(USAGE))?.path).toContain('bucket=db-archive');
  await user.click(clear);
  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent(/from bucket db-archive/);
  await user.click(within(dialog).getByRole('button', { name: 'Clear' }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  const put = http.callsTo('PUT', SECTION)[0];
  expect(put.headers['x-dgp-confirm-key-removal']).toBe('db-archive-2026-04');
  expect(put.body).toEqual({ buckets: { 'db-archive': { encryption: { legacy_key: null, legacy_key_id: null } } } });
  await waitFor(() => expect(onApplied).toHaveBeenCalled());
});

test('a refusal of the server shows in the group', async () => {
  const user = userEvent.setup();
  http.on('POST', VALIDATE, json({ ok: false, error: 'config refused: bucket encryption needs a witness' }));
  renderGroup(bucketKeyEff());
  await user.click(screen.getByText('Off for this bucket'));
  await user.click(screen.getByRole('button', { name: 'Review & apply' }));
  await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Store as plaintext' }));
  // The apply dialog shows the server's error and offers no apply.
  expect(await screen.findByText(/needs a witness/)).toBeInTheDocument();
});
