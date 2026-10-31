/**
 * UX harvest cycle 3 on the rule drawers:
 * - D9: Batch size `-3` became 1 and `abc` became 1, silently.
 * - D10: a rule name with spaces showed nothing; Review & apply with no
 *   buckets did nothing visible (replication) or showed a toast while the
 *   Bucket field kept its normal border (lifecycle).
 * - D20: one apply showed two toasts ("Applied" and a scheduler warning).
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { LifecycleRulesPanel, ReplicationRulesPanel } from '../components/jobs/RulesPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/storage';
let http: ReturnType<typeof mockFetch>;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [], workers: { lifecycle: { enabled: false } } }));
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', SECTION, json({}));
});
afterEach(() => vi.unstubAllGlobals());

async function newRule(user: ReturnType<typeof userEvent.setup>, label: RegExp) {
  await user.click(await screen.findByRole('button', { name: label }));
  return screen.findByRole('dialog');
}

test('Review & apply with no buckets shows the errors under the fields, not a toast', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ReplicationRulesPanel />);
  const drawer = await newRule(user, /New replication rule/);
  // Nothing typed yet: no error is shown, and the bar offers the review.
  expect(within(drawer).queryByText('Choose the bucket to copy from.')).toBeNull();
  await user.click(within(drawer).getByRole('button', { name: 'Review & apply' }));
  expect(await within(drawer).findByText('Choose the bucket to copy from.')).toBeInTheDocument();
  expect(within(drawer).getByText('Choose the bucket to copy into.')).toBeInTheDocument();
  expect(within(drawer).getByText('2 issues to fix')).toBeInTheDocument();
  expect(within(drawer).getByRole('button', { name: 'Review & apply' })).toBeDisabled();
  expect(http.callsTo('POST', `${SECTION}/validate`)).toHaveLength(0);
});

test('a batch size of -3 or abc stays as typed, with an error; nothing is clamped', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ReplicationRulesPanel />);
  const drawer = await newRule(user, /New replication rule/);
  await user.click(within(drawer).getByText('Advanced'));
  const batch = within(drawer).getByRole('textbox', { name: 'Batch size' });
  for (const typed of ['-3', 'abc']) {
    await user.clear(batch);
    await user.type(batch, typed);
    await user.tab();
    expect(batch).toHaveValue(typed);
    expect(batch).toHaveAttribute('aria-invalid', 'true');
    expect(within(drawer).getByText('Enter a whole number between 1 and 10,000.')).toBeInTheDocument();
  }
  await user.clear(batch);
  await user.type(batch, '250');
  await user.tab();
  expect(batch).toHaveValue('250');
  expect(batch).not.toHaveAttribute('aria-invalid');
});

test('a rule name with spaces is kept as typed and refused under the field', async () => {
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  const drawer = await newRule(user, /New lifecycle rule/);
  const name = within(drawer).getByRole('textbox', { name: 'Rule name' });
  await user.clear(name);
  await user.type(name, 'qa bad name!');
  await user.tab();
  expect(name).toHaveValue('qa bad name!');
  expect(within(drawer).getByText('Remove the spaces. Use letters, digits, dots, dashes and underscores only.')).toBeInTheDocument();
});

test('one apply shows one toast, and the drawer closes', async () => {
  let saved: unknown[] = [];
  http.on('GET', SECTION, () => json({ lifecycle: { enabled: false, tick_interval: '1h', max_failures_retained: 100, rules: saved } }));
  http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: { storage: { 'lifecycle.rules': { before: null, after: [{ name: 'expire-old', bucket: 'releases' }] } } } }));
  http.on('PUT', SECTION, (call) => {
    saved = (call.body as { lifecycle: { rules: unknown[] } }).lifecycle.rules;
    return json({ ok: true, persisted_path: 'dg.yaml' });
  });
  const user = userEvent.setup();
  renderWithQuery(<LifecycleRulesPanel />);
  const drawer = await newRule(user, /New lifecycle rule/);
  // The scope is the one required field a new rule leaves empty.
  await user.type(within(drawer).getAllByRole('combobox')[0], 'releases');
  await user.click(within(drawer).getByRole('button', { name: 'Review & apply' }));
  await user.click(await screen.findByRole('button', { name: 'Apply and persist changes' }));
  expect(await screen.findByText('Applied + persisted to dg.yaml')).toBeInTheDocument();
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  // The scheduler is off: the page banner says so, not a second toast.
  expect(screen.queryByText(/do not run until/)).toBeNull();
  expect(screen.getByTestId('rules-scheduler-off')).toBeInTheDocument();
});
