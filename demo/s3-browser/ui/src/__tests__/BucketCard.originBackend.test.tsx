/**
 * Jobs QA maint M5: the Buckets page labelled every bucket without an
 * explicit route "default — <default backend>", also a bucket that lives
 * only on another backend, and took the encryption chip from the default
 * backend. The chip now names the backend that holds the bucket (origins
 * `backend_name`), and the encryption chip follows it.
 */
import { screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import BucketCard from '../components/BucketCard';
import type { BackendInfo } from '../adminApi';
import { renderWithQuery } from '../test/render';

const backend = (name: string, encryption: BackendInfo['encryption']): BackendInfo => ({
  name, backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null, has_credentials: true, encryption,
});
const BACKENDS = [
  backend('local-disk', { mode: 'none', has_key: false, shim_active: false }),
  backend('hetzner-fsn1', { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k1', shim_active: false }),
];

function renderCard(originBackend: string | null) {
  renderWithQuery(
    <BucketCard
      name="releases"
      row={null}
      real
      expanded={false}
      onToggle={() => {}}
      backends={BACKENDS}
      defaultBackend="local-disk"
      originBackend={originBackend}
      globalCompressionOn
      globalRatio={0.75}
      onPatch={() => {}}
      onPrefixesChange={() => {}}
      inputRadius={{ borderRadius: 6 }}
    />,
  );
}

test('a bucket on a non-default backend without a route names that backend and its encryption', () => {
  renderCard('hetzner-fsn1');
  expect(screen.getByText('on hetzner-fsn1')).toBeInTheDocument();
  expect(screen.queryByText(/default — local-disk/)).toBeNull();
  expect(screen.getByText('Encrypted (proxy, AES-256-GCM)')).toBeInTheDocument();
});

test('a bucket on the default backend keeps the default label', () => {
  renderCard('local-disk');
  expect(screen.getByText('default — local-disk')).toBeInTheDocument();
  expect(screen.getByText('Not encrypted')).toBeInTheDocument();
});
