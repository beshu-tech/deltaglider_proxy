/**
 * The migrate modal exposes the destination mode. By default the server
 * refuses a destination that already holds objects (an old safety copy
 * would bring deleted objects back); "exact mirror" sends `target: mirror`
 * and warns that destination extras are deleted.
 *
 * Jobs QA (maint B1, B5, M4, M5): the target picker shows health and
 * encryption and refuses a gated backend; a move from an encrypting backend
 * to a plaintext one needs an acknowledgement; a 409 shows inside the
 * dialog in its own words; the source backend comes from the origins.
 */
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import MigrateBucketModal from '../components/MigrateBucketModal';
import type { EffectiveEncryption } from '../adminApi';
import { bucketKeyEff, eff, inheritedAesEff } from '../test/encryption';

const PLAIN = { mode: 'none', has_key: false, shim_active: false };
const AES = { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k1', shim_active: false };
const NOW = 1_760_000_000;

function backend(name: string, type: string, encryption: object, health?: object) {
  return {
    name,
    backend_type: type,
    path: null,
    endpoint: null,
    region: null,
    force_path_style: null,
    has_credentials: true,
    encryption,
    ...(health ? { health: { probed_at: NOW, ...health } } : {}),
  };
}

const MIGRATE = '/_/api/admin/buckets/releases/migrate';
let http: ReturnType<typeof mockFetch>;

/** The server's answer for `releases`: by default it inherits the
 *  encryption of the backend that holds it. */
function serve(backends: object[], originBackend = 'hetzner-fsn1', own?: EffectiveEncryption) {
  http.on('GET', '/_/api/admin/backends', json({ default_backend: 'hetzner-fsn1', backends }));
  const holder = backends.find((b) => (b as { name: string }).name === originBackend) as
    | { encryption: { mode: string } }
    | undefined;
  const inherited =
    holder?.encryption.mode === 'aes256-gcm-proxy' ? inheritedAesEff(originBackend, 'k1') : eff({ backend: originBackend });
  http.on(
    'GET',
    '/_/api/admin/buckets',
    json({ buckets: [{ name: 'releases', backend_name: originBackend, effective_encryption: own ?? inherited }] }),
  );
}

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('POST', MIGRATE, json({ job_id: 1, id: 'maintenance:1', bucket: 'releases', from_backend: 'hetzner-fsn1', to_backend: 'local-disk' }, 202));
});
afterEach(() => vi.unstubAllGlobals());

async function openTargets() {
  const combo = await screen.findByRole('combobox');
  await waitFor(() => expect(combo.closest('.ant-select-disabled')).toBeNull());
  fireEvent.mouseDown(combo);
}

async function pickTarget(label: RegExp) {
  await openTargets();
  fireEvent.click(await screen.findByText(label));
}

const startButton = () => screen.getByRole('button', { name: 'Start migration' });

describe('MigrateBucketModal destination mode', () => {
  test('defaults to an empty destination and says so', async () => {
    serve([backend('hetzner-fsn1', 's3', PLAIN, { status: 'healthy' }), backend('local-disk', 'filesystem', PLAIN, { status: 'healthy' })]);
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    expect(await screen.findByText(/already holds objects, the job refuses/i)).toBeTruthy();
    await pickTarget(/^local-disk \(filesystem\)/);
    fireEvent.click(startButton());
    await waitFor(() => expect(http.callsTo('POST', MIGRATE)).toHaveLength(1));
    expect(http.callsTo('POST', MIGRATE)[0].body).toEqual({ target_backend: 'local-disk', delete_source: false, target: 'empty' });
  });

  test('exact mirror warns and sends target: mirror', async () => {
    serve([backend('hetzner-fsn1', 's3', PLAIN, { status: 'healthy' }), backend('local-disk', 'filesystem', PLAIN, { status: 'healthy' })]);
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    await pickTarget(/^local-disk \(filesystem\)/);
    expect(screen.queryByText(/are deleted before the switch-over/i)).toBeNull();
    fireEvent.click(screen.getByRole('checkbox', { name: /exact mirror/i }));
    expect(await screen.findByText(/are deleted before the switch-over/i)).toBeTruthy();
    fireEvent.click(startButton());
    await waitFor(() => expect(http.callsTo('POST', MIGRATE)).toHaveLength(1));
    expect(http.callsTo('POST', MIGRATE)[0].body).toMatchObject({ target: 'mirror' });
  });
});

