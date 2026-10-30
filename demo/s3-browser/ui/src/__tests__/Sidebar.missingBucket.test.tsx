/**
 * A linked bucket that does not exist (UX harvest C47): the sidebar used to
 * open another bucket in silence. With a caller that shows the missing
 * bucket, it reports the bucket names and stays at the linked bucket.
 */
import { screen, waitFor } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import Sidebar from '../components/Sidebar';
import { renderWithQuery } from '../test/render';

const setBucket = vi.hoisted(() => vi.fn());
vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    getBucket: () => 'no-such-bucket',
    setBucket,
    listBuckets: async () => [
      { name: 'db-archive', creationDate: '' },
      { name: 'releases', creationDate: '' },
    ],
  };
});

test('a missing linked bucket is reported, not swapped for another bucket', async () => {
  const onBucketChange = vi.fn();
  const onBucketNamesChanged = vi.fn();
  renderWithQuery(
    <Sidebar
      onUploadClick={() => {}} onBucketChange={onBucketChange} onBucketNamesChanged={onBucketNamesChanged}
      canCreateBucket={false} canDeleteBucket={() => false}
      canUpload={false} canAdmin={false} open onClose={() => {}} isMobile={false}
    />,
  );
  await screen.findByText('releases');
  await waitFor(() => expect(onBucketNamesChanged).toHaveBeenCalledWith(['db-archive', 'releases']));
  expect(onBucketChange).not.toHaveBeenCalled();
  expect(setBucket).not.toHaveBeenCalled();
});
