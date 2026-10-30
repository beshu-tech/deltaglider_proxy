/**
 * A Refresh that cannot read the size of some objects keeps the counter and
 * answers `estimated: true` with the scan's own numbers. The chip dropped
 * the field, so the kept total read as a reconciled one.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import BucketUsageChip from '../components/BucketUsageChip';

afterEach(() => vi.unstubAllGlobals());

test('an estimated Refresh shows "≈" and says that the counter was kept', async () => {
  const http = mockFetch();
  const row = {
    bucket: 'releases', object_count: 7, logical_bytes: 700, stored_bytes: 70,
    savings_percentage: 90, last_scan_at: 1_760_000_000, never_scanned: false,
  };
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [{ name: 'releases' }] }));
  http.on('GET', '/_/api/admin/config', json({ bucket_policies: {} }));
  http.on('GET', '/_/api/admin/usage/bucket/releases', json(row));
  http.on('POST', /^\/_\/api\/admin\/usage\/refresh\?/, json({
    ...row, estimated: true, estimate: { object_count: 5, logical_bytes: 500, stored_bytes: 50 },
  }));
  renderWithQuery(<BucketUsageChip bucket="releases" canAdmin />);
  await userEvent.click(await screen.findByRole('button', { name: /Refresh the size/ }));
  expect(await screen.findByLabelText('estimated')).toHaveTextContent('≈');
  const chip = screen.getByTitle(/kept the running total/);
  expect(chip.getAttribute('title')).toMatch(/5 objects/);
});
