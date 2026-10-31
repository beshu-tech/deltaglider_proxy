/**
 * UX harvest D16, D17, D36 on the Buckets page: while the data loaded, the
 * page showed "Loading buckets…" AND the bucket rows with placeholder chips
 * ("Default backend · Private"); a failed load was a red box with the wire
 * text ("Section fetch (storage) failed (500) [internal]: boom"); create was
 * a split button.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BucketsPanel from '../components/BucketsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { eff } from '../test/encryption';

const SECTION = '/_/api/admin/config/section/storage';
let http: ReturnType<typeof mockFetch>;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75, backends: [], bucket_policies: {} }));
  http.on('GET', '/_/api/admin/backends', json({ backends: [], default_backend: null }));
  http.on('GET', '/_/api/admin/buckets', json({
    buckets: [{ name: 'releases', backend_name: 'default', effective_encryption: eff({ backend: 'default' }) }],
  }));
  http.on('GET', '/_/api/admin/config/section/advanced', json({}));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', '/_/api/admin/encryption-witness', json(null));
});
afterEach(() => vi.unstubAllGlobals());

test('while the settings load: a skeleton, no bucket rows and no placeholder chips', async () => {
  let release: () => void = () => {};
  const gate = new Promise<void>((r) => (release = r));
  http.on('GET', SECTION, async () => {
    await gate;
    return json({ buckets: { releases: { public: true } } });
  });
  renderWithQuery(<BucketsPanel />);
  expect(await screen.findByRole('status', { name: 'Loading the buckets' })).toBeInTheDocument();
  // The origins are in, the settings not yet: no row may claim "Private".
  await waitFor(() => expect(http.callsTo('GET', '/_/api/admin/buckets').length).toBeGreaterThan(0));
  expect(screen.queryByText('Private')).toBeNull();
  expect(screen.queryByRole('button', { name: /^releases / })).toBeNull();
  release();
  expect(await screen.findByRole('button', { name: /^releases / })).toHaveTextContent('Public bucket');
});

test('a failed load is one plain error with Retry, and no rows', async () => {
  http.on('GET', SECTION, json({ error: 'boom', code: 'internal' }, 500));
  renderWithQuery(<BucketsPanel />);
  expect(await screen.findByText('Could not load the buckets')).toBeInTheDocument();
  expect(screen.queryByText(/boom|\(500\)|\[internal\]/)).toBeNull();
  expect(screen.queryByRole('button', { name: /^releases / })).toBeNull();
  http.on('GET', SECTION, json({ buckets: {} }));
  await userEvent.setup().click(screen.getByRole('button', { name: /Retry/ }));
  expect(await screen.findByRole('button', { name: /^releases / })).toBeInTheDocument();
});

test('one primary "New bucket"; the future-bucket settings are a plain secondary button', async () => {
  http.on('GET', SECTION, json({ buckets: {} }));
  renderWithQuery(<BucketsPanel />);
  const create = await screen.findByRole('button', { name: /New bucket/ });
  expect(create).toHaveClass('ant-btn-primary');
  const future = screen.getByRole('button', { name: /Settings for a future bucket/ });
  expect(future).not.toHaveClass('ant-btn-primary');
  expect(screen.queryByRole('button', { name: 'More ways to add a bucket' })).toBeNull();
});