describe('MigrateBucketModal target facts', () => {
  test('each target shows its encryption and health; a gated one cannot be picked', async () => {
    serve([
      backend('hetzner-fsn1', 's3', AES, { status: 'healthy' }),
      backend('local-disk', 'filesystem', AES, { status: 'erroring', detail: 'disk full' }),
      backend('aws-dr', 's3', { mode: 'sse-kms', has_key: false, kms_key_id: 'arn', shim_active: false }, { status: 'auth-rejected', detail: '403' }),
      backend('spare', 's3', PLAIN),
    ]);
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    await openTargets();
    expect(await screen.findByText('local-disk (filesystem) · proxy AES-256-GCM · erroring')).toBeInTheDocument();
    expect(screen.getByText('spare (s3) · plaintext · never probed')).toBeInTheDocument();
    const gated = screen.getByText('aws-dr (s3) · SSE-KMS · unavailable: credentials rejected');
    expect(gated.closest('.ant-select-item-option')).toHaveClass('ant-select-item-option-disabled');
    // The source backend is not a target.
    expect(screen.queryByText(/^hetzner-fsn1 \(s3\)/)).toBeNull();
    fireEvent.click(gated);
    expect(startButton()).toBeDisabled();
  });

  test('a target that becomes gated after it was picked blocks Start and says why', async () => {
    serve([backend('hetzner-fsn1', 's3', PLAIN, { status: 'healthy' }), backend('local-disk', 'filesystem', PLAIN, { status: 'healthy' })]);
    const { client } = renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    await pickTarget(/^local-disk \(filesystem\)/);
    expect(startButton()).toBeEnabled();
    serve([
      backend('hetzner-fsn1', 's3', PLAIN, { status: 'healthy' }),
      backend('local-disk', 'filesystem', PLAIN, { status: 'unreachable', detail: 'connection refused' }),
    ]);
    await client.invalidateQueries();
    expect(await screen.findByText('Backend local-disk is unavailable: unreachable')).toBeInTheDocument();
    expect(startButton()).toBeDisabled();
  });

  test('the source backend comes from the origins, not the default backend', async () => {
    serve(
      [backend('hetzner-fsn1', 's3', PLAIN, { status: 'healthy' }), backend('local-disk', 'filesystem', PLAIN, { status: 'healthy' })],
      'local-disk',
    );
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    await openTargets();
    expect(await screen.findByText(/^hetzner-fsn1 \(s3\)/)).toBeInTheDocument();
    expect(screen.queryByText(/^local-disk \(filesystem\)/)).toBeNull();
  });

  test('encrypted source to plaintext target: a loud warning and an acknowledgement before Start', async () => {
    serve([backend('hetzner-fsn1', 's3', AES, { status: 'healthy' }), backend('local-disk', 'filesystem', PLAIN, { status: 'healthy' })]);
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    await pickTarget(/^local-disk \(filesystem\)/);
    expect(await screen.findByText('Objects will be stored unencrypted at rest on local-disk')).toBeInTheDocument();
    expect(startButton()).toBeDisabled();
    fireEvent.click(screen.getByRole('checkbox', { name: /stored unencrypted on local-disk/ }));
    expect(startButton()).toBeEnabled();
    fireEvent.click(startButton());
    await waitFor(() => expect(http.callsTo('POST', MIGRATE)).toHaveLength(1));
  });

  /** Issue #116: a bucket's own encryption block moves with the bucket, so
   *  a plaintext target backend does not make it plaintext. */
  test("a bucket with its own key keeps it on a plaintext target: no acknowledgement", async () => {
    serve(
      [backend('hetzner-fsn1', 's3', AES, { status: 'healthy' }), backend('local-disk', 'filesystem', PLAIN, { status: 'healthy' })],
      'hetzner-fsn1',
      bucketKeyEff({ key_id: 'releases-2026-10' }),
    );
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    expect(await screen.findByText(/own encryption moves with it/)).toBeInTheDocument();
    await pickTarget(/^local-disk \(filesystem\)/);
    expect(screen.queryByText(/stored unencrypted at rest/)).toBeNull();
    expect(startButton()).toBeEnabled();
  });

  test('encrypted to encrypted needs no acknowledgement', async () => {
    serve([backend('hetzner-fsn1', 's3', AES, { status: 'healthy' }), backend('local-disk', 'filesystem', AES, { status: 'healthy' })]);
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    await pickTarget(/^local-disk \(filesystem\)/);
    expect(screen.queryByText(/stored unencrypted at rest/)).toBeNull();
    expect(startButton()).toBeEnabled();
  });

  test('a 409 shows inside the dialog, in the dialog words', async () => {
    serve([backend('hetzner-fsn1', 's3', PLAIN, { status: 'healthy' }), backend('local-disk', 'filesystem', PLAIN, { status: 'healthy' })]);
    http.on(
      'POST',
      MIGRATE,
      json(
        {
          error:
            "destination bucket 'releases' on backend 'local-disk' already holds 3 object(s). A migrate copies on top of them, so objects deleted at the source since that copy was made come back. Empty the destination first, or start the move again with \"Make the destination an exact mirror\" (API: \"target\": \"mirror\"): objects absent at the source are then deleted from the destination.",
        },
        409,
      ),
    );
    const onClose = vi.fn();
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={onClose} />);
    await pickTarget(/^local-disk \(filesystem\)/);
    fireEvent.click(startButton());
    const alert = (await screen.findByText('The migration did not start')).closest('.ant-alert') as HTMLElement;
    expect(within(alert).getByText(/already holds 3/)).toBeInTheDocument();
    expect(alert.textContent).toContain('already holds 3 object(s)');
    expect(alert.textContent).toContain('"Make the destination an exact mirror of the source"');
    expect(alert.textContent).not.toContain('API:');
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.queryByText(/started — track progress/)).toBeNull();
  });
});
