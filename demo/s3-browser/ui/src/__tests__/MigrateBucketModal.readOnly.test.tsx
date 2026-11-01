/**
 * Compatibility review C7: the migrate dialog said "uploads and deletes
 * get a temporary 503" also for a read-only bucket (an encrypted bucket
 * on a backend without atomic writes). There writes get 403 before, and
 * during, the migration; the migration is the way out.
 */
import { screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import MigrateBucketModal from '../components/MigrateBucketModal';
import { inheritedAesEff } from '../test/encryption';

const DOC = 'https://deltaglider.com/docs/how-to/backend-capability-validation';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on(
    'GET',
    '/_/api/admin/backends',
    json({
      default_backend: 'b2',
      backends: [
        { name: 'b2', backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null, has_credentials: true, encryption: { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k1', shim_active: false } },
      ],
    }),
  );
});
afterEach(() => vi.unstubAllGlobals());

function origins(readOnly: string | null) {
  http.on(
    'GET',
    '/_/api/admin/buckets',
    json({ buckets: [{ name: 'releases', backend_name: 'b2', effective_encryption: inheritedAesEff('b2', 'k1'), read_only: readOnly }] }),
  );
}

test('a read-only bucket: the dialog says writes stay refused (403) until the switch-over, not a 503', async () => {
  origins(`Bucket 'releases' is read-only: it is encrypted, and its backend does not support atomic writes — see ${DOC}`);
  renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
  const note = await screen.findByTestId('migrate-while-running');
  await screen.findByText(/is read-only/);
  expect(note).not.toHaveTextContent('503');
  expect(note).toHaveTextContent(/refused.*until the switch-over/);
});

test('a writable bucket keeps the temporary 503 note', async () => {
  origins(null);
  renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
  expect(await screen.findByTestId('migrate-while-running')).toHaveTextContent('temporary 503');
});
