/**
 * "Clone" in a rule's "⋯" menu copies the rule into a new draft and opens
 * it. The copy acts on the same objects until the operator changes a bucket,
 * and the drawer says so.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import { ReplicationRulesPanel } from '../components/jobs/RulesPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const row: JobRow = {
  id: 'replication:nightly', kind: 'replication', name: 'nightly',
  scope: { bucket: 'releases', target: 'downloads' }, trigger: 'continuous', enabled: true, paused: false,
  status: 'idle', status_raw: 'idle', progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 }, detail: {},
};
beforeEach(() => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [row] }));
  http.on('GET', /\/_\/api\/admin\/jobs\/.*\/runs/, json({ runs: [] }));
  http.on('GET', /\/_\/api\/admin\/jobs\/.*\/failures/, json({ failures: [] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({
    replication: { enabled: true, rules: [{ name: 'nightly', enabled: true,
      source: { bucket: 'releases', prefix: '' }, destination: { bucket: 'downloads', prefix: '' } }] },
  }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('Clone stages a copy, opens it, and warns that it acts on the same objects', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ReplicationRulesPanel />);
  await screen.findAllByText('nightly');
  const saved = screen.getAllByRole('row').find((r) => /^nightly/.test(r.textContent ?? ''))!;
  await user.click(within(saved).getByRole('button', { name: /^More actions for / }));
  await user.click(await screen.findByRole('menuitem', { name: /Clone/ }));

  const drawer = await screen.findByRole('dialog');
  expect(within(drawer).getByText('draft')).toBeInTheDocument();
  expect((within(drawer).getAllByRole('textbox')[0] as HTMLInputElement).value).toBe('nightly-copy');
  expect(within(drawer).getByText(/Rule "nightly" has the same source, destination and filters/)).toBeInTheDocument();

  await waitFor(() => {
    const rows = screen.getAllByRole('row').slice(1).map((r) => r.textContent ?? '');
    expect(rows).toHaveLength(2);
    expect(rows[1]).toMatch(/^nightly-copydraft/);
  });
  expect(screen.getByRole('button', { name: 'Review & apply' })).toBeInTheDocument();
});
