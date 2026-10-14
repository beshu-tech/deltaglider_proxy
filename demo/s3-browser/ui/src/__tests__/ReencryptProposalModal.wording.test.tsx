/**
 * Explore finding 19: the re-encrypt proposal said rewritten objects get a
 * new Last-Modified, but the job keeps each object's created-at (served as
 * LastModified), so sync tools do not re-download them.
 */
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import ReencryptProposalModal from '../components/ReencryptProposalModal';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff, eff, inheritedAesEff } from '../test/encryption';

test('the proposal says rewritten objects keep their Last-Modified', async () => {
  renderWithQuery(
    <ReencryptProposalModal open transition="rotate" backendName="local-disk" buckets={['releases']} onClose={() => {}} />,
  );
  expect(await screen.findByText(/keep their Last-Modified/)).toBeInTheDocument();
  expect(screen.queryByText(/get a new Last-Modified/)).toBeNull();
});

/**
 * Jobs QA maint B4: Jobs → New job → Re-encrypt said "Encrypt existing
 * objects? ... still plaintext" (wrong after a key rotation) and offered
 * every bucket, including buckets on plaintext backends (a no-op job that
 * still gates writes). Pick mode now lists only buckets that a re-encrypt
 * can change, with backend + encryption, behind a search box.
 *
 * Issue #116 P2: "can change" is the server's per-bucket answer
 * (`effective_encryption.reencrypt`). A bucket with its own key on a
 * plaintext backend is listed; the old per-backend lookup hid it.
 */
describe('pick mode (Jobs → New job)', () => {
  const PLAIN = { mode: 'none', has_key: false, shim_active: false };
  const AES = { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k2', shim_active: false };
  const SHIM = { mode: 'none', has_key: false, shim_active: true };
  const backend = (name: string, encryption: object) => ({
    name, backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null, has_credentials: true, encryption,
  });
  let http: ReturnType<typeof mockFetch>;
  beforeEach(() => {
    http = mockFetch();
    http.on('GET', '/_/api/admin/backends', json({
      default_backend: 'hetzner-fsn1',
      backends: [backend('hetzner-fsn1', AES), backend('local-disk', PLAIN), backend('aws-dr', SHIM)],
    }));
    http.on('GET', '/_/api/admin/buckets', json({
      buckets: [
        { name: 'releases', backend_name: 'hetzner-fsn1', effective_encryption: inheritedAesEff('hetzner-fsn1', 'k2') },
        { name: 'downloads', backend_name: 'local-disk', effective_encryption: eff({ backend: 'local-disk' }) },
        {
          name: 'db-archive',
          backend_name: 'aws-dr',
          effective_encryption: eff({ backend: 'aws-dr', reencrypt: true, legacy_key_id: 'aws-old' }),
        },
        // Its own key on the plaintext local-disk backend.
        { name: 'vault', backend_name: 'local-disk', effective_encryption: bucketKeyEff({ backend: 'local-disk', key_id: 'vault-2026-10' }) },
      ],
    }));
    http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  });
  afterEach(() => vi.unstubAllGlobals());

  const renderPick = () =>
    renderWithQuery(
      <ReencryptProposalModal open transition="encrypt" backendName="" buckets={[]} pickBuckets onClose={() => {}} />,
    );

  test('neutral wording, not the post-apply "encrypt" copy', async () => {
    renderPick();
    expect(await screen.findByText('Re-encrypt existing objects')).toBeInTheDocument();
    expect(screen.getByText(/an object under an older key gets re-encrypted/)).toBeInTheDocument();
    expect(screen.queryByText('Encrypt existing objects?')).toBeNull();
    expect(screen.queryByText(/still plaintext/)).toBeNull();
  });

  test('lists only buckets a re-encrypt can change, with backend and encryption', async () => {
    renderPick();
    expect(await screen.findByRole('checkbox', { name: 'releases' })).toBeInTheDocument();
    expect(screen.getByText('on hetzner-fsn1 · Encrypted · from hetzner-fsn1')).toBeInTheDocument();
    expect(screen.getByRole('checkbox', { name: 'db-archive' })).toBeInTheDocument();
    expect(screen.getByText('on aws-dr · Not encrypted, legacy key aws-old')).toBeInTheDocument();
    expect(screen.queryByRole('checkbox', { name: 'downloads' })).toBeNull();
    // Per bucket: its own key counts, although its backend is plaintext.
    expect(screen.getByRole('checkbox', { name: 'vault' })).toBeInTheDocument();
    expect(screen.getByText('on local-disk · Encrypted · bucket key vault-2026-10')).toBeInTheDocument();
    expect(screen.getByText(/1 bucket is not listed: the job has nothing to change there/)).toBeInTheDocument();
  });

  test('the search filters by bucket or backend name', async () => {
    renderPick();
    await screen.findByRole('checkbox', { name: 'releases' });
    fireEvent.change(screen.getByRole('searchbox', { name: 'Search buckets' }), { target: { value: 'aws' } });
    expect(screen.queryByRole('checkbox', { name: 'releases' })).toBeNull();
    expect(screen.getByRole('checkbox', { name: 'db-archive' })).toBeInTheDocument();
  });

  test('after a partial start only the failed buckets stay selected', async () => {
    http.on('POST', '/_/api/admin/jobs/reencrypt', json({
      started: [{ bucket: 'releases', job_id: 7 }],
      errors: [{ bucket: 'db-archive', error: 'busy' }],
    }));
    renderPick();
    fireEvent.click(await screen.findByRole('checkbox', { name: 'releases' }));
    fireEvent.click(screen.getByRole('checkbox', { name: 'db-archive' }));
    fireEvent.click(screen.getByRole('button', { name: /Start now \(2 buckets\)/ }));
    await waitFor(() => expect(screen.getByRole('button', { name: /Start now \(1 bucket\)/ })).toBeInTheDocument());
    expect(screen.getByRole('checkbox', { name: 'releases' })).not.toBeChecked();
    expect(screen.getByRole('checkbox', { name: 'db-archive' })).toBeChecked();
  });
});
