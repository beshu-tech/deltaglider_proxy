/**
 * UX harvest D32: on the upload page the sidebar marked both the bucket
 * (accent text, bold, accent edge) and "Upload files". One entry is the
 * current one: "Upload files". The bucket row looks like any other row.
 */
import { screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import Sidebar from '../components/Sidebar';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    getBucket: () => 'releases',
    setBucket: vi.fn(),
    listBuckets: async () => [
      { name: 'db-archive', creationDate: '' },
      { name: 'releases', creationDate: '' },
    ],
  };
});

afterEach(() => window.history.replaceState(null, '', '/'));

function renderAt(path: string) {
  window.history.replaceState(null, '', path);
  renderWithQuery(
    <Sidebar
      onUploadClick={() => {}} onBucketChange={() => {}}
      canCreateBucket={false} canDeleteBucket={() => false}
      canUpload canAdmin={false} open onClose={() => {}} isMobile={false}
    />,
  );
}

/** The marks of a highlighted entry in an inline style. */
function highlighted(el: HTMLElement): boolean {
  return !el.style.borderLeft.includes('transparent') || el.style.fontWeight === '600' || el.getAttribute('aria-current') !== null;
}

test('on the upload page only "Upload files" is highlighted', async () => {
  renderAt('/_/upload');
  const bucketRow = (await screen.findByText('releases')).closest('button')!;
  const upload = screen.getByRole('button', { name: /upload files/i });
  expect(highlighted(upload)).toBe(true);
  expect(highlighted(bucketRow)).toBe(false);
  expect(bucketRow.querySelector('[style*="font-weight: 600"]')).toBeNull();
});

test('in the browser the active bucket is highlighted', async () => {
  renderAt('/_/browse/releases/');
  const bucketRow = (await screen.findByText('releases')).closest('button')!;
  expect(highlighted(bucketRow)).toBe(true);
});
