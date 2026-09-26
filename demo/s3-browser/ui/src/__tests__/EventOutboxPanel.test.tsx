/**
 * EventOutboxPanel: the event log. Pins the per-endpoint delivery view
 * (6b2eff15): each row carries `deliveries`, the status cell summarises
 * "ok/n endpoints", and an expandable row lists every endpoint (webhook URL
 * or Slack channel) with its own status, attempts, and error. Also pins the
 * requests the panel sends (list query, status filter, requeue).
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import type { EndpointDelivery, EventOutboxRecord } from '../adminApi';
import EventOutboxPanel from '../components/EventOutboxPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const LIST = '/_/api/admin/event-outbox';
const NOW_S = Math.floor(Date.now() / 1000);

function record(id: number, over: Partial<EventOutboxRecord> = {}): EventOutboxRecord {
  return {
    id,
    kind: 'object_created',
    bucket: 'releases',
    key: `v1.${id}.zip`,
    source: 's3_put',
    occurred_at: NOW_S - 60,
    payload: { size: 10 },
    status: 'delivered',
    attempts: 1,
    next_attempt_at: null,
    claimed_by: null,
    claimed_at: null,
    delivered_at: NOW_S - 30,
    last_error: null,
    created_at: NOW_S - 60,
    deliveries: [],
    ...over,
  };
}

const hookOk: EndpointDelivery = {
  endpoint_id: 'ep-hook',
  label: 'https://hooks.example.com/… (#1)',
  status: 'delivered',
  attempts: 1,
  last_error: null,
  updated_at: NOW_S - 30,
};
const slackFail: EndpointDelivery = {
  endpoint_id: 'ep-slack',
  label: '#releases',
  status: 'failed',
  attempts: 3,
  last_error: 'slack: channel_not_found',
  updated_at: NOW_S - 20,
};
const removedEndpoint: EndpointDelivery = {
  endpoint_id: 'ep-gone',
  label: null,
  status: 'failed',
  attempts: 5,
  last_error: 'connection refused',
  updated_at: NOW_S - 10,
};

function page(rows: EventOutboxRecord[], counts = { pending: 0, in_progress: 0, delivered: 1, failed: 1 }) {
  return json({ rows, counts, total: rows.length, delivery_enabled: true, delivery_active: true });
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

/** The table body row that holds `text`. */
async function rowWith(text: string): Promise<HTMLElement> {
  const cell = await screen.findByText(text);
  const tr = cell.closest('tr');
  if (!tr) throw new Error(`no table row for ${text}`);
  return tr;
}

test('first load asks for page 1, newest first, all statuses', async () => {
  http.on('GET', LIST, page([record(1)]));
  renderWithQuery(<EventOutboxPanel />);
  await rowWith('v1.1.zip');
  const [first] = http.callsTo('GET', LIST);
  const qs = new URLSearchParams(first.path.split('?')[1]);
  expect(Object.fromEntries(qs)).toEqual({ limit: '50', offset: '0', sort: 'occurred_at', order: 'desc' });
  expect(screen.getByText('delivery active')).toBeInTheDocument();
});

test('a failing delivery says Failing, not active', async () => {
  http.on(
    'GET',
    LIST,
    json({
      rows: [record(1)],
      counts: { pending: 0, in_progress: 0, delivered: 0, failed: 1 },
      total: 1,
      delivery_enabled: true,
      delivery_active: true,
      delivery_state: 'failing',
      last_delivery_error: 'webhook endpoint rejected',
    }),
  );
  renderWithQuery(<EventOutboxPanel />);
  expect(await screen.findByText('delivery failing')).toBeInTheDocument();
});

