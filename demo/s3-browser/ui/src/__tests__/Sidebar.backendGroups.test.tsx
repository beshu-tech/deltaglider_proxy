/**
 * Sidebar groups: with buckets on more than one backend, the list is in
 * collapsible groups by backend (the group header replaces the per-row
 * backend chip, so the bucket name gets the full row). A collapsed group
 * stays collapsed after a reload. The sidebar width is the operator's: the
 * right edge is a keyboard-operable separator, and the width is stored.
 */
import { fireEvent, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import Sidebar from '../components/Sidebar';
import { readStorage, removeStorage, writeStorage } from '../safeStorage';
import {
  COLLAPSED_GROUPS_STORAGE_KEY,
  SIDEBAR_DEFAULT_WIDTH,
  SIDEBAR_WIDTH_STORAGE_KEY,
} from '../sidebarLayout';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    getBucket: () => 'downloads',
    listBuckets: async () => [
      { name: 'db-archive', creationDate: '', backend: { backendName: 'local-disk' } },
      { name: 'downloads', creationDate: '', backend: { backendName: 'hetzner-fsn1' } },
      { name: 'releases', creationDate: '', backend: { backendName: 'hetzner-fsn1', realBucket: 'rel-prod' } },
    ],
  };
});

afterEach(() => {
  removeStorage(COLLAPSED_GROUPS_STORAGE_KEY);
  removeStorage(SIDEBAR_WIDTH_STORAGE_KEY);
});

function renderSidebar(canAdmin = true) {
  return renderWithQuery(
    <Sidebar
      onUploadClick={() => {}} onBucketChange={() => {}} canCreateBucket={false} canDeleteBucket={() => false}
      canUpload={false} canAdmin={canAdmin} open onClose={() => {}} isMobile={false}
    />,
  );
}

const groupHeader = (backend: string) => screen.getByRole('button', { name: new RegExp(`^${backend} \\(\\d+ buckets\\)$`) });

test('buckets are in groups by backend, in name order, without per-row chips', async () => {
  renderSidebar();
  await screen.findByText('releases');
  const headers = screen.getAllByRole('button', { expanded: true });
  expect(headers.map((h) => h.textContent)).toEqual(['hetzner-fsn12', 'local-disk1']);
  const hetznerList = document.getElementById(groupHeader('hetzner-fsn1').getAttribute('aria-controls')!)!;
  expect(within(hetznerList).getAllByRole('listitem').map((li) => li.textContent)).toEqual(['downloads', 'releases']);
  expect(screen.queryByLabelText(/^Backend /)).toBeNull();
  // The row keeps the per-bucket facts on hover; the header only the backend's.
  expect(screen.getByRole('button', { name: 'releases' })).toHaveAttribute('title', 'releases\nBackend: hetzner-fsn1\nReal bucket: rel-prod');
  expect(groupHeader('hetzner-fsn1')).toHaveAttribute('title', 'Backend: hetzner-fsn1');
});

test('a collapsed group hides its rows, marks the open bucket, and stays collapsed', async () => {
  const user = userEvent.setup();
  const { unmount } = renderSidebar();
  await screen.findByText('releases');
  await user.click(groupHeader('hetzner-fsn1'));
  expect(groupHeader('hetzner-fsn1')).toHaveAttribute('aria-expanded', 'false');
  expect(screen.queryByRole('button', { name: 'downloads' })).toBeNull();
  expect(groupHeader('hetzner-fsn1').title).toContain('Contains the open bucket');
  expect(readStorage(COLLAPSED_GROUPS_STORAGE_KEY)).toBe('["hetzner-fsn1"]');

  unmount();
  renderSidebar();
  await screen.findByText('db-archive');
  expect(groupHeader('hetzner-fsn1')).toHaveAttribute('aria-expanded', 'false');
  await user.click(groupHeader('hetzner-fsn1'));
  expect(screen.getByRole('button', { name: 'downloads' })).toBeVisible();
  expect(readStorage(COLLAPSED_GROUPS_STORAGE_KEY)).toBe('[]');
});

test('without backend data (non-admin) the list is flat', async () => {
  renderSidebar(false);
  await screen.findByText('releases');
  expect(screen.queryByRole('button', { expanded: true })).toBeNull();
  expect(screen.queryByRole('button', { expanded: false })).toBeNull();
});

test('the sidebar edge resizes with the keyboard and stores the width', async () => {
  const user = userEvent.setup();
  renderSidebar();
  const handle = screen.getByRole('separator', { name: 'Resize sidebar' });
  expect(handle).toHaveAttribute('aria-valuenow', String(SIDEBAR_DEFAULT_WIDTH));
  handle.focus();
  await user.keyboard('{ArrowRight}{ArrowRight}');
  expect(handle).toHaveAttribute('aria-valuenow', String(SIDEBAR_DEFAULT_WIDTH + 32));
  expect(readStorage(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(String(SIDEBAR_DEFAULT_WIDTH + 32));
  fireEvent.doubleClick(handle);
  expect(handle).toHaveAttribute('aria-valuenow', String(SIDEBAR_DEFAULT_WIDTH));
  expect(readStorage(SIDEBAR_WIDTH_STORAGE_KEY)).toBeNull();
});

test('a stored width is restored', () => {
  writeStorage(SIDEBAR_WIDTH_STORAGE_KEY, '400');
  renderSidebar();
  expect(screen.getByRole('separator', { name: 'Resize sidebar' })).toHaveAttribute('aria-valuenow', '400');
});
