/**
 * Review finding 12: the states of a backend's atomic writes in the admin
 * UI. The first check (no verdict yet: writes wait with 503) shows on the
 * backend card; "could not verify" reads differently from "does not
 * support" and uses the server's words; a read-only alert does not say
 * the same thing in its title and its description.
 */
import { screen, within } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendsPanel from '../components/BackendsPanel';
import BucketCard from '../components/BucketCard';
import { DEFAULT_ROW_FIELDS } from '../components/bucketPolicyPayload';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff } from '../test/encryption';

const DOC = 'https://deltaglider.com/docs/how-to/backend-capability-validation';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75 }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
});
afterEach(() => vi.unstubAllGlobals());

function b2(over: Record<string, unknown>) {
  return {
    name: 'b2', backend_type: 's3', path: null, endpoint: 'https://b2.example', region: 'eu-central-003',
    force_path_style: true, has_credentials: true,
    encryption: { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-1', shim_active: false },
    ...over,
  };
}

test('the backend card shows the first check while it runs', async () => {
  http.on('GET', '/_/api/admin/backends', json({ backends: [b2({ atomic_writes_checking: true })], default_backend: 'b2' }));
  renderWithQuery(<BackendsPanel />);
  const checking = await screen.findByTestId('backend-atomic-writes-checking');
  expect(checking).toHaveTextContent('Checking atomic writes…');
  expect(checking).toHaveTextContent(/retry/);
});

test('an unverified backend says "could not verify", in the server words, never "does not support"', async () => {
  http.on(
    'GET',
    '/_/api/admin/backends',
    json({
      backends: [
        b2({
          capability: { verdict: 'unknown', reason: 'dispatch failure' },
          capability_words: 'the proxy could not verify the backend (it did not answer in time)',
          proxy_unavailable: `Encryption with a proxy key is not available on backend 'b2' now: the proxy could not verify that it supports atomic writes — see ${DOC}`,
        }),
      ],
      default_backend: 'b2',
    }),
  );
  renderWithQuery(<BackendsPanel />);
  const alert = await screen.findByTestId('backend-capability-unknown');
  expect(alert).toHaveTextContent(/the proxy could not verify the backend \(it did not answer in time\)/i);
  expect(alert).not.toHaveTextContent('dispatch failure');
  expect(alert).not.toHaveTextContent('does not support');
  expect(within(alert).queryByText(DOC)).toBeNull();
});

test('the read-only alert of a bucket says the reason once', () => {
  const reason = `Bucket 'releases' is read-only: it is encrypted, and its backend does not support atomic writes, which encryption needs. Reads keep working — see ${DOC}`;
  renderWithQuery(
    <BucketCard
      name="releases"
      row={{ ...DEFAULT_ROW_FIELDS, public_prefixes: [], _id: 'r1', name: 'releases' }}
      real
      expanded
      onToggle={() => {}}
      backends={[]}
      defaultBackend="b2"
      originBackend="b2"
      effectiveEncryption={bucketKeyEff({ backend: 'b2', capability: { verdict: 'non-cas' } })}
      readOnly={reason}
      globalCompressionOn
      globalRatio={0.75}
      onPatch={() => {}}
      onPrefixesChange={() => {}}
      inputRadius={{ borderRadius: 6 }}
    />,
  );
  const alert = screen.getByTestId('bucket-read-only');
  const said = (alert.textContent ?? '').match(/does not support atomic writes/g) ?? [];
  expect(said).toHaveLength(1);
  expect(within(alert).getByRole('link', { name: 'How to check a backend' })).toBeInTheDocument();
});
