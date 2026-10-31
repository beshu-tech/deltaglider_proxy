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
import PurgeVersionsBox from '../components/PurgeVersionsBox';
import type { ConversionView, EffectiveEncryption } from '../adminApi';
import { NavigationContext } from '../NavigationContext';
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
    /keeps old versions of bucket db-archive.*old unencrypted versions stay/,
  );
  // The old "refuse after the job" advice is gone.
  expect(dialog).not.toHaveTextContent(/Refuse unencrypted objects/);
});

/** Open the bucket dialog of a new bucket key, with `versioned` previews. */
async function openBucketDialog(versioned: boolean) {
  http.on(
    'POST',
    VALIDATE,
    json({
      ok: true,
      diff: { storage: { 'buckets.db-archive.encryption.mode': { before: 'none', after: 'aes256-gcm-proxy' } } },
      conversion_preview: [{ bucket: 'db-archive', objects: 3, more: false, versioned }],
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
  return { user, dialog: await screen.findByRole('dialog') };
}

const PURGE_HEADER = 'x-dgp-purge-plaintext-versions';

test('a versioned bucket: the purge box is unchecked, and the apply asks for no purge', async () => {
  const { user, dialog } = await openBucketDialog(true);
  const box = within(dialog).getByRole('checkbox', { name: /^Also delete the old unencrypted versions/ });
  expect(box).not.toBeChecked();
  // A destructive opt-in never takes the initial focus: one Space press
  // must not tick a permanent delete.
  expect(document.activeElement).not.toBe(box);
  expect(dialog).toHaveTextContent(/This cannot be undone/);
  // The versioning note is neutral; the box under it resolves it.
  expect(within(dialog).getByTestId('conversion-versioned')).toHaveClass('ant-typography-secondary');
  await user.click(within(dialog).getByTestId('apply-dialog-confirm'));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  expect(http.callsTo('PUT', SECTION)[0].headers[PURGE_HEADER]).toBeUndefined();
});

test('a versioned bucket: a ticked purge box sends the purge header with the apply', async () => {
  const { user, dialog } = await openBucketDialog(true);
  await user.click(within(dialog).getByRole('checkbox', { name: /^Also delete the old unencrypted versions/ }));
  await user.click(within(dialog).getByTestId('apply-dialog-confirm'));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  expect(http.callsTo('PUT', SECTION)[0].headers[PURGE_HEADER]).toBe('true');
});

test('the purge box never takes the focus when it mounts', () => {
  // jsdom runs no dialog focus management, so the dialog tests above only
  // show that focus is not on the box; this proves the box itself never
  // asks for it (an autoFocus here fails).
  renderWithQuery(<PurgeVersionsBox onChange={vi.fn()} />);
  const box = screen.getByRole('checkbox', { name: /^Also delete the old unencrypted versions/ });
  expect(box).not.toBeChecked();
  expect(document.activeElement).not.toBe(box);
});

test('a bucket without versions offers no purge box', async () => {
  const { dialog } = await openBucketDialog(false);
  expect(within(dialog).queryByRole('checkbox', { name: /^Also delete the old unencrypted versions/ })).toBeNull();
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
  expect(within(row).queryByRole('button', { name: /^Resume/ })).toBeNull();
  await user.click(within(row).getByRole('button', { name: 'View the run of db-archive' }));
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
  expect(row).toHaveTextContent(/this proxy runs no re-encrypt job/);
  await user.click(within(row).getByRole('button', { name: 'Resume the conversion of db-archive' }));
  await waitFor(() => expect(http.callsTo('POST', REENCRYPT)).toHaveLength(1));
  expect(http.callsTo('POST', REENCRYPT)[0].body).toEqual({ buckets: ['db-archive'] });
});

test('a failed job offers Resume', () => {
  renderGroup(bucketKeyEff({ conversion: view({ job_on_this_node: { id: 7, status: 'failed', percent: null } }) }));
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveAttribute('data-state', 'resume');
  expect(row).toHaveTextContent(/stopped with failures/);
  expect(within(row).getByRole('button', { name: 'Resume the conversion of db-archive' })).toBeInTheDocument();
});

test('deadline passed: a warning that the conversion stopped before it finished, and Resume', () => {
  renderGroup(
    bucketKeyEff({ conversion: view({ open: false, job_on_this_node: { id: 7, status: 'cancelled', percent: null } }) }),
  );
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveAttribute('data-state', 'stopped');
  expect(row).toHaveTextContent('the conversion stopped before it finished');
  expect(row.querySelector('.ant-typography-warning')).not.toBeNull();
  expect(within(row).getByRole('button', { name: 'Resume the conversion of db-archive' })).toBeInTheDocument();
});

test('objects stored after the cutoff are counted, with a link to the run\'s failures', async () => {
  const user = userEvent.setup();
  const navigate = renderGroup(
    bucketKeyEff({
      conversion: view({ adopted_after_cutoff: 3, job_on_this_node: { id: 7, status: 'running', percent: 80 } }),
    }),
  );
  const row = screen.getByTestId('conversion-status');
  await user.click(within(row).getByRole('button', { name: '3 objects were stored after encryption started; they were encrypted too (see the run)' }));
  expect(navigate).toHaveBeenCalledWith('/_/admin/jobs?job=maintenance%3A7&tab=failures');
});

test('no marker: no row', () => {
  renderGroup(bucketKeyEff());
  expect(screen.queryByTestId('conversion-status')).toBeNull();
});

test('a bucket with the deprecated serve: Convert now removes the line', async () => {
  http.on('POST', VALIDATE, json({ ok: true, diff: { storage: { 'buckets.db-archive.encryption.plaintext_reads': { before: 'serve', after: null } } } }));
  const user = userEvent.setup();
  renderGroup(bucketKeyEff({ conversion: view({ run: undefined, open: false, serves_plaintext_deprecated: true }) }));
  expect(screen.queryByTestId('conversion-status')).toBeNull();
  const notice = screen.getByTestId('serve-deprecation-notice');
  expect(notice).toHaveTextContent(/next release no longer accepts/);
  await user.click(within(notice).getByRole('button', { name: 'Convert now' }));
  await user.click(within(await screen.findByRole('dialog')).getByTestId('apply-dialog-confirm'));
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

test('a backend over a versioned bucket: the ticked purge box sends the purge header', async () => {
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
    conversion_preview: [{ bucket: 'releases', objects: 5, more: false, versioned: true }],
  }));
  const user = userEvent.setup();
  renderWithQuery(<BackendsPanel />);
  await user.click(within(await screen.findByTestId('encryption-mode-select')).getByRole('combobox'));
  await user.click(await screen.findByText('AES-256-GCM (proxy-side)', { selector: '.ant-select-item-option-content' }));
  await user.click(screen.getByTestId('encryption-key-stored'));
  await user.click(screen.getByRole('button', { name: 'Apply encryption change' }));
  const dialog = await screen.findByRole('dialog');
  const box = within(dialog).getByRole('checkbox', { name: /^Also delete the old unencrypted versions/ });
  expect(box).not.toBeChecked();
  expect(document.activeElement).not.toBe(box);
  await user.click(box);
  await user.click(within(dialog).getByRole('button', { name: 'Encrypt' }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  expect(http.callsTo('PUT', SECTION)[0].headers['x-dgp-purge-plaintext-versions']).toBe('true');
});

/** The visual review's bug: a job that finished left "encrypting…" on the
 *  card until a reload. The refetch itself runs on fake timers in
 *  useBucketOrigins.polling.test.tsx; here, the card follows a new
 *  answer: the status goes away when the conversion ends. */
test('a running conversion goes away from the card when the list says it ended', () => {
  const running = bucketKeyEff({ conversion: view({ job_on_this_node: { id: 7, status: 'running', percent: null } }) });
  const { rerender } = renderWithQuery(
    <NavigationContext.Provider value={{ navigate: vi.fn(), subPath: '' } as never}>
      <BucketEncryptionGroup bucket="db-archive" eff={running} onApplied={vi.fn()} />
    </NavigationContext.Provider>,
  );
  expect(screen.getByTestId('conversion-status')).toHaveAttribute('data-state', 'running');
  rerender(
    <NavigationContext.Provider value={{ navigate: vi.fn(), subPath: '' } as never}>
      <BucketEncryptionGroup bucket="db-archive" eff={bucketKeyEff()} onApplied={vi.fn()} />
    </NavigationContext.Provider>,
  );
  expect(screen.queryByTestId('conversion-status')).toBeNull();
  expect(screen.getByText(/^Now:/)).toHaveTextContent(/^Now: .*\.$/);
});

// ---------------------------------------------------------------------------
// Review of 2026-10-31 (one test per confirmed finding).

/** The date as the UI writes it ("readable until <date>"). */
function untilDate(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
}

/** Finding 1: an env var sets the key, so the encryption is read-only; a
 *  Resume is a job request, not a config write, and stays. */
test('review 1: a bucket whose key comes from an env var keeps Resume', () => {
  renderGroup(bucketKeyEff({ key_env: 'DGP_BUCKET_DB_ARCHIVE_KEY', conversion: view({ open: false }) }));
  expect(screen.getByTestId('conversion-status')).toHaveAttribute('data-state', 'stopped');
  expect(screen.getByRole('button', { name: /^Resume/ })).toBeInTheDocument();
});

test('review 1: a backend whose key comes from an env var keeps Resume', () => {
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="hetzner-fsn1"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k', shim_active: false }}
      onApply={vi.fn()}
      readOnly
      conversions={[{ bucket: 'releases', view: view() }]}
    />,
  );
  expect(screen.getByRole('button', { name: /^Resume/ })).toBeInTheDocument();
});

/** Finding 2: Resume of a versioned bucket asks about the old versions
 *  and sends the choice; elsewhere it stays one click. */
test('review 2: Resume of a versioned bucket asks about the old versions and sends the choice', async () => {
  const user = userEvent.setup();
  renderGroup(bucketKeyEff({ conversion: view({ versioned: true }) }));
  await user.click(screen.getByRole('button', { name: /^Resume/ }));
  const dialog = await screen.findByRole('dialog');
  const box = within(dialog).getByRole('checkbox', { name: /^Also delete the old unencrypted versions/ });
  expect(box).not.toBeChecked();
  expect(document.activeElement).not.toBe(box);
  expect(http.callsTo('POST', REENCRYPT)).toHaveLength(0);
  await user.click(box);
  await user.click(within(dialog).getByRole('button', { name: 'Resume' }));
  await waitFor(() => expect(http.callsTo('POST', REENCRYPT)).toHaveLength(1));
  expect(http.callsTo('POST', REENCRYPT)[0].body).toEqual({ buckets: ['db-archive'], purge_plaintext_versions: true });
});

test('review 2: Resume of a bucket without versions is one click', async () => {
  const user = userEvent.setup();
  renderGroup(bucketKeyEff({ conversion: view({ versioned: false }) }));
  await user.click(screen.getByRole('button', { name: /^Resume/ }));
  await waitFor(() => expect(http.callsTo('POST', REENCRYPT)).toHaveLength(1));
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(http.callsTo('POST', REENCRYPT)[0].body).toEqual({ buckets: ['db-archive'] });
});

/** Finding 3: Convert now starts a conversion like turning encryption on,
 *  so it goes through the same validate and review. */
test('review 3: bucket Convert now validates, shows the review dialog, then applies', async () => {
  http.on('POST', VALIDATE, json({
    ok: true,
    diff: { storage: { 'buckets.db-archive.encryption.plaintext_reads': { before: 'serve', after: null } } },
    conversion_preview: [{ bucket: 'db-archive', objects: 3, more: false, versioned: true }],
  }));
  const user = userEvent.setup();
  renderGroup(bucketKeyEff({ conversion: view({ run: undefined, open: false, serves_plaintext_deprecated: true }) }));
  await user.click(within(screen.getByTestId('serve-deprecation-notice')).getByRole('button', { name: 'Convert now' }));
  const dialog = await screen.findByRole('dialog');
  expect(http.callsTo('POST', VALIDATE)).toHaveLength(1);
  expect(http.callsTo('PUT', SECTION)).toHaveLength(0);
  expect(within(dialog).getByTestId('conversion-count')).toHaveTextContent(/^The 3 objects already in bucket db-archive/);
  await user.click(within(dialog).getByRole('checkbox', { name: /^Also delete the old unencrypted versions/ }));
  await user.click(within(dialog).getByTestId('apply-dialog-confirm'));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  expect(http.callsTo('PUT', SECTION)[0].body).toEqual({ buckets: { 'db-archive': { encryption: { plaintext_reads: null } } } });
  expect(http.callsTo('PUT', SECTION)[0].headers['x-dgp-purge-plaintext-versions']).toBe('true');
});

test('review 3: backend Convert now validates and asks first; Cancel sends nothing', async () => {
  const backend = {
    name: 'hetzner-fsn1', backend_type: 's3', path: null, endpoint: 'https://fsn1.example', region: 'eu-central-1',
    force_path_style: true, has_credentials: true,
    encryption: { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k', shim_active: false, serves_plaintext_deprecated: true },
  };
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75 }));
  http.on('GET', '/_/api/admin/backends', json({ backends: [backend], default_backend: 'hetzner-fsn1' }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', SECTION, json({ backends: [{ name: 'hetzner-fsn1', type: 's3', encryption: { mode: 'aes256-gcm-proxy', plaintext_reads: 'serve' } }] }));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('POST', VALIDATE, json({ ok: true, conversion_preview: [{ bucket: 'releases', objects: 5, more: false, versioned: false }] }));
  const user = userEvent.setup();
  renderWithQuery(<BackendsPanel />);
  await user.click(within(await screen.findByTestId('serve-deprecation-notice')).getByRole('button', { name: 'Convert now' }));
  const dialog = await screen.findByRole('dialog');
  expect(http.callsTo('POST', VALIDATE)).toHaveLength(1);
  expect(within(dialog).getByTestId('conversion-count')).toHaveTextContent(/^The 5 objects already in bucket releases/);
  await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(http.callsTo('PUT', SECTION)).toHaveLength(0);
});

/** Finding 4: the versioning note shows once, in plain words. */
test('review 4: the bucket dialog shows the versioning note once, with no header name', async () => {
  const server =
    "bucket 'db-archive' keeps old versions on its backend (versioning is enabled or suspended): the job converts the current objects, and their plaintext versions stay on the backend as noncurrent versions, unless the write asks the job to delete them (the header X-DGP-Purge-Plaintext-Versions: true, or the box in the admin UI's dialog). A deleted version cannot be restored";
  http.on('POST', VALIDATE, json({
    ok: true,
    warnings: [server, 'another warning'],
    diff: { storage: { 'buckets.db-archive.encryption.mode': { before: 'none', after: 'aes256-gcm-proxy' } } },
    conversion_preview: [{ bucket: 'db-archive', objects: 3, more: false, versioned: true }],
  }));
  const user = userEvent.setup();
  renderGroup(eff());
  await user.click(screen.getByText('Encrypt with a bucket key'));
  const key = (screen.getByLabelText('Generated key') as HTMLTextAreaElement).value;
  await user.type(screen.getByLabelText('Last 6 characters of the key'), key.slice(-6));
  const review = screen.getByRole('button', { name: 'Review & apply' });
  await waitFor(() => expect(review).toBeEnabled());
  await user.click(review);
  const dialog = await screen.findByRole('dialog');
  expect(within(dialog).getAllByText(/keeps old versions/)).toHaveLength(1);
  expect(dialog).not.toHaveTextContent(/X-DGP-/);
  // Other warnings stay.
  expect(dialog).toHaveTextContent('another warning');
});

/** Finding 6: a cancelled job is not a failure; the count of objects
 *  stored after the start shows also when the job ran on another node. */
test('review 6: a cancelled job reads as cancelled, not as failures', () => {
  renderGroup(bucketKeyEff({ conversion: view({ job_on_this_node: { id: 7, status: 'cancelled', percent: 40 } }) }));
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveTextContent(/re-encrypt job was cancelled/);
  expect(row).not.toHaveTextContent(/failures/);
});

test('review 6: objects stored after encryption started are counted when the job ran elsewhere', () => {
  renderGroup(bucketKeyEff({ conversion: view({ adopted_after_cutoff: 5 }) }));
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveTextContent('5 objects were stored after encryption started; they were encrypted too');
  // No run here to link to.
  expect(within(row).queryByRole('button', { name: /stored after/ })).toBeNull();
});

/** Finding 7: unique names for per-bucket buttons, the date in the text,
 *  plain words, and the purge box's help tied to it. */
test('review 7: per-bucket buttons on the backend card have unique accessible names', () => {
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="hetzner-fsn1"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k', shim_active: false }}
      onApply={vi.fn()}
      conversions={[
        { bucket: 'releases', view: view({ job_on_this_node: { id: 3, status: 'failed', percent: 10 } }) },
        { bucket: 'downloads', view: view({ job_on_this_node: { id: 4, status: 'failed', percent: 10 } }) },
      ]}
    />,
  );
  expect(screen.getByRole('button', { name: 'Resume the conversion of releases' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Resume the conversion of downloads' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'View the run of releases' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'View the run of downloads' })).toBeInTheDocument();
});

test('review 7: the deadline is in the visible text, with no jargon', () => {
  renderGroup(bucketKeyEff({ conversion: view() }));
  const row = screen.getByTestId('conversion-status');
  expect(row).toHaveTextContent(`unencrypted objects stay readable until ${untilDate(1_793_604_800)}`);
  expect(row).not.toHaveTextContent(/window|cutoff|this instance/);
});

test('review 7: the purge box help is its description', () => {
  renderWithQuery(<PurgeVersionsBox onChange={vi.fn()} />);
  const box = screen.getByRole('checkbox', { name: /^Also delete the old unencrypted versions/ });
  expect(box).toHaveAccessibleDescription(/This cannot be undone/);
});
