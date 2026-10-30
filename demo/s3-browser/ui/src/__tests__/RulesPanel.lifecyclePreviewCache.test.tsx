/**
 * Cockroach scan 2026-10-10, UI finding E: a lifecycle preview walks the
 * rule's whole scope with a HEAD per object, and the UI recomputed it on
 * every open of the Preview tab and again in the Run-now confirmation
 * (staleTime 0, plus an invalidation on each Preview click). The preview now
 * stays fresh for a few minutes: reopening shows it again, the Run-now
 * confirmation reuses it, and "Refresh preview" recomputes it. A run, or an
 * apply of the storage section (the rule may have changed), drops it.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { QueryClient } from '@tanstack/react-query';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import { LifecycleRulesPanel } from '../components/jobs/RulesPanel';
import { applySection } from '../applySection';
import { qk } from '../queries/keys';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const JOBS = '/_/api/admin/jobs';
const SECTION = '/_/api/admin/config/section/storage';
const ID = 'lifecycle%3Aexpire-nightlies';
const RUN_NOW = `/_/api/admin/jobs/${ID}/run-now`;
const PREVIEW = `/_/api/admin/jobs/${ID}/preview`;

const row: JobRow = {
  id: 'lifecycle:expire-nightlies',
  kind: 'lifecycle',
  name: 'expire-nightlies',
  scope: { bucket: 'releases' },
  trigger: 'scheduled',
  enabled: true,
  paused: false,
  status: 'idle',
  status_raw: 'idle',
  progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
  detail: {},
};

const candidate = (key: string, size: number) => ({
  bucket: 'releases', key, action: 'delete', delete_source_after_success: false, created_at: '2026-01-01T00:00:00Z', size,
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', JOBS, json({ jobs: [row] }));
  http.on('GET', SECTION, json({ lifecycle: { enabled: true, rules: [] } }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  // A walk that just ended: the run-now confirmation may reuse it.
  http.on('POST', PREVIEW, () => json({
    rule_name: 'expire-nightlies', status: 'preview', objects_scanned: 40, objects_affected: 2, objects_skipped: 0,
    bytes_affected: 3 * 1024 * 1024, errors: 0,
    candidates: [candidate('nightly/app-0.9.tar', 1024 * 1024), candidate('nightly/app-0.8.tar', 2 * 1024 * 1024)],
    failures: [], computed_at: new Date().toISOString(),
  }));
  http.on('GET', `/_/api/admin/jobs/${ID}/runs`, json({ runs: [] }));
  http.on('POST', RUN_NOW, json({ started: true }, 202));
});
afterEach(() => {
  vi.unstubAllGlobals();
});

const previews = () => http.callsTo('POST', PREVIEW).length;

function jobRow(): HTMLElement {
  return screen.getByText('expire-nightlies').closest('[role="row"]') as HTMLElement;
}

async function openPreviewTab(user: ReturnType<typeof userEvent.setup>) {
  await user.click(within(jobRow()).getByRole('button', { name: 'Preview: expire-nightlies' }));
  await screen.findByRole('tab', { name: 'Preview', selected: true });
  expect(await screen.findByText('nightly/app-0.9.tar')).toBeInTheDocument();
}

async function closeDrawer(user: ReturnType<typeof userEvent.setup>) {
  await user.keyboard('{Escape}');
  await waitFor(() => expect(screen.queryByRole('tab', { name: 'Preview' })).toBeNull());
}

test('reopening the Preview tab shows the same preview; Refresh preview recomputes it', async () => {
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await screen.findByText('expire-nightlies');
  await openPreviewTab(user);
  expect(previews()).toBe(1);
  await closeDrawer(user);

  await openPreviewTab(user);
  expect(previews()).toBe(1);

  await user.click(screen.getByRole('button', { name: /Refresh preview/ }));
  await waitFor(() => expect(previews()).toBe(2));
  // The proxy keeps a finished walk for a minute: Refresh asks for a new one.
  expect(http.callsTo('POST', PREVIEW)[1].path).toBe(`${PREVIEW}?fresh=true`);
  expect(http.callsTo('POST', PREVIEW)[0].path).toBe(PREVIEW);
});

test('Refresh preview is disabled while a walk runs, and the time shown is when the proxy computed it', async () => {
  let release: () => void = () => {};
  const gate = new Promise<void>((r) => { release = r; });
  http.on('POST', PREVIEW, async () => {
    await gate;
    return json({
      rule_name: 'expire-nightlies', status: 'preview', objects_scanned: 40, objects_affected: 0, objects_skipped: 0,
      bytes_affected: 0, errors: 0, candidates: [], failures: [], computed_at: '2026-10-13T10:00:00Z',
    });
  });
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await screen.findByText('expire-nightlies');
  await user.click(within(jobRow()).getByRole('button', { name: 'Preview: expire-nightlies' }));
  await screen.findByRole('tab', { name: 'Preview', selected: true });
  const refresh = await screen.findByRole('button', { name: /Refresh preview/ });
  expect(refresh).toBeDisabled();
  await act(async () => release());
  await waitFor(() => expect(screen.getByRole('button', { name: /Refresh preview/ })).toBeEnabled());
  const shown = new Date('2026-10-13T10:00:00Z').toLocaleTimeString();
  expect(screen.getByText((t) => t.startsWith(`Scanned 40 objects at ${shown} (`))).toBeInTheDocument();
});

test("Run now's confirmation reuses the cached preview", async () => {
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await screen.findByText('expire-nightlies');
  await openPreviewTab(user);
  await closeDrawer(user);

  await user.click(within(jobRow()).getByRole('button', { name: 'Run now: expire-nightlies' }));
  const dialog = await screen.findByRole('dialog');
  expect(await within(dialog).findByText('nightly/app-0.9.tar')).toBeInTheDocument();
  expect(within(dialog).getByRole('button', { name: 'Run: delete 2 objects' })).toBeEnabled();
  expect(previews()).toBe(1);
});

test('after a run the next confirmation computes a new preview', async () => {
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await screen.findByText('expire-nightlies');
  await user.click(within(jobRow()).getByRole('button', { name: 'Run now: expire-nightlies' }));
  const dialog = await screen.findByRole('dialog');
  await user.click(await within(dialog).findByRole('button', { name: 'Run: delete 2 objects' }));
  await waitFor(() => expect(http.callsTo('POST', RUN_NOW)).toHaveLength(1));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(previews()).toBe(1);

  await user.click(within(jobRow()).getByRole('button', { name: 'Run now: expire-nightlies' }));
  await within(await screen.findByRole('dialog')).findByText('nightly/app-0.9.tar');
  await waitFor(() => expect(previews()).toBe(2));
});

test('applying the storage section (the lifecycle rules live there) drops the cached previews', async () => {
  http.on('PUT', SECTION, () => new Response(JSON.stringify({ ok: true }), {
    status: 200, headers: { 'content-type': 'application/json', etag: '"v2"' },
  }));
  const qc = new QueryClient();
  const key = qk.jobs.preview('lifecycle:expire-nightlies');
  qc.setQueryData(key, { rule_name: 'expire-nightlies' });
  expect(qc.getQueryState(key)?.isInvalidated).toBe(false);
  await act(async () => {
    await applySection(qc, 'storage', { lifecycle: { enabled: true, rules: [] } }, '"v1"');
  });
  expect(qc.getQueryState(key)?.isInvalidated).toBe(true);
  qc.clear();
});

/**
 * #1: the confirmation said "delete 46" from a walk that the proxy had
 * cached for 29 s, and the run deleted 80. A preview older than a few
 * seconds is walked again (`?fresh=true`) before the button names a
 * count, and the dialog says how old the walk is.
 */
