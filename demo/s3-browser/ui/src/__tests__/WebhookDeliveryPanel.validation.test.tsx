/**
 * UX harvest D9/D10 on the Event delivery page: the number fields clamped
 * a typo ("-1" → the minimum) without a word, and a bad endpoint URL was
 * named only in a banner above the page.
 */
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import WebhookDeliveryPanel from '../components/WebhookDeliveryPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  mockFetch()
    .on('GET', '/_/api/admin/config/section/advanced', json({ event_delivery: { enabled: true, webhook_urls: ['https://hooks.example.com/a'] } }))
    .on('GET', '/_/api/admin/config', json({ env_overrides: [] }))
    .on('GET', '/_/api/admin/event-outbox', json({ error: 'off' }, 500));
});
afterEach(() => vi.unstubAllGlobals());

test('a max attempts of -1 stays as typed, with an error the bar counts', async () => {
  const user = userEvent.setup();
  renderWithQuery(<WebhookDeliveryPanel />);
  await user.click(await screen.findByText('Delivery tuning'));
  const field = screen.getByRole('textbox', { name: 'Max attempts' });
  await user.clear(field);
  await user.type(field, '-1');
  await user.tab();
  expect(field).toHaveValue('-1');
  expect(field).toHaveAttribute('aria-invalid', 'true');
  expect(screen.getByText('Enter a whole number of 1 or more.')).toBeInTheDocument();
  expect(screen.getByText('1 issue to fix')).toBeInTheDocument();
});

test('a bad endpoint URL is named under its field', async () => {
  const user = userEvent.setup();
  renderWithQuery(<WebhookDeliveryPanel />);
  const url = await screen.findByRole('textbox', { name: 'Endpoint URL' });
  await user.clear(url);
  await user.type(url, 'not a url');
  await user.tab();
  const row = url.closest('div')?.parentElement as HTMLElement;
  expect(within(row).getByRole('alert')).toHaveTextContent('Enter a full URL that starts with https://.');
});
