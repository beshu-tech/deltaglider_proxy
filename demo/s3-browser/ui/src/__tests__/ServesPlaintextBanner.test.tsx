/**
 * #120: "Refuse unencrypted objects" (`plaintext_reads: refuse`) makes every
 * unencrypted object fail to read. The click asks first, says so, and shows
 * the last re-encrypt job of each affected bucket (GET /jobs). Cancel sends
 * nothing.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import BucketEncryptionGroup from '../components/BucketEncryptionGroup';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff } from '../test/encryption';

const SECTION = '/_/api/admin/config/section/storage';
const JOBS = '/_/api/admin/jobs';
const BUTTON = 'Refuse unencrypted objects';

function reencrypt(id: number, bucket: string, status: string, failed = 0) {
  return {
    id: `maintenance:${id}`,
    kind: 'reencrypt',
    name: bucket,
    scope: { bucket },
    trigger: 'oneoff',
    status,
    status_raw: status,
    progress: { processed: 40, bytes: 0, failed, skipped: 0 },
    created_at: id,
    detail: {},
  };
}

function jobs(...rows: unknown[]) {
  return json({
    workers: { replication: { enabled: true, tick_interval: '1m' }, lifecycle: { enabled: false, tick_interval: '1h' } },
    jobs: rows,
  });
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', SECTION, json({ buckets: {} }));
  http.on('PUT', SECTION, json({ ok: true, warnings: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('bucket: the dialog names the consequence and the last re-encrypt job; Cancel sends nothing', async () => {
  http.on('GET', JOBS, jobs(reencrypt(1, 'db-archive', 'failed'), reencrypt(2, 'db-archive', 'succeeded')));
  const user = userEvent.setup();
  renderWithQuery(
    <BucketEncryptionGroup bucket="db-archive" eff={bucketKeyEff({ serves_plaintext: true })} onApplied={vi.fn()} />,
  );
  expect(screen.getByText('Unencrypted objects are served')).toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: BUTTON }));
  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent('Refuse unencrypted objects?');
  expect(dialog).toHaveTextContent(/not encrypted fails to read with an error/);
  expect(dialog).toHaveTextContent(/Run the re-encrypt job first, and make sure that it finished with no failures/);
  expect(within(dialog).getByTestId('refuse-plaintext-jobs')).toHaveTextContent(
    'db-archive: the last re-encrypt job finished with no failures.',
  );
  await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(http.callsTo('PUT', SECTION)).toHaveLength(0);
});

test('bucket: confirming sends plaintext_reads: refuse', async () => {
  http.on('GET', JOBS, jobs(reencrypt(1, 'db-archive', 'completed_with_errors', 4)));
  const user = userEvent.setup();
  const onApplied = vi.fn();
  renderWithQuery(
    <BucketEncryptionGroup bucket="db-archive" eff={bucketKeyEff({ serves_plaintext: true })} onApplied={onApplied} />,
  );
  await user.click(screen.getByRole('button', { name: BUTTON }));
  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent('db-archive: the last re-encrypt job finished with 4 failures.');
  await user.click(within(dialog).getByRole('button', { name: BUTTON }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  const body = http.callsTo('PUT', SECTION)[0].body as { buckets: Record<string, { encryption: unknown }> };
  expect(body.buckets['db-archive'].encryption).toEqual({ plaintext_reads: 'refuse' });
  await waitFor(() => expect(onApplied).toHaveBeenCalledTimes(1));
});

test('backend: one job line per bucket; confirm calls the handler, cancel does not', async () => {
  http.on('GET', JOBS, jobs(reencrypt(1, 'releases', 'running')));
  const user = userEvent.setup();
  const onRefuse = vi.fn(async () => {});
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="hetzner-fsn1"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-2026', shim_active: false, serves_plaintext: true }}
      onApply={async () => {}}
      onRefusePlaintext={onRefuse}
      buckets={['releases', 'downloads']}
    />,
  );
  await user.click(screen.getByRole('button', { name: BUTTON }));
  let dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent(/an object in the buckets of this backend that is not encrypted fails to read/);
  const lines = within(dialog).getByTestId('refuse-plaintext-jobs');
  expect(lines).toHaveTextContent('releases: a re-encrypt job is still running.');
  expect(lines).toHaveTextContent('downloads: no recent re-encrypt job.');
  await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(onRefuse).not.toHaveBeenCalled();

  await user.click(screen.getByRole('button', { name: BUTTON }));
  dialog = await screen.findByRole('dialog');
  await user.click(within(dialog).getByRole('button', { name: BUTTON }));
  await waitFor(() => expect(onRefuse).toHaveBeenCalledTimes(1));
});

test('backend: an unreadable job list still asks, and says so', async () => {
  http.on('GET', JOBS, json({ error: 'boom' }, 500));
  const user = userEvent.setup();
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="hetzner-fsn1"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-2026', shim_active: false, serves_plaintext: true }}
      onApply={async () => {}}
      onRefusePlaintext={async () => {}}
      buckets={['releases']}
    />,
  );
  await user.click(screen.getByRole('button', { name: BUTTON }));
  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent('The job list could not be read.');
  expect(within(dialog).queryByTestId('refuse-plaintext-jobs')).toBeNull();
});

test('read-only backend: the banner shows, without the button', () => {
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="hetzner-fsn1"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-2026', shim_active: false, serves_plaintext: true }}
      onApply={async () => {}}
      onRefusePlaintext={async () => {}}
      readOnly
    />,
  );
  expect(screen.getByText('Unencrypted objects are served')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: BUTTON })).toBeNull();
});
