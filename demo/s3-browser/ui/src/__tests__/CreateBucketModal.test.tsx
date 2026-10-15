/**
 * Browser-review item 24: typing "My_Bucket" in Create bucket silently
 * became "mybucket". The input now keeps the text, says what is wrong and
 * disables Create.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { waitFor, within } from '@testing-library/react';
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
 * Issue #116 P2: the dialog asks for the bucket's encryption. What the
 * bucket gets by default is the server's answer (encryption-preview), and
 * a chosen block goes WITH the create in one request (review defect 1:
 * the policy used to be written first, and a create of an existing name
 * then moved that bucket away from its objects).
 */
describe('encryption of a new bucket', () => {
  const backend = (name: string, encryption: object) => ({
    name, backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null, has_credentials: true,
    encryption, bucket_encryption_modes: ['none', 'aes256-gcm-proxy'],
  });
  const preview = (over: object = {}) => ({
    mode: 'none', source: 'backend', backend: 'hetzner-fsn1', has_key: false, encrypts: false, reencrypt: false,
    pinned: true, bucket_modes: ['none', 'aes256-gcm-proxy'], inherited: { mode: 'none', encrypts: false }, ...over,
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
    http.on('GET', /\/encryption-preview/, (req) =>
      json(req.path.includes('backend=local-disk')
        ? preview({ backend: 'local-disk', mode: 'aes256-gcm-proxy', encrypts: true, has_key: true, key_id: 'k1' })
        : preview()),
    );
    vi.mocked(createBucket).mockReset();
  });
  afterEach(() => vi.unstubAllGlobals());

  test('the default sends no block; the picker names each backend encryption', async () => {
    const user = userEvent.setup();
    renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
    await user.type(await screen.findByLabelText('Bucket name'), 'releases');
    expect(await screen.findByText('Same as backend (plaintext)')).toBeInTheDocument();
    await user.click(screen.getByRole('combobox', { name: 'Backend' }));
    expect(await screen.findByText('s3 · proxy AES-256-GCM')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Create' }));
    await waitFor(() => expect(createBucket).toHaveBeenCalledWith('releases', 'hetzner-fsn1', undefined));
  });

  test('a bucket key goes with the create, in one request', async () => {
    const user = userEvent.setup();
    renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
    await user.type(await screen.findByLabelText('Bucket name'), 'db-archive');
    await user.click(await screen.findByText('Encrypt with a bucket key'));
    const create = screen.getByRole('button', { name: 'Create' });
    expect(create).toBeDisabled();
    const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
    await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
    await waitFor(() => expect(create).toBeEnabled());
    await user.click(create);
    await waitFor(() => expect(createBucket).toHaveBeenCalledTimes(1));
    const [bucket, target, opts] = vi.mocked(createBucket).mock.calls[0];
    expect([bucket, target]).toEqual(['db-archive', 'hetzner-fsn1']);
    expect(opts?.encryption).toMatchObject({ mode: 'aes256-gcm-proxy', key });
    expect(String(opts?.encryption?.key_id)).toMatch(/^db-archive-\d{4}-\d{2}$/);
    expect(opts?.confirm).toBeUndefined();
    // Nothing goes to the storage section first.
    expect(http.calls.filter((c) => c.method === 'PUT')).toHaveLength(0);
  });

  test('off on an encrypting backend is confirmed first, and only then sends the downgrade header', async () => {
    const user = userEvent.setup();
    renderWithQuery(<CreateBucketModal open canAdmin presetBackend="local-disk" onClose={() => {}} onCreated={() => {}} />);
    await user.type(await screen.findByLabelText('Bucket name'), 'downloads');
    expect(await screen.findByText('Same as backend (proxy AES-256-GCM)')).toBeInTheDocument();
    await user.click(screen.getByText('Off for this bucket (plaintext)'));
    await user.click(screen.getByRole('button', { name: 'Create' }));
    const confirmDlg = async () => {
      await waitFor(() => expect(screen.getAllByText('Store downloads as plaintext?').length).toBeGreaterThan(0));
      const all = screen.getAllByRole('dialog');
      return all[all.length - 1];
    };
    // Declined: nothing is sent.
    await user.click(within(await confirmDlg()).getByRole('button', { name: 'Cancel' }));
    expect(createBucket).not.toHaveBeenCalled();
    await user.click(await screen.findByRole('button', { name: 'Create' }));
    await user.click(within(await confirmDlg()).getByRole('button', { name: 'Store as plaintext' }));
    await waitFor(() => expect(createBucket).toHaveBeenCalledTimes(1));
    expect(vi.mocked(createBucket).mock.calls[0]).toEqual([
      'downloads',
      'local-disk',
      { encryption: { mode: 'none' }, confirm: { downgrades: ['downloads'] } },
    ]);
  });

  test('a name with settings already keeps them by default', async () => {
    http.on('GET', /\/encryption-preview/, json(preview({ source: 'bucket', mode: 'aes256-gcm-proxy', has_key: true, encrypts: true, key_id: 'ghost-2026-10' })));
    const user = userEvent.setup();
    renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
    await user.type(await screen.findByLabelText('Bucket name'), 'ghost');
    expect(await screen.findByText(/Keep the settings this name already has \(Encrypted · bucket key ghost-2026-10\)/)).toBeInTheDocument();
  });

  test('a refused create shows the reason; a second Enter sends nothing more', async () => {
    let finish: (e: Error) => void = () => {};
    vi.mocked(createBucket).mockImplementation(() => new Promise((_, reject) => { finish = reject; }));
    const user = userEvent.setup();
    renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
    const input = await screen.findByLabelText('Bucket name');
    await user.type(input, 'db-archive');
    await user.click(await screen.findByText('Encrypt with a bucket key'));
    const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
    await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
    await user.click(screen.getByRole('button', { name: 'Create' }));
    await user.type(input, '{Enter}');
    await user.type(input, '{Enter}');
    expect(createBucket).toHaveBeenCalledTimes(1);
    finish(new Error("bucket 'db-archive' already exists"));
    expect(await screen.findByText(/The bucket was not created: .*already exists/)).toBeInTheDocument();
  });
});

/**
 * "Create and open settings": an admin creates the bucket and lands on its
 * card on the Buckets page (the full form). A non-admin never sees it.
 */
describe('create and open settings', () => {
  afterEach(() => vi.unstubAllGlobals());

  test('a non-admin sees no such button', async () => {
    renderWithQuery(<CreateBucketModal open canAdmin={false} onClose={() => {}} onCreated={() => {}} />);
    await screen.findByLabelText('Bucket name');
    expect(screen.queryByRole('button', { name: 'Create and open settings' })).toBeNull();
  });

  test('an admin creates the bucket, then opens its settings', async () => {
    const http = mockFetch();
    http.on('GET', '/_/api/admin/backends', json({ default_backend: 'hetzner-fsn1', backends: [] }));
    http.on('GET', /\/encryption-preview/, json(null, 404));
    vi.mocked(createBucket).mockReset().mockResolvedValue(undefined as never);
    const navigate = vi.fn();
    const onCreated = vi.fn();
    const user = userEvent.setup();
    const { NavigationContext } = await import('../NavigationContext');
    renderWithQuery(
      <NavigationContext.Provider value={{ navigate, subPath: '' }}>
        <CreateBucketModal open canAdmin onClose={() => {}} onCreated={onCreated} />
      </NavigationContext.Provider>,
    );
    const button = screen.getByRole('button', { name: 'Create and open settings' });
    expect(button).toBeDisabled();
    await user.type(await screen.findByLabelText('Bucket name'), 'releases');
    await user.click(button);
    await waitFor(() => expect(navigate).toHaveBeenCalledWith('/_/admin/storage/buckets?bucket=releases'));
    expect(createBucket).toHaveBeenCalledWith('releases', undefined, undefined);
    expect(onCreated).toHaveBeenCalledWith('releases');
  });
});
