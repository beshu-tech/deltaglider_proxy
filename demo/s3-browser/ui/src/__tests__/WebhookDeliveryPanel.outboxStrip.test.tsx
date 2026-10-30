/**
 * The Event delivery page's status strip reads the event outbox counts. It
 * is best-effort: a failed read hides the strip and leaves the settings.
 */
import { screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import WebhookDeliveryPanel from '../components/WebhookDeliveryPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const OUTBOX = '/_/api/admin/event-outbox';
const counts = (pending: number, failed: number) => ({
  rows: [], counts: { pending, in_progress: 0, delivered: 0, failed }, total: 0, limit: 1, offset: 0,
  status: null, sort: 'occurred_at', order: 'desc', delivery_enabled: true, delivery_active: true,
  delivery_state: 'failing', last_delivery_error: 'hooks.example.com: 502',
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch()
    .on('GET', '/_/api/admin/config/section/advanced', json({ event_delivery: { enabled: true } }))
    .on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('the strip shows the counts and the last delivery error', async () => {
  http.on('GET', OUTBOX, json(counts(4, 2)));
  renderWithQuery(<WebhookDeliveryPanel />);
  expect(await screen.findByText('4 pending')).toBeInTheDocument();
  expect(screen.getByText('2 failed')).toBeInTheDocument();
  expect(screen.getByText('Last error: hooks.example.com: 502')).toBeInTheDocument();
  expect(http.callsTo('GET', OUTBOX)[0].path).toMatch(/limit=1&/);
});

test('a failed outbox read hides the strip and keeps the settings', async () => {
  http.on('GET', OUTBOX, json({ error: 'jobs db locked' }, 500));
  renderWithQuery(<WebhookDeliveryPanel />);
  expect(await screen.findByText('Enable delivery')).toBeInTheDocument();
  await waitFor(() => expect(http.callsTo('GET', OUTBOX)).toHaveLength(1));
  expect(screen.queryByText(/pending$/)).toBeNull();
  expect(screen.queryByText(/jobs db locked/)).toBeNull();
});
