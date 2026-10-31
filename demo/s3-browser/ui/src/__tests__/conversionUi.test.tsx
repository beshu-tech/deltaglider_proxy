/**
 * #120, conversion marker (docs/plan/encryption-conversion-marker.md P6):
 * turning on proxy AES over objects names how many the job encrypts and
 * that they stay readable; the bucket and backend cards show one row per
 * open conversion (running %, Resume, deadline passed, objects stored
 * after the cutoff); a bucket or backend with the deprecated
 * `plaintext_reads: serve` offers Convert now.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import BackendsPanel from '../components/BackendsPanel';
import BucketEncryptionGroup from '../components/BucketEncryptionGroup';
import type { ConversionView, EffectiveEncryption } from '../adminApi';
import { NavigationContext } from '../NavigationContext';
import { useBucketOrigins } from '../queries/backends';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff, eff } from '../test/encryption';

const SECTION = '/_/api/admin/config/section/storage';
const VALIDATE = '/_/api/admin/config/section/storage/validate';
const REENCRYPT = '/_/api/admin/jobs/reencrypt';
const RUN = 'a'.repeat(32);

function view(over: Partial<ConversionView> = {}): ConversionView {
  return {
    open: true,
    cutoff: 1_793_000_000,
    deadline: 1_793_604_800,
    run: RUN,
    adopted_after_cutoff: 0,
    job_on_this_node: null,
    serves_plaintext_deprecated: false,
    ...over,
  };
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', SECTION, json({ buckets: {} }));
  http.on('PUT', SECTION, json({ ok: true, warnings: [] }));
  http.on('POST', REENCRYPT, json({ started: [{ bucket: 'db-archive', job_id: 9 }], errors: [] }));
});
afterEach(() => vi.unstubAllGlobals());

function renderGroup(e: EffectiveEncryption, navigate = vi.fn()) {
  renderWithQuery(
    <NavigationContext.Provider value={{ navigate, subPath: '' } as never}>
      <BucketEncryptionGroup bucket="db-archive" eff={e} onApplied={vi.fn()} />
    </NavigationContext.Provider>,
  );
  return navigate;
}

test('the dialog names the objects the job encrypts, that they stay readable, and the versioned warning', async () => {
  http.on(
    'POST',
    VALIDATE,
    json({
      ok: true,
      diff: { storage: { 'buckets.db-archive.encryption.mode': { before: 'none', after: 'aes256-gcm-proxy' } } },
      conversion_preview: [{ bucket: 'db-archive', objects: 1204, more: false, versioned: true }],
    }),
  );
  const user = userEvent.setup();
  renderGroup(eff());
  await user.click(screen.getByText('Encrypt with a bucket key'));
  const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
  await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
  const review = screen.getByRole('button', { name: 'Review & apply' });
  await waitFor(() => expect(review).toBeEnabled());
  await user.click(review);

  const dialog = await screen.findByRole('dialog');
  expect(within(dialog).getByTestId('conversion-count')).toHaveTextContent(
    'The 1,204 objects already in bucket db-archive are not encrypted yet. After you apply, a re-encrypt job encrypts them in the background. They stay readable while the job runs.',
  );
  expect(within(dialog).getByTestId('conversion-versioned')).toHaveTextContent(
    /keeps old versions of bucket db-archive.*noncurrent versions/,
  );
  // The old "refuse after the job" advice is gone.
  expect(dialog).not.toHaveTextContent(/Refuse unencrypted objects/);
});

test('running: the progress of this node\'s job and a link to the run, no Resume', async () => {
  const user = userEvent.setup();
  const navigate = renderGroup(
    bucketKeyEff({ conversion: view({ job_on_this_node: { id: 7, status: 'running', percent: 42 } }) }),
  );
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveAttribute('data-state', 'running');
  // On the state line, as its continuation: one block, one hierarchy.
  expect(screen.getByText(/^Now:/)).toContainElement(row);
  expect(screen.getByText(/^Now:/)).toHaveTextContent(/^Now: .* · encrypting the existing objects, 42% · View run$/);
  expect(within(row).queryByRole('button', { name: 'Resume' })).toBeNull();
  await user.click(within(row).getByRole('button', { name: 'View run' }));
  expect(navigate).toHaveBeenCalledWith('/_/admin/jobs?job=maintenance%3A7');
});

test('running with no count yet says it is starting', () => {
  renderGroup(bucketKeyEff({ conversion: view({ job_on_this_node: { id: 7, status: 'queued', percent: null } }) }));
  expect(screen.getByTestId('conversion-status')).toHaveTextContent('starting to encrypt the existing objects…');
});

test('no job on this node: Resume starts the re-encrypt job of the bucket', async () => {
  const user = userEvent.setup();
  renderGroup(bucketKeyEff({ conversion: view() }));
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveAttribute('data-state', 'resume');
  expect(row).toHaveTextContent(/no job runs on this instance/);
  await user.click(within(row).getByRole('button', { name: 'Resume' }));
  await waitFor(() => expect(http.callsTo('POST', REENCRYPT)).toHaveLength(1));
  expect(http.callsTo('POST', REENCRYPT)[0].body).toEqual({ buckets: ['db-archive'] });
});

test('a failed job offers Resume', () => {
  renderGroup(bucketKeyEff({ conversion: view({ job_on_this_node: { id: 7, status: 'failed', percent: null } }) }));
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveAttribute('data-state', 'resume');
  expect(row).toHaveTextContent(/stopped with failures/);
  expect(within(row).getByRole('button', { name: 'Resume' })).toBeInTheDocument();
});

test('deadline passed: a warning that the conversion stopped before it finished, and Resume', () => {
  renderGroup(
    bucketKeyEff({ conversion: view({ open: false, job_on_this_node: { id: 7, status: 'cancelled', percent: null } }) }),
  );
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveAttribute('data-state', 'stopped');
  expect(row).toHaveTextContent('the conversion stopped before it finished');
  expect(row.querySelector('.ant-typography-warning')).not.toBeNull();
  expect(within(row).getByRole('button', { name: 'Resume' })).toBeInTheDocument();
});

test('objects stored after the cutoff are counted, with a link to the run\'s failures', async () => {
  const user = userEvent.setup();
  const navigate = renderGroup(
    bucketKeyEff({
      conversion: view({ adopted_after_cutoff: 3, job_on_this_node: { id: 7, status: 'running', percent: 80 } }),
    }),
  );
  const row = screen.getByTestId('conversion-status');
  await user.click(within(row).getByRole('button', { name: '3 objects stored after the cutoff' }));
  expect(navigate).toHaveBeenCalledWith('/_/admin/jobs?job=maintenance%3A7&tab=failures');
});

test('no marker: no row', () => {
  renderGroup(bucketKeyEff());
  expect(screen.queryByTestId('conversion-status')).toBeNull();
});

test('a bucket with the deprecated serve: Convert now removes the line', async () => {
  const user = userEvent.setup();
  renderGroup(bucketKeyEff({ conversion: view({ run: undefined, open: false, serves_plaintext_deprecated: true }) }));
  expect(screen.queryByTestId('conversion-status')).toBeNull();
  const notice = screen.getByTestId('serve-deprecation-notice');
  expect(notice).toHaveTextContent(/next release no longer accepts/);
  await user.click(within(notice).getByRole('button', { name: 'Convert now' }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  expect(http.callsTo('PUT', SECTION)[0].body).toEqual({
    buckets: { 'db-archive': { encryption: { plaintext_reads: null } } },
  });
});

test('a backend card: one row per bucket in conversion, and Convert now for a deprecated serve', async () => {
  const onConvertNow = vi.fn(async () => {});
  const user = userEvent.setup();
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="hetzner-fsn1"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k', shim_active: false, serves_plaintext_deprecated: true }}
      onApply={vi.fn()}
      onConvertNow={onConvertNow}
      conversions={[
        { bucket: 'releases', view: view({ job_on_this_node: { id: 3, status: 'running', percent: 10 } }) },
        { bucket: 'downloads', view: view({ open: false }) },
      ]}
    />,
  );
  const rows = screen.getAllByTestId('conversion-status');
  expect(rows.map((r) => r.getAttribute('data-state'))).toEqual(['running', 'stopped']);
  // The bucket is a mono label; the state a sentence-case sentence.
  expect(within(rows[0]).getByText('releases').tagName).toBe('CODE');
  expect(rows[0]).toHaveTextContent(/^releasesEncrypting the existing objects, 10%\./);
  expect(within(rows[1]).getByText('downloads').tagName).toBe('CODE');
  expect(rows[1]).toHaveTextContent(/^downloadsThe conversion stopped before it finished; .*\./);
  await user.click(within(screen.getByTestId('serve-deprecation-notice')).getByRole('button', { name: 'Convert now' }));
  expect(onConvertNow).toHaveBeenCalledTimes(1);
});

test('turning on a backend key over objects names the count first; Cancel sends nothing', async () => {
  const backend = {
    name: 'hetzner-fsn1', backend_type: 's3', path: null, endpoint: 'https://fsn1.example', region: 'eu-central-1',
    force_path_style: true, has_credentials: true,
    encryption: { mode: 'none', has_key: false, shim_active: false },
  };
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75 }));
  http.on('GET', '/_/api/admin/backends', json({ backends: [backend], default_backend: 'hetzner-fsn1' }));
  http.on('GET', '/_/api/admin/buckets', json({
    buckets: [{ name: 'releases', backend_name: 'hetzner-fsn1', effective_encryption: eff() }],
  }));
  http.on('GET', SECTION, json({ backends: [{ name: 'hetzner-fsn1', type: 's3' }] }));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('POST', VALIDATE, json({
    ok: true,
    conversion_preview: [
      { bucket: 'releases', objects: 10000, more: true, versioned: false },
      { bucket: 'downloads', objects: 3, more: false, versioned: false },
    ],
  }));
  const user = userEvent.setup();
  renderWithQuery(<BackendsPanel />);
  await user.click(within(await screen.findByTestId('encryption-mode-select')).getByRole('combobox'));
  await user.click(await screen.findByText('AES-256-GCM (proxy-side)', { selector: '.ant-select-item-option-content' }));
  await user.click(screen.getByTestId('encryption-key-stored'));
  await user.click(screen.getByRole('button', { name: 'Apply encryption change' }));
  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent('Encrypt backend hetzner-fsn1?');
  expect(within(dialog).getByTestId('conversion-count')).toHaveTextContent(
    'More than 10,003 objects already in 2 buckets (releases, downloads) are not encrypted yet. After you apply, a re-encrypt job encrypts them in the background. They stay readable while the job runs.',
  );
  await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(http.callsTo('PUT', SECTION)).toHaveLength(0);
  // The edit stays, so the operator can apply after all.
  expect(screen.getByRole('button', { name: 'Apply encryption change' })).toBeInTheDocument();
});

/** The visual review's bug: a job that finished left "encrypting…" on the
 *  card until a reload. The bucket list refetches while a conversion runs,
 *  so the state follows the job, and goes away when the window closes. */
test('a running conversion follows the job to its close with no reload', async () => {
  let calls = 0;
  http.on('GET', '/_/api/admin/buckets', () => {
    calls += 1;
    const conversion = calls === 1 ? view({ job_on_this_node: { id: 7, status: 'running', percent: null } }) : undefined;
    return json({
      buckets: [{ name: 'db-archive', backend_name: 'hetzner-fsn1', effective_encryption: bucketKeyEff({ conversion }) }],
    });
  });
  function Harness() {
    const origins = useBucketOrigins();
    const e = origins.data?.buckets[0]?.effective_encryption;
    return e ? <BucketEncryptionGroup bucket="db-archive" eff={e} onApplied={vi.fn()} /> : null;
  }
  renderWithQuery(<Harness />);
  expect(await screen.findByTestId('conversion-status')).toHaveAttribute('data-state', 'running');
  await waitFor(() => expect(screen.queryByTestId('conversion-status')).toBeNull(), { timeout: 6_000 });
  expect(screen.getByText(/^Now:/)).toHaveTextContent(/^Now: .*\.$/);
  const after = calls;
  // Closed: no more polling.
  await new Promise((r) => setTimeout(r, 3_500));
  expect(calls).toBe(after);
}, 15_000);
