/**
 * Browser-review item 24: typing "My_Bucket" in Create bucket silently
 * became "mybucket". The input now keeps the text, says what is wrong and
 * disables Create.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { waitFor } from '@testing-library/react';
import { renderWithQuery } from '../test/render';
import { json, mockFetch } from '../test/fetchMock';

vi.mock('../s3client', () => ({ createBucket: vi.fn() }));
import { createBucket } from '../s3client';
import CreateBucketModal from '../components/CreateBucketModal';

test('an invalid name is kept and explained, not rewritten', async () => {
  const user = userEvent.setup();
  renderWithQuery(<CreateBucketModal open canAdmin={false} onClose={() => {}} onCreated={() => {}} />);
  const input = await screen.findByLabelText('Bucket name');
  await user.type(input, 'My_Bucket');
  expect(input).toHaveValue('My_Bucket');
  expect(screen.getByRole('alert')).toHaveTextContent('Use only lowercase letters, digits, dots and hyphens.');
  expect(screen.getByRole('button', { name: 'Create' })).toBeDisabled();
  await user.clear(input);
  await user.type(input, 'my-bucket');
  expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Create' })).toBeEnabled();
});

/**
 * Issue #116 P2: the dialog asks for the bucket's encryption ("Same as
 * backend (<mode>)" by default), and writes a chosen bucket encryption as
 * the bucket's policy BEFORE it creates the bucket.
 */
describe('encryption of a new bucket', () => {
  const SECTION = '/_/api/admin/config/section/storage';
  const backend = (name: string, encryption: object) => ({
    name, backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null, has_credentials: true,
    encryption, bucket_encryption_modes: ['none', 'aes256-gcm-proxy'],
  });
  let http: ReturnType<typeof mockFetch>;
  beforeEach(() => {
    http = mockFetch();
    http.on('GET', '/_/api/admin/backends', json({
      default_backend: 'hetzner-fsn1',
      backends: [
        backend('hetzner-fsn1', { mode: 'none', has_key: false, shim_active: false }),
        backend('local-disk', { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k1', shim_active: false }),
      ],
    }));
    http.on('GET', SECTION, json({ buckets: {} }));
    http.on('PUT', SECTION, json({ ok: true }));
    vi.mocked(createBucket).mockReset();
  });
  afterEach(() => vi.unstubAllGlobals());

  test('the default writes no policy; the picker names each backend encryption', async () => {
    const user = userEvent.setup();
    renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
    await user.type(await screen.findByLabelText('Bucket name'), 'releases');
    expect(await screen.findByText('Same as backend (plaintext)')).toBeInTheDocument();
    await user.click(screen.getByRole('combobox', { name: 'Backend' }));
    expect(await screen.findByText('s3 · proxy AES-256-GCM')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Create' }));
    await waitFor(() => expect(createBucket).toHaveBeenCalledWith('releases', 'hetzner-fsn1'));
    expect(http.callsTo('PUT', SECTION)).toHaveLength(0);
  });

  test('a bucket key is written as the policy before the bucket is created', async () => {
    const user = userEvent.setup();
    let putsBeforeCreate = -1;
    vi.mocked(createBucket).mockImplementation(async () => {
      putsBeforeCreate = http.callsTo('PUT', SECTION).length;
    });
    renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
    await user.type(await screen.findByLabelText('Bucket name'), 'db-archive');
    await user.click(await screen.findByText('Encrypt with a bucket key'));
    const create = screen.getByRole('button', { name: 'Create' });
    expect(create).toBeDisabled();
    const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
    await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
    await waitFor(() => expect(create).toBeEnabled());
    await user.click(create);
    await waitFor(() => expect(createBucket).toHaveBeenCalledWith('db-archive', 'hetzner-fsn1'));
    expect(putsBeforeCreate).toBe(1);
    const body = http.callsTo('PUT', SECTION)[0].body as { buckets: Record<string, { backend: string; encryption: Record<string, string> }> };
    expect(body.buckets['db-archive'].backend).toBe('hetzner-fsn1');
    expect(body.buckets['db-archive'].encryption).toMatchObject({ mode: 'aes256-gcm-proxy', key });
    expect(body.buckets['db-archive'].encryption.key_id).toMatch(/^db-archive-\d{4}-\d{2}$/);
  });

  test('a refused policy stops the create and shows the reason', async () => {
    const user = userEvent.setup();
    http.on('PUT', SECTION, json({ ok: false, error: 'config refused: per-bucket encryption needs X' }));
    renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
    await user.type(await screen.findByLabelText('Bucket name'), 'db-archive');
    await user.click(await screen.findByText('Encrypt with a bucket key'));
    const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
    await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
    await user.click(screen.getByRole('button', { name: 'Create' }));
    expect(await screen.findByText(/per-bucket encryption needs X/)).toBeInTheDocument();
    expect(createBucket).not.toHaveBeenCalled();
  });
});
