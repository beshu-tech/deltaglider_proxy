/**
 * Prod, rule expire-old-qa-reports: "0 of 12,148 scanned objects would be
 * affected", and "Refresh preview" spun with no sign of progress; only the
 * time changed. The preview now says when the first object becomes due,
 * and while the proxy walks, the tab shows the walk's live counters.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const JOBS = '/_/api/admin/jobs';
const SECTION = '/_/api/admin/config/section/storage';
const PREVIEW = `/_/api/admin/jobs/${encodeURIComponent('lifecycle:expire-old-qa-reports')}/preview`;

const row: JobRow = {
  id: 'lifecycle:expire-old-qa-reports',
  kind: 'lifecycle',
  name: 'expire-old-qa-reports',
  scope: { bucket: 'releases', prefix: 'ror/e2e_reports/' },
  trigger: 'scheduled',
  enabled: true,
  paused: false,
  status: 'idle',
  status_raw: 'idle',
  progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
  detail: {},
};

const nothingDue = (nextDueAt?: string) => ({
  rule_name: 'expire-old-qa-reports', status: 'preview', objects_scanned: 12148, objects_affected: 0,
  objects_skipped: 12148, bytes_affected: 0, errors: 0, candidates: [], failures: [],
  computed_at: '2026-10-13T20:19:26Z', next_due_at: nextDueAt,
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', JOBS, json({ jobs: [row] }));
  http.on('GET', SECTION, json({ lifecycle: { enabled: true, rules: [] } }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', PREVIEW, json({ running: false, objects_scanned: 0, objects_affected: 0 }));
});
afterEach(() => {
  vi.unstubAllGlobals();
});

async function openPreviewTab(user: ReturnType<typeof userEvent.setup>) {
  renderWithQuery(<JobsPanel />);
  await screen.findByText('expire-old-qa-reports');
  const jobRow = screen.getByText('expire-old-qa-reports').closest('[role="row"]') as HTMLElement;
  await user.click(within(jobRow).getByRole('button', { name: 'Preview: expire-old-qa-reports' }));
  await screen.findByRole('tab', { name: 'Preview', selected: true });
}

test('when nothing matches, the preview says when the first object becomes due', async () => {
  const due = new Date(Date.now() + 13 * 3_600_000 + 60_000).toISOString();
  http.on('POST', PREVIEW, json(nothingDue(due)));
  await openPreviewTab(userEvent.setup());
  expect(
    await screen.findByText(
      `Nothing to do now. The first object becomes due on ${new Date(due).toLocaleString()} (in ~13 h).`,
    ),
  ).toBeInTheDocument();
  const at = new Date('2026-10-13T20:19:26Z').toLocaleTimeString();
  expect(screen.getByText(`Scanned 12,148 objects at ${at}.`)).toBeInTheDocument();
});

test('while the proxy walks, the tab shows how many objects it scanned so far', async () => {
  let release: () => void = () => {};
  const gate = new Promise<void>((r) => { release = r; });
  http.on('POST', PREVIEW, async () => {
    await gate;
    return json(nothingDue());
  });
  let polls = 0;
  http.on('GET', PREVIEW, () => {
    polls += 1;
    return json({ running: true, started_at: '2026-10-13T20:19:00Z', objects_scanned: polls * 4210, objects_affected: 0 });
  });
  await openPreviewTab(userEvent.setup());
  expect(await screen.findByText('Scanning… 4,210 objects so far.')).toBeInTheDocument();
  // The counter advances: the tab reads it again while the walk runs.
  expect(await screen.findByText('Scanning… 8,420 objects so far.', {}, { timeout: 3000 })).toBeInTheDocument();
  await act(async () => release());
  await waitFor(() => expect(screen.queryByText(/^Scanning…/)).toBeNull());
  expect(screen.getByText('Nothing to do now: no scanned object matches the rule.')).toBeInTheDocument();
});
