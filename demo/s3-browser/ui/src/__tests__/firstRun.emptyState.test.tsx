/**
 * Browser-review item 17: a fresh proxy (no buckets) showed only "Create a
 * bucket". An admin also gets a link to the first-run setup wizard there.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    listObjects: async () => ({ objects: [], folders: [], isTruncated: false }),
    listBuckets: async () => [],
  };
});

import Root from '../Root';
import ThemeProvider from '../ThemeProvider';

beforeEach(() => {
  window.history.replaceState(null, '', '/_/browse/');
  const http = mockFetch();
  for (const m of ['GET', 'POST', 'PUT', 'DELETE']) http.on(m, /./, json({}, 404));
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://localhost:9000', region: 'us-east-1', bucket: '', access_key_id: 'AK', secret_access_key: 'secret',
  }));
  http.on('GET', '/_/api/admin/session', json({ valid: true, admin_gui: true }));
  http.on('GET', '/_/api/whoami', json({ mode: 'open', user: null }));
});
afterEach(() => vi.unstubAllGlobals());

test('the no-buckets empty state links an admin to the setup wizard', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ThemeProvider><Root /></ThemeProvider>);
  const link = await screen.findByRole('button', { name: 'Run the setup wizard' });
  await user.click(link);
  expect(window.location.pathname).toBe('/_/admin/setup');
});
