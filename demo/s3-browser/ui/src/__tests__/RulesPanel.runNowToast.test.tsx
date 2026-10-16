/**
 * Review 2d, UI finding 3: one lifecycle Run now gave two "finished" toasts.
 * The confirmation dialog watched the run on its own and compared the
 * browser clock with the server's started_at, and the rules page watched the row
 * too. Now the rules page is the one watcher (server clock only), and its toast
 * names what the run did, from the run record.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import { LifecycleRulesPanel } from '../components/jobs/RulesPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const base: JobRow = {
  id: 'lifecycle:expire-nightly-dumps', kind: 'lifecycle', name: 'expire-nightly-dumps',
  scope: { bucket: 'db-archive', prefix: 'nightly/' }, trigger: 'scheduled', enabled: true, paused: false,
  status: 'idle', status_raw: 'idle', progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 }, detail: {},
  last_run_at: 1000,
};
let row = base;
let runs: unknown[] = [];
/** Seconds that the server clock is behind the browser clock. */
let skew = 0;
beforeEach(() => {
  row = base;
  runs = [];
  skew = 0;
  const http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({ lifecycle: { enabled: true, tick_interval: '1h', max_failures_retained: 100, rules: [] } }));
  http.on('GET', '/_/api/admin/jobs', () => json({ jobs: [row] }));
  http.on('GET', /\/runs$/, () => json({ runs }));
  http.on('GET', /\/failures$/, json({ failures: [] }));
  http.on('POST', /\/preview/, () => json({
    computed_at: new Date().toISOString(), objects_scanned: 1, objects_affected: 1, objects_deleted: 1, bytes_affected: 1,
    stored_bytes_affected: 1,
    candidates: [{ bucket: 'db-archive', key: 'nightly/a', action: 'delete', size: 1, stored_size: 1, created_at: new Date().toISOString(), delete_source_after_success: false }],
  }));
  http.on('POST', /\/run-now$/, () => {
    const now = Math.floor(Date.now() / 1000) - skew;
    row = { ...base, last_run_at: now + 1, progress: { processed: 1, bytes: 1, failed: 0, skipped: 0 } };
    runs = [{
      id: 9, started_at: now, finished_at: now + 1, status: 'succeeded', status_raw: 'succeeded', objects_scanned: 1,
      objects_skipped: 0, bytes: 1, triggered_by: 'manual', objects_processed: 1, objects_deleted: 1, errors: 0,
    }];
    return json({ started: true }, 202);
  });
});
afterEach(() => vi.unstubAllGlobals());

// Every "finished" toast seen since the test started. A toast closes after a
// few seconds, so the test collects them as they show instead of counting
// the ones still open at the end.
let seen: string[] = [];
let collector: ReturnType<typeof setInterval> | undefined;
beforeEach(() => {
  seen = [];
  collector = setInterval(() => {
    for (const n of Array.from(document.querySelectorAll('.ant-message-notice'))) {
      const t = n.textContent ?? '';
      if (/finished/.test(t) && !seen.includes(t)) seen.push(t);
    }
  }, 50);
});
afterEach(() => clearInterval(collector));
const finishedToasts = () => seen;

async function runNow() {
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  const r = (await screen.findByText('expire-nightly-dumps')).closest('[role="row"]') as HTMLElement;
  await user.click(await within(r).findByRole('button', { name: 'Run now: expire-nightly-dumps' }));
  const dlg = await screen.findByRole('dialog');
  const ok = await within(dlg).findByRole('button', { name: 'Run: delete 1 object' }, { timeout: 4000 });
  await waitFor(() => expect(ok).toBeEnabled(), { timeout: 4000 });
  await user.click(ok);
}

test('one lifecycle Run now gives one "finished" toast, with the run counts', async () => {
  await runNow();
  await waitFor(() => expect(finishedToasts()).toHaveLength(1), { timeout: 6000 });
  // A second watcher would add its toast within its 2 s refetch.
  await new Promise((r) => setTimeout(r, 3000));
  expect(finishedToasts()).toEqual(['Lifecycle rule "expire-nightly-dumps" finished: deleted 1.']);
}, 20000);

test('the toast does not depend on the browser clock', async () => {
  skew = 600;
  await runNow();
  await waitFor(() => expect(finishedToasts()).toHaveLength(1), { timeout: 6000 });
  expect(finishedToasts()).toEqual(['Lifecycle rule "expire-nightly-dumps" finished: deleted 1.']);
}, 20000);
