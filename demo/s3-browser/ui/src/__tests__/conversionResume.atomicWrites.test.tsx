/**
 * Review finding 9: Resume on a bucket whose backend has no proven atomic
 * writes. The server refuses it (a conversion keeps writing), so the UI
 * disables it and says why, visibly. A refusal the server still sends
 * shows as an alert with a clickable doc link, not as a toast.
 */
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BucketEncryptionGroup from '../components/BucketEncryptionGroup';
import type { BackendCapabilityVerdict, ConversionView } from '../adminApi';
import { NavigationContext } from '../NavigationContext';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff } from '../test/encryption';

const DOC = 'https://deltaglider.com/docs/how-to/backend-capability-validation';
const REENCRYPT = '/_/api/admin/jobs/reencrypt';

function view(): ConversionView {
  return { open: true, cutoff: 1_793_000_000, deadline: 1_793_604_800, run: 'a'.repeat(32), adopted_after_cutoff: 0, job_on_this_node: null };
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config/section/storage', json({ buckets: {} }));
});
afterEach(() => vi.unstubAllGlobals());

function render(capability: BackendCapabilityVerdict) {
  renderWithQuery(
    <NavigationContext.Provider value={{ navigate: vi.fn(), subPath: '' } as never}>
      <BucketEncryptionGroup bucket="db-archive" eff={bucketKeyEff({ backend: 'b2', conversion: view(), capability })} onApplied={vi.fn()} />
    </NavigationContext.Provider>,
  );
}

test('a read-only bucket: Resume is disabled, with a visible reason', () => {
  render({ verdict: 'non-cas' });
  const row = screen.getByTestId('conversion-status');
  const resume = within(row).getByRole('button', { name: /Resume the conversion of db-archive/ });
  expect(resume).toBeDisabled();
  expect(row).toHaveTextContent('Resume needs atomic writes, which b2 does not support.');
});

test('a backend the proxy could not verify: Resume is disabled and says so', () => {
  render({ verdict: 'unknown', reason: 'timeout' });
  const row = screen.getByTestId('conversion-status');
  expect(within(row).getByRole('button', { name: /Resume the conversion of db-archive/ })).toBeDisabled();
  expect(row).toHaveTextContent('could not verify');
});

test('a refusal of Resume renders as an alert with a clickable doc link', async () => {
  http.on(
    'POST',
    REENCRYPT,
    json(
      {
        started: [],
        errors: [{ bucket: 'db-archive', error: `the re-encrypt job of bucket 'db-archive' refused — see ${DOC}` }],
      },
      409,
    ),
  );
  render({ verdict: 'cas-verified', via: 'probe' });
  await userEvent.setup().click(screen.getByRole('button', { name: 'Resume the conversion of db-archive' }));
  const alert = await screen.findByRole('alert');
  expect(within(alert).getByRole('link', { name: 'How to check a backend' })).toHaveAttribute(
    'href',
    '/_/docs/how-to-backend-capability-validation',
  );
});