test("Run now's confirmation walks again when its preview is old, and says how old", async () => {
  let fresh = 0;
  http.on('POST', PREVIEW, (req) => {
    const isFresh = req.path.endsWith('?fresh=true');
    if (isFresh) fresh += 1;
    return json({
      rule_name: 'expire-nightlies', status: 'preview', objects_scanned: 90,
      objects_affected: isFresh ? 80 : 46, objects_deleted: isFresh ? 80 : 46, objects_skipped: 10,
      objects_not_due: 8, objects_excluded: 2, bytes_affected: 1024, errors: 0,
      candidates: [candidate('nightly/app-0.9.tar', 1024)], failures: [],
      computed_at: isFresh ? new Date().toISOString() : new Date(Date.now() - 29_000).toISOString(),
    });
  });
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await screen.findByText('expire-nightlies');
  await user.click(within(jobRow()).getByRole('button', { name: 'Run now: expire-nightlies' }));
  const dialog = await screen.findByRole('dialog');
  expect(await within(dialog).findByRole('button', { name: 'Run: delete 80 objects' })).toBeEnabled();
  expect(fresh).toBe(1);
  expect(within(dialog).queryByRole('button', { name: 'Run: delete 46 objects' })).toBeNull();
  expect(within(dialog).getByText((t) => t.startsWith('Scanned 90 objects at') && t.includes('s ago)'))).toBeInTheDocument();
  expect(
    within(dialog).getByText('Of the other scanned objects, 8 are not old enough yet, 2 are kept out by a glob.'),
  ).toBeInTheDocument();
  await user.click(within(dialog).getByRole('button', { name: /Refresh preview/ }));
  await waitFor(() => expect(fresh).toBe(2));
});

/** #15: when the run that the operator started ends, a toast says what it did. */
test('a run-now ends with a toast that counts what it did', async () => {
  // The run ends at once: the row's last run moves past the one at click time.
  let ran = false;
  http.on('POST', RUN_NOW, () => {
    ran = true;
    return json({ started: true }, 202);
  });
  http.on('GET', JOBS, () => json({ jobs: [ran ? { ...row, last_run_at: Math.floor(Date.now() / 1000) } : row] }));
  http.on('GET', `/_/api/admin/jobs/${ID}/runs`, () => json({
    runs: [{
      id: 7, triggered_by: 'run-now', started_at: Math.floor(Date.now() / 1000), finished_at: Math.floor(Date.now() / 1000),
      status: 'succeeded', status_raw: 'succeeded', objects_scanned: 40, objects_processed: 2, objects_skipped: 38,
      objects_deleted: 2, objects_moved: 0, objects_copied: 0, bytes: 3, errors: 0,
    }],
  }));
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await screen.findByText('expire-nightlies');
  await user.click(within(jobRow()).getByRole('button', { name: 'Run now: expire-nightlies' }));
  const dialog = await screen.findByRole('dialog');
  await user.click(await within(dialog).findByRole('button', { name: 'Run: delete 2 objects' }));
  expect(await screen.findByText('Lifecycle rule "expire-nightlies" finished: deleted 2.')).toBeInTheDocument();
});