describe('per-endpoint delivery', () => {
  const partial = record(7, {
    status: 'failed',
    attempts: 3,
    delivered_at: null,
    last_error: 'slack: channel_not_found',
    deliveries: [hookOk, slackFail, removedEndpoint],
  });
  const noDeliveries = record(8, { key: 'fresh.zip', status: 'pending', delivered_at: null, deliveries: [] });

  test('the status cell summarises delivered/total endpoints', async () => {
    http.on('GET', LIST, page([partial, noDeliveries]));
    renderWithQuery(<EventOutboxPanel />);
    const row = await rowWith('v1.7.zip');
    expect(within(row).getByText('1/3 endpoints')).toBeInTheDocument();
    // A row nothing has been attempted for has no summary.
    const fresh = await rowWith('fresh.zip');
    expect(within(fresh).queryByText(/endpoints?$/)).not.toBeInTheDocument();
  });

  test('only rows with deliveries are expandable', async () => {
    http.on('GET', LIST, page([partial, noDeliveries]));
    renderWithQuery(<EventOutboxPanel />);
    const row = await rowWith('v1.7.zip');
    expect(within(row).getByRole('button', { name: /expand row/i })).toBeInTheDocument();
    const fresh = await rowWith('fresh.zip');
    expect(within(fresh).queryByRole('button', { name: /expand row/i })).not.toBeInTheDocument();
  });

  test('expanding lists each endpoint with its own status, attempts and error', async () => {
    http.on('GET', LIST, page([partial]));
    renderWithQuery(<EventOutboxPanel />);
    const row = await rowWith('v1.7.zip');
    await userEvent.setup().click(within(row).getByRole('button', { name: /expand row/i }));

    const hook = await screen.findByText('https://hooks.example.com/… (#1)');
    const hookLine = hook.parentElement as HTMLElement;
    expect(within(hookLine).getByText('delivered')).toBeInTheDocument();
    expect(within(hookLine).getByText(/^1 attempt ·/)).toBeInTheDocument();

    const slackLine = screen.getByText('#releases').parentElement as HTMLElement;
    expect(within(slackLine).getByText('failed')).toBeInTheDocument();
    expect(within(slackLine).getByText(/^3 attempts ·/)).toBeInTheDocument();
    expect(within(slackLine).getByText('slack: channel_not_found')).toBeInTheDocument();

    // An endpoint removed from the config keeps its row, labelled as such.
    const goneLine = screen.getByText('endpoint no longer configured').parentElement as HTMLElement;
    expect(goneLine).toHaveTextContent('connection refused');
    expect(within(goneLine).getByText('endpoint no longer configured')).toHaveAttribute('title', 'endpoint id ep-gone');
  });

  test('all endpoints delivered reads n/n', async () => {
    http.on('GET', LIST, page([record(9, { deliveries: [hookOk, { ...slackFail, status: 'delivered', last_error: null }] })]));
    renderWithQuery(<EventOutboxPanel />);
    const row = await rowWith('v1.9.zip');
    expect(within(row).getByText('2/2 endpoints')).toBeInTheDocument();
  });
});

test('clicking the Failed pill refetches with status=failed from page 1', async () => {
  http.on('GET', LIST, page([record(1)], { pending: 0, in_progress: 0, delivered: 1, failed: 4 }));
  renderWithQuery(<EventOutboxPanel />);
  await rowWith('v1.1.zip');
  await userEvent.setup().click(screen.getByRole('button', { name: /Failed\s*4/ }));
  await waitFor(() => {
    const gets = http.callsTo('GET', LIST);
    expect(gets[gets.length - 1].path).toContain('status=failed');
  });
  const gets = http.callsTo('GET', LIST);
  expect(gets[gets.length - 1].path).toContain('offset=0');
  expect(screen.getByRole('button', { name: 'Clear status filter' })).toBeInTheDocument();
});

test('Requeue on a failed row POSTs /:id/requeue and reloads the list', async () => {
  http.on('GET', LIST, page([record(7, { status: 'failed', delivered_at: null, deliveries: [slackFail] })]));
  http.on('POST', '/_/api/admin/event-outbox/7/requeue', json({ requeued: 1 }));
  renderWithQuery(<EventOutboxPanel />);
  const row = await rowWith('v1.7.zip');
  const before = http.callsTo('GET', LIST).length;
  await userEvent.setup().click(within(row).getByRole('button', { name: /Requeue/ }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/event-outbox/7/requeue')).toHaveLength(1));
  await waitFor(() => expect(http.callsTo('GET', LIST).length).toBeGreaterThan(before));
});

test('Requeue is disabled on a row that is not failed', async () => {
  http.on('GET', LIST, page([record(1)]));
  renderWithQuery(<EventOutboxPanel />);
  const row = await rowWith('v1.1.zip');
  expect(within(row).getByRole('button', { name: /Requeue/ })).toBeDisabled();
});

test('a 401 calls onSessionExpired and shows no error banner', async () => {
  http.on('GET', LIST, json({ error: 'unauthorized' }, 401));
  const onSessionExpired = vi.fn();
  renderWithQuery(<EventOutboxPanel onSessionExpired={onSessionExpired} />);
  await waitFor(() => expect(onSessionExpired).toHaveBeenCalled());
  expect(screen.queryByText('Fetch failed')).not.toBeInTheDocument();
});

test('a 500 shows the fetch error', async () => {
  http.on('GET', LIST, json({ error: 'db locked' }, 500));
  renderWithQuery(<EventOutboxPanel />);
  expect(await screen.findByText('Fetch failed')).toBeInTheDocument();
});
