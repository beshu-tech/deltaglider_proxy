/**
 * Review findings 8 and 10: the Create bucket dialog on a backend that
 * encrypts with the proxy and does not support atomic writes.
 *
 * 8: "Check again" refreshes the dialog (the backends and the preview are
 *    react-query data under `qk.backends`).
 * 10: "Same as backend" (proxy AES) is disabled AND not selected; the
 *    dialog selects "Off", each disabled option says why, and Create
 *    sends the request (after the plaintext confirmation).
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';
import { json, mockFetch } from '../test/fetchMock';
import { eff } from '../test/encryption';

vi.mock('../s3client', () => ({ createBucket: vi.fn(async () => {}) }));
import { createBucket } from '../s3client';
import CreateBucketModal from '../components/CreateBucketModal';

const DOC = 'https://deltaglider.com/docs/how-to/backend-capability-validation';
const UNAVAILABLE = `Encryption with a proxy key is not available on backend 'b2': it does not support atomic writes (conditional writes), which encryption needs — see ${DOC}`;

let cas = false;
let http: ReturnType<typeof mockFetch>;

function backend() {
  return {
    name: 'b2', backend_type: 's3', path: null, endpoint: 'https://b2.example', region: 'r',
    force_path_style: true, has_credentials: true, is_synthesized: false,
    encryption: { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k', shim_active: false },
    capability: cas ? { verdict: 'cas-verified', via: 'probe' } : { verdict: 'non-cas' },
    capability_words: cas ? 'the backend enforces conditional writes' : 'the backend does not support conditional writes',
    proxy_unavailable: cas ? null : UNAVAILABLE,
    atomic_writes_checking: false,
  };
}

beforeEach(() => {
  cas = false;
  http = mockFetch();
  http.on('GET', '/_/api/admin/backends', () => json({ backends: [backend()], default_backend: 'b2' }));
  http.on('GET', /encryption-preview/, () =>
    json(
      eff({
        backend: 'b2', mode: 'aes256-gcm-proxy', encrypts: true, has_key: true,
        inherited: { mode: 'aes256-gcm-proxy', encrypts: true, key_id: 'k' },
        bucket_modes: cas ? ['none', 'aes256-gcm-proxy'] : ['none'],
        capability: backend().capability as never,
        proxy_unavailable: cas ? null : UNAVAILABLE,
      }),
    ),
  );
  http.on('POST', '/_/api/admin/backends/b2/capability-probe', () => {
    cas = true;
    return json({ backend: 'b2', capability: { verdict: 'cas-verified', via: 'probe' }, probe_result: 'ok', read_only_buckets: [] });
  });
});
afterEach(() => vi.unstubAllGlobals());

test('Check again inside Create bucket refreshes the dialog', async () => {
  const user = userEvent.setup();
  renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
  await user.type(await screen.findByLabelText('Bucket name'), 'releases');
  const notice = await screen.findByTestId('atomic-writes-missing');
  await user.click(within(notice).getByRole('button', { name: /Check again/ }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/backends/b2/capability-probe')).toHaveLength(1));
  await waitFor(() => expect(screen.queryByTestId('atomic-writes-missing')).toBeNull());
  expect(screen.getByRole('radio', { name: /Same as backend/ })).toBeEnabled();
});

test('without atomic writes: Same as backend is disabled and not selected, Off is, and each disabled option says why', async () => {
  const user = userEvent.setup();
  renderWithQuery(<CreateBucketModal open canAdmin onClose={() => {}} onCreated={() => {}} />);
  await user.type(await screen.findByLabelText('Bucket name'), 'releases');
  await screen.findByTestId('atomic-writes-missing');
  const group = screen.getByRole('radiogroup', { name: 'Encryption of the new bucket' });
  const same = within(group).getByRole('radio', { name: /Same as backend/ });
  expect(same).toBeDisabled();
  expect(same).not.toBeChecked();
  expect(within(group).getByRole('radio', { name: /Off for this bucket/ })).toBeChecked();
  // The bucket key shows disabled with its reason, like the bucket card.
  const key = within(group).getByRole('radio', { name: /Encrypt with a bucket key/ });
  expect(key).toBeDisabled();
  for (const radio of [same, key]) {
    const label = radio.closest('label')!;
    expect(label).toHaveTextContent(/Needs atomic writes, which b2 does not support/);
  }
  await user.click(screen.getByRole('button', { name: 'Create' }));
  // Off on an encrypting backend asks first; the confirm then creates.
  await user.click(await screen.findByRole('button', { name: 'Store as plaintext' }));
  await waitFor(() => expect(createBucket).toHaveBeenCalled());
});
