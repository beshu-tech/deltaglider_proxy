/**
 * Sidebar with many buckets on many backends: a filter input (from
 * BUCKET_FILTER_MIN buckets) narrows the list and opens every group that
 * still has a match; "Collapse all" / "Expand all" act on every group; the
 * header shows the backend type and marks a collapsed group that holds the
 * open bucket.
 */
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import Sidebar from '../components/Sidebar';
import { readStorage, removeStorage, writeStorage } from '../safeStorage';
import { BUCKET_FILTER_MIN, COLLAPSED_GROUPS_STORAGE_KEY } from '../sidebarLayout';
import { renderWithQuery } from '../test/render';

const BUCKETS = [
  ...['releases', 'releases-staging', 'downloads', 'firmware-images', 'ci-artifacts', 'telemetry']
    .map((name) => ({ name, creationDate: '', backend: { backendName: 'hetzner-fsn1', backendType: 's3' } })),
  ...['db-archive', 'pg-wal', 'media-raw']
    .map((name) => ({ name, creationDate: '', backend: { backendName: 'local-disk', backendType: 'filesystem' } })),
  { name: 'releases-dr', creationDate: '', backend: { backendName: 'aws-dr', backendType: 's3' } },
];

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return { ...real, getBucket: () => 'releases', listBuckets: async () => BUCKETS };
});

afterEach(() => removeStorage(COLLAPSED_GROUPS_STORAGE_KEY));

function renderSidebar() {
  return renderWithQuery(
    <Sidebar
      onUploadClick={() => {}} onBucketChange={() => {}} canCreateBucket={false} canDeleteBucket={() => false}
      canUpload={false} canAdmin open onClose={() => {}} isMobile={false}
    />,
  );
}

const header = (backend: string) => screen.getByRole('button', { name: new RegExp(`^${backend} \\(\\d+ buckets\\)$`) });
const rowsOf = (backend: string) =>
  within(document.getElementById(header(backend).getAttribute('aria-controls')!)!).queryAllByRole('listitem').map((li) => li.textContent);

test('the filter keeps the groups of the matching buckets and opens a collapsed one', async () => {
  expect(BUCKETS.length).toBeGreaterThanOrEqual(BUCKET_FILTER_MIN);
  writeStorage(COLLAPSED_GROUPS_STORAGE_KEY, '["aws-dr"]');
  const user = userEvent.setup();
  renderSidebar();
  await screen.findByText('pg-wal');
  expect(header('aws-dr')).toHaveAttribute('aria-expanded', 'false');

  await user.type(screen.getByRole('textbox', { name: 'Filter buckets' }), 'REL');
  expect(rowsOf('hetzner-fsn1')).toEqual(['releases', 'releases-staging']);
  expect(rowsOf('aws-dr')).toEqual(['releases-dr']);
  expect(header('aws-dr')).toHaveAttribute('aria-expanded', 'true');
  expect(screen.queryByRole('button', { name: /^local-disk/ })).toBeNull();
  // The stored collapsed state is not touched by a filter.
  expect(readStorage(COLLAPSED_GROUPS_STORAGE_KEY)).toBe('["aws-dr"]');

  await user.type(screen.getByRole('textbox', { name: 'Filter buckets' }), ' zzz');
  expect(screen.getByRole('status')).toHaveTextContent('No bucket matches “REL zzz”.');
  expect(screen.queryByRole('button', { expanded: true })).toBeNull();

  await user.clear(screen.getByRole('textbox', { name: 'Filter buckets' }));
  expect(header('aws-dr')).toHaveAttribute('aria-expanded', 'false');
  expect(rowsOf('local-disk')).toEqual(['db-archive', 'pg-wal', 'media-raw']);
});

test('collapse all / expand all act on every group and persist', async () => {
  const user = userEvent.setup();
  renderSidebar();
  await screen.findByText('pg-wal');
  await user.click(screen.getByRole('button', { name: 'Collapse all' }));
  expect(screen.getAllByRole('button', { expanded: false })).toHaveLength(3);
  expect(readStorage(COLLAPSED_GROUPS_STORAGE_KEY)).toBe('["aws-dr","hetzner-fsn1","local-disk"]');
  // The open bucket is in a collapsed group: its header says so.
  expect(header('hetzner-fsn1').title).toContain('Contains the open bucket releases');
  expect(header('hetzner-fsn1').querySelector('.dg-bucket-group-active-dot')).not.toBeNull();
  expect(header('aws-dr').querySelector('.dg-bucket-group-active-dot')).toBeNull();

  await user.click(screen.getByRole('button', { name: 'Expand all' }));
  expect(screen.getAllByRole('button', { expanded: true })).toHaveLength(3);
  expect(readStorage(COLLAPSED_GROUPS_STORAGE_KEY)).toBe('[]');
});

test('the header shows the backend type as an icon, not as text', async () => {
  renderSidebar();
  await screen.findByText('pg-wal');
  expect(header('local-disk').querySelector('.anticon-hdd')).not.toBeNull();
  expect(header('aws-dr').querySelector('.anticon-cloud')).not.toBeNull();
  expect(header('local-disk').textContent).toBe('local-disk3');
});
