/**
 * A bucket's row menu in the browser sidebar offers "Edit settings" next to
 * Delete: it opens the Buckets page with that bucket's card open. Settings
 * need the admin GUI, so a non-admin sees no such item.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import Sidebar from '../components/Sidebar';
import { NavigationContext } from '../NavigationContext';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    listBuckets: async () => [
      { name: 'releases', creationDate: '' },
      { name: 'db-archive', creationDate: '', unavailable: 'backend down' },
    ],
    countBucketObjects: async () => ({ count: 0, truncated: false }),
    countMultipartUploads: async () => 0,
  };
});

function renderSidebar(canAdmin: boolean, canDelete: boolean) {
  const navigate = vi.fn();
  renderWithQuery(
    <NavigationContext.Provider value={{ navigate, subPath: '' }}>
      <Sidebar
        onUploadClick={() => {}}
        onBucketChange={() => {}}
        canCreateBucket
        canDeleteBucket={() => canDelete}
        canUpload
        canAdmin={canAdmin}
        includeBucketOrigins={false}
        open
        onClose={() => {}}
        isMobile={false}
      />
    </NavigationContext.Provider>,
  );
  return navigate;
}

test('Edit settings opens the Buckets page with the bucket named', async () => {
  const navigate = renderSidebar(true, true);
  const user = userEvent.setup();
  await user.click(await screen.findByRole('button', { name: 'Actions for bucket releases' }));
  expect(await screen.findByText(/Delete bucket/)).toBeInTheDocument();
  await user.click(screen.getByText('Edit settings'));
  expect(navigate).toHaveBeenCalledWith('/_/admin/storage/buckets?bucket=releases');
});

test('an unavailable bucket offers its settings, not Delete', async () => {
  renderSidebar(true, true);
  const user = userEvent.setup();
  await user.click(await screen.findByRole('button', { name: 'Actions for bucket db-archive' }));
  expect(await screen.findByText('Edit settings')).toBeInTheDocument();
  expect(screen.queryByText(/Delete bucket/)).toBeNull();
});

test('a non-admin sees no Edit settings', async () => {
  renderSidebar(false, true);
  const user = userEvent.setup();
  await user.click(await screen.findByRole('button', { name: 'Actions for bucket releases' }));
  expect(await screen.findByText(/Delete bucket/)).toBeInTheDocument();
  expect(screen.queryByText('Edit settings')).toBeNull();
  expect(screen.queryByRole('button', { name: 'Actions for bucket db-archive' })).toBeNull();
});
