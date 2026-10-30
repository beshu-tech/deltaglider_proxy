/**
 * UX harvest cycle 2 on the lifecycle rules page:
 * - C22: Delete sent POST /jobs/<id>/delete at once, while every other rule
 *   change waits for Review & apply. Remove is now a staged edit too.
 * - C3: a bad duration showed no error under the field.
 * - C50: a new rule landed disabled while the scheduler was off, and
 *   nothing said so.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { LifecycleRulesPanel } from '../components/jobs/RulesPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/storage';
const RULE = {
  name: 'expire-nightly-dumps', enabled: true, bucket: 'db-archive', prefix: 'nightly/', action: 'delete',
  expire_after: '30d', include_globs: [], exclude_globs: ['.deltaglider/**'], batch_size: 100,
};
const ROW = {
  id: 'lifecycle:expire-nightly-dumps', kind: 'lifecycle', name: 'expire-nightly-dumps',
  scope: { bucket: 'db-archive', prefix: 'nightly/' }, trigger: 'scheduled', enabled: true, paused: false,
  status: 'idle', status_raw: 'idle', progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 }, detail: {},
};
let http: ReturnType<typeof mockFetch>;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [ROW], workers: { lifecycle: { enabled: true } } }));
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', SECTION, json({ lifecycle: { enabled: true, tick_interval: '1h', max_failures_retained: 100, rules: [RULE] } }));
});
afterEach(() => vi.unstubAllGlobals());

const rowOf = (name: string) =>
  screen.getAllByText(name).map((t) => t.closest('[role="row"]')).find(Boolean) as HTMLElement;

test('Remove rule is staged: no request, a "removing" row, the dirty bar; Keep undoes it', async () => {
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await screen.findByText('expire-nightly-dumps');
  await waitFor(() => expect(http.callsTo('GET', SECTION).length).toBeGreaterThan(0));

  await user.click(within(rowOf('expire-nightly-dumps')).getByRole('button', { name: /^More actions for / }));
  await user.click(await screen.findByRole('menuitem', { name: /Remove rule/ }));
  const ok = await screen.findAllByRole('button', { name: 'Remove rule' });
  await user.click(ok[ok.length - 1]);

  expect(await within(rowOf('expire-nightly-dumps')).findByText('removing')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Review & apply' })).toBeEnabled();
  expect(http.calls.filter((c) => c.method === 'POST')).toHaveLength(0);

  await user.click(within(rowOf('expire-nightly-dumps')).getByRole('button', { name: /^More actions for / }));
  await user.click(await screen.findByRole('menuitem', { name: /Keep this rule/ }));
  await waitFor(() => expect(within(rowOf('expire-nightly-dumps')).queryByText('removing')).toBeNull());
  await waitFor(() => expect(screen.queryByRole('button', { name: 'Review & apply' })).toBeNull());
});

test('a bad Expire after shows an error under the field and blocks the apply', async () => {
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await user.click(await screen.findByText('expire-nightly-dumps'));
  const drawer = await screen.findByRole('dialog');
  const field = within(drawer).getByRole('textbox', { name: 'Expire after' });
  await user.clear(field);
  await user.type(field, 'notaduration');
  await user.tab();
  expect(field).toHaveAttribute('aria-invalid', 'true');
  expect(within(drawer).getByText('"notaduration" is not a duration. Use a duration such as 30d, 12h or 90m.')).toBeInTheDocument();
  expect(within(drawer).getByText('1 issue to fix')).toBeInTheDocument();
  expect(within(drawer).getByRole('button', { name: 'Review & apply' })).toBeDisabled();
});

test('a new rule is enabled; with the scheduler off, the page says it will not run', async () => {
  http.on('GET', SECTION, json({ lifecycle: { enabled: false, tick_interval: '1h', max_failures_retained: 100, rules: [] } }));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  await waitFor(() => expect(http.callsTo('GET', SECTION).length).toBeGreaterThan(0));
  expect(screen.queryByTestId('rules-scheduler-off')).toBeNull();
  await user.click(await screen.findByRole('button', { name: /New lifecycle rule/ }));
  const drawer = await screen.findByRole('dialog');
  expect(within(drawer).getByRole('switch', { name: 'Enabled' })).toBeChecked();
  expect(screen.getByTestId('rules-scheduler-off')).toHaveTextContent(
    'No lifecycle rule runs while “Run lifecycle rules on schedule” is off.',
  );
});
