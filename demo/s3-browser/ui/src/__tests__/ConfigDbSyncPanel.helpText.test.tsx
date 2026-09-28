/**
 * Docs-audit finding 4: the Sync bucket help said "the most recently saved
 * copy wins". The sync is a three-way merge by name (config_db/iam_merge.rs):
 * edits from two instances both survive, and only a field changed on both
 * sides goes to the newer change.
 */
import { screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { ConfigDbSyncPanel } from '../components/advancedPanels';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  mockFetch()
    .on('GET', '/_/api/admin/config/section/advanced', json({ config_sync_bucket: 'dgp-iam-state' }))
    .on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('the Sync bucket help describes the merge, not last-writer-wins', async () => {
  renderWithQuery(<ConfigDbSyncPanel />);
  const help = await screen.findByText(/Every instance must point at the same bucket/);
  expect(help.textContent).not.toMatch(/most recently saved copy wins/);
  expect(help.textContent).toMatch(/merge/);
});
