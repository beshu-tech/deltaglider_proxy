/**
 * N2: encryption needs atomic writes. On a backend whose capability
 * verdict is not CAS, the UI disables the choices that encrypt with the
 * proxy and says why (visible text with the doc link, not a hover title);
 * "Check again" runs a fresh probe; an encrypted bucket on such a backend
 * shows that it is read-only; a job that encrypts no longer warns of a 503.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BucketEncryptionGroup from '../components/BucketEncryptionGroup';
import BucketCard from '../components/BucketCard';
import BackendsPanel from '../components/BackendsPanel';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import ReencryptProposalModal from '../components/ReencryptProposalModal';
import { DEFAULT_ROW_FIELDS } from '../components/bucketPolicyPayload';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff, eff } from '../test/encryption';

const PROBE = '/_/api/admin/backends/b2/capability-probe';
const NON_CAS = { verdict: 'non-cas' } as const;
const CAS = { verdict: 'cas-verified', via: 'probe' } as const;

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

test('a non-CAS backend: the bucket key choice is disabled, with the reason and the doc link', () => {
  renderWithQuery(<BucketEncryptionGroup bucket="db-archive" eff={eff({ backend: 'b2' })} onApplied={vi.fn()} capability={NON_CAS} />);
  const group = screen.getByRole('radiogroup', { name: 'Encryption of this bucket' });
  const key = within(group).getByRole('radio', { name: /Encrypt with a bucket key/ });
  expect(key).toBeDisabled();
  // Off stays possible.
  expect(within(group).getByRole('radio', { name: /Off for this bucket/ })).toBeEnabled();
  const notice = screen.getByTestId('atomic-writes-missing');
  expect(notice).toHaveTextContent('Encryption is not available on backend b2: it does not support atomic writes (the backend ignored or rejected a conditional write, as Backblaze B2 does)');
  expect(within(notice).getByRole('link')).toHaveAttribute('href', expect.stringContaining('backend-capability-validation'));
  expect(within(notice).getByRole('button', { name: /Check again/ })).toBeInTheDocument();
});

test('a CAS backend: the bucket key choice is enabled and no notice shows', () => {
  renderWithQuery(<BucketEncryptionGroup bucket="db-archive" eff={eff({ backend: 'b2' })} onApplied={vi.fn()} capability={CAS} />);
  expect(screen.getByRole('radio', { name: /Encrypt with a bucket key/ })).toBeEnabled();
  expect(screen.queryByTestId('atomic-writes-missing')).toBeNull();
});

test('the backend encryption editor disables rotation on a non-CAS backend and says why', () => {
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="b2"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-2026', shim_active: false }}
      onApply={async () => {}}
      capability={{ verdict: 'unknown', reason: 'timeout' }}
    />,
  );
  expect(screen.getByRole('button', { name: /Rotate key/ })).toBeDisabled();
  expect(screen.getByTestId('atomic-writes-missing')).toHaveTextContent('could not reach the backend to verify (timeout)');
});

test('the bucket card shows a read-only bucket in its row and in the open card', () => {
  const reason =
    "Bucket 'releases' is read-only: it is encrypted, and encryption needs atomic writes. See https://deltaglider.com/docs/how-to/backend-capability-validation";
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
      effectiveEncryption={bucketKeyEff({ backend: 'b2' })}
      readOnly={reason}
      globalCompressionOn
      globalRatio={0.75}
      onPatch={() => {}}
      onPrefixesChange={() => {}}
      inputRadius={{ borderRadius: 6 }}
    />,
  );
  // The header pill is short (the long text made the pills wrap); the
  // reason is its hover title and the visible text of the open card.
  const pill = screen.getByText('Read-only', { exact: true });
  expect(pill.closest('[title]')).toHaveAttribute('title', reason);
  const alert = screen.getByTestId('bucket-read-only');
  expect(alert).toHaveTextContent('Read-only: b2 does not support atomic writes');
  expect(within(alert).getByRole('link')).toHaveAttribute('href', expect.stringContaining('backend-capability-validation'));
  expect(within(alert).getByRole('button', { name: /Check again/ })).toBeInTheDocument();
});

test('the synthesized single backend without atomic writes: the bucket card offers no bucket key', () => {
  // GET /config lists the synthesized `default` backend WITHOUT a verdict;
  // the server's answer for the bucket carries it (and drops proxy AES).
  renderWithQuery(
    <BucketCard
      name="releases"
      row={{ ...DEFAULT_ROW_FIELDS, public_prefixes: [], _id: 'r1', name: 'releases' }}
      real
      expanded
      onToggle={() => {}}
      backends={[
        {
          name: 'default', backend_type: 'filesystem', path: '/data', endpoint: null, region: null,
          force_path_style: null, has_credentials: false, is_synthesized: true,
          encryption: { mode: 'none', has_key: false, shim_active: false },
        },
      ]}
      defaultBackend="default"
      originBackend="default"
      effectiveEncryption={eff({
        backend: 'default',
        bucket_modes: ['none'],
        capability: NON_CAS,
        proxy_unavailable:
          "Encryption with a proxy key needs atomic writes, which backend 'default' does not provide — see https://deltaglider.com/docs/how-to/backend-capability-validation",
      })}
      onEncryptionApplied={vi.fn()}
      globalCompressionOn
      globalRatio={0.75}
      onPatch={() => {}}
      onPrefixesChange={() => {}}
      inputRadius={{ borderRadius: 6 }}
    />,
  );
  const group = screen.getByRole('radiogroup', { name: 'Encryption of this bucket' });
  expect(within(group).getByRole('radio', { name: /Encrypt with a bucket key/ })).toBeDisabled();
  const notice = screen.getByTestId('atomic-writes-missing');
  expect(notice).toHaveTextContent('Encryption is not available on backend default');
  expect(within(notice).getByRole('button', { name: /Check again/ })).toBeInTheDocument();
});

test('the backend card lists its read-only buckets, and Check again lifts them after a CAS probe', async () => {
  let cas = false;
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75 }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/backends', () =>
    json({
      backends: [
        {
          name: 'b2', backend_type: 's3', path: null, endpoint: 'https://b2.example', region: 'eu-central-003',
          force_path_style: true, has_credentials: true,
          encryption: { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-1', shim_active: false },
          capability: cas ? CAS : NON_CAS,
          ...(cas ? {} : { read_only_buckets: ['ror-encrypted'] }),
        },
      ],
      default_backend: 'b2',
    }),
  );
  http.on('POST', PROBE, () => {
    cas = true;
    return json({ backend: 'b2', capability: CAS, probe_result: 'the backend enforces conditional writes', read_only_buckets: [] });
  });
  renderWithQuery(<BackendsPanel />);
  const badge = await screen.findByTestId('backend-read-only');
  expect(badge).toHaveTextContent('Read-only: b2 does not support atomic writes');
  expect(badge).toHaveTextContent('ror-encrypted');
  await userEvent.setup().click(within(badge).getByRole('button', { name: /Check again/ }));
  await waitFor(() => expect(http.callsTo('POST', PROBE)).toHaveLength(1));
  await waitFor(() => expect(screen.queryByTestId('backend-read-only')).toBeNull());
});

test('a job that encrypts does not warn of a 503; a decrypt on a backend without atomic writes may', () => {
  const { unmount } = renderWithQuery(
    <ReencryptProposalModal open transition="encrypt" backendName="b2" buckets={['releases']} onClose={() => {}} />,
  );
  expect(screen.getByTestId('reencrypt-while-running')).not.toHaveTextContent(/temporary 503/);
  unmount();
  renderWithQuery(<ReencryptProposalModal open transition="decrypt" backendName="b2" buckets={['releases']} onClose={() => {}} />);
  expect(screen.getByTestId('reencrypt-while-running')).toHaveTextContent(/without atomic writes.*temporary 503/);
});

test('a re-encrypt refused for lack of atomic writes shows the reason with the doc link in the dialog', async () => {
  http.on('POST', '/_/api/admin/jobs/reencrypt', json({
    started: [],
    errors: [{
      bucket: 'releases',
      error: "the re-encrypt job of bucket 'releases' refused: see https://deltaglider.com/docs/how-to/backend-capability-validation",
      atomic_writes: { code: 'atomic_writes_required', backend: 'b2', probe: 'non-cas', probe_result: 'x', doc_url: 'https://deltaglider.com/docs/how-to/backend-capability-validation' },
    }],
  }, 409));
  renderWithQuery(<ReencryptProposalModal open transition="encrypt" backendName="b2" buckets={['releases']} onClose={() => {}} />);
  await userEvent.setup().click(screen.getByRole('button', { name: /Start now/ }));
  const refusal = await screen.findByTestId('reencrypt-atomic-refusal');
  expect(within(refusal).getByRole('link')).toHaveAttribute('href', expect.stringContaining('backend-capability-validation'));
});
