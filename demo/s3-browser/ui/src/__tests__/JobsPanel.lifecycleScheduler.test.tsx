/**
 * Docs-audit finding 1: no control set `storage.lifecycle.enabled` (default
 * false), so a GUI-only operator could never make a lifecycle rule run. The
 * Jobs screen now has a switch for it, applied through the lifecycle editor.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/storage';
const LABEL = 'Run lifecycle rules on schedule';
let http: ReturnType<typeof mockFetch>;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', SECTION, json({ lifecycle: { enabled: false, tick_interval: '1h', max_failures_retained: 100, rules: [] } }));
  http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: { storage: { 'lifecycle.enabled': { before: false, after: true } } } }));
  http.on('PUT', SECTION, json({ ok: true }));
});
afterEach(() => vi.unstubAllGlobals());

test('the lifecycle scheduler switch shows the saved value and applies storage.lifecycle.enabled', async () => {
  const user = userEvent.setup();
  renderWithQuery(<JobsPanel />);
  const sw = await screen.findByRole('switch', { name: LABEL });
  await waitFor(() => expect(http.callsTo('GET', SECTION).length).toBeGreaterThan(0));
  expect(sw).not.toBeChecked();

  await user.click(sw);
  expect(sw).toBeChecked();
  await user.click(screen.getByRole('button', { name: /Review & apply/ }));
  await user.click(await screen.findByRole('button', { name: 'Apply and persist changes' }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  const body = http.callsTo('PUT', SECTION)[0].body as { lifecycle: { enabled: boolean } };
  expect(body.lifecycle.enabled).toBe(true);
});

test('the switch reads on when the saved config has the scheduler on', async () => {
  http.on('GET', SECTION, json({ lifecycle: { enabled: true, tick_interval: '1h', max_failures_retained: 100, rules: [] } }));
  renderWithQuery(<JobsPanel />);
  const sw = await screen.findByRole('switch', { name: LABEL });
  await waitFor(() => expect(sw).toBeChecked());
});
