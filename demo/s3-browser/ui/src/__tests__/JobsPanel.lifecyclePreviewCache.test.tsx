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
import JobsPanel from '../components/jobs/JobsPanel';
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
  http.on('POST', PREVIEW, json({
    rule_name: 'expire-nightlies', status: 'preview', objects_scanned: 40, objects_affected: 2, objects_skipped: 0,
    bytes_affected: 3 * 1024 * 1024, errors: 0,
    candidates: [candidate('nightly/app-0.9.tar', 1024 * 1024), candidate('nightly/app-0.8.tar', 2 * 1024 * 1024)],
    failures: [],
  }));
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
  renderWithQuery(<JobsPanel />);
  await screen.findByText('expire-nightlies');
  await openPreviewTab(user);
  expect(previews()).toBe(1);
  await closeDrawer(user);

  await openPreviewTab(user);
  expect(previews()).toBe(1);

  await user.click(screen.getByRole('button', { name: /Refresh preview/ }));
  await waitFor(() => expect(previews()).toBe(2));
});

test("Run now's confirmation reuses the cached preview", async () => {
  const user = userEvent.setup();
  renderWithQuery(<JobsPanel />);
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
  renderWithQuery(<JobsPanel />);
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
