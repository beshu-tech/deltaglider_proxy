/**
 * The "Not reconciled" title names every reason why the running total is
 * not trusted, including a scan that ran while writes changed the bucket:
 * only a quiet scan marks the counter as trusted.
 */
import { screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import BucketUsageChip from '../components/BucketUsageChip';

afterEach(() => vi.unstubAllGlobals());

test('the not-reconciled title says that a scan during writes does not reconcile', async () => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [{ name: 'releases' }] }));
  http.on('GET', '/_/api/admin/config', json({ bucket_policies: {} }));
  http.on('GET', '/_/api/admin/usage/bucket/releases', json({
    bucket: 'releases', object_count: 7, logical_bytes: 700, stored_bytes: 70,
    savings_percentage: 90, last_scan_at: null, never_scanned: false,
  }));
  renderWithQuery(<BucketUsageChip bucket="releases" canAdmin />);
  const chip = await screen.findByTitle(/Not reconciled/);
  expect(chip.getAttribute('title')).toMatch(/scan .*while .*writ/i);
});

test('a bucket with a quota shows the share of it that is stored (B54)', async () => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [{ name: 'terraform-state' }] }));
  http.on('GET', '/_/api/admin/config', json({ bucket_policies: { 'terraform-state': { quota_bytes: 1024 * 1024 * 1024 } } }));
  http.on('GET', '/_/api/admin/usage/bucket/terraform-state', json({
    bucket: 'terraform-state', object_count: 1, logical_bytes: 32, stored_bytes: 950 * 1024 * 1024,
    savings_percentage: 0, last_scan_at: 1_760_000_000, never_scanned: false,
  }));
  renderWithQuery(<BucketUsageChip bucket="terraform-state" canAdmin />);
  expect(await screen.findByTestId('bucket-quota')).toHaveTextContent('93% of 1.0 GiB quota');
});
