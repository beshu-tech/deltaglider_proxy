/**
 * Upload into a read-only prefix: the page keeps the folder the user was in,
 * says "You cannot upload here", and offers the writable prefixes. It never
 * moves the destination by itself (browser review: a user in the read-only
 * docs/ folder got an upload page aimed at firmware/widget-3000/, silently).
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';

const uploadObject = vi.hoisted(() => vi.fn(async (..._args: unknown[]) => ({})));
vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  listCommonPrefixes: async () => [],
  uploadObject,
}));

import UploadPage from '../components/UploadPage';

const canWrite = (p: string) => p.startsWith('firmware/widget-3000/');
const destInput = () => screen.getByRole('combobox', { name: /Destination path prefix/ });

test('a read-only destination says so, offers the writable prefixes and blocks the upload', async () => {
  const user = userEvent.setup();
  renderWithQuery(
    <UploadPage
      prefix="docs/"
      canWrite={canWrite}
      writablePrefixes={['firmware/widget-3000/']}
      initialFiles={[new File(['x'], 'README.txt')]}
      onBack={() => {}}
      onDone={() => {}}
    />,
  );
  expect(destInput()).toHaveValue('docs/');
  expect(screen.getByText(/You cannot upload to releases\/docs\//)).toBeInTheDocument();
  const upload = screen.getByRole('button', { name: /Upload 1 file to docs\// });
  expect(upload).toBeDisabled();

  await user.click(screen.getByRole('button', { name: 'firmware/widget-3000/' }));
  await waitFor(() => expect(destInput()).toHaveValue('firmware/widget-3000/'));
  expect(screen.queryByText(/You cannot upload to/)).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: /Upload 1 file to firmware\/widget-3000\// })).toBeEnabled();
});

test('a file over max_object_size or the remaining quota is refused before upload', async () => {
  const { json, mockFetch } = await import('../test/fetchMock');
  const http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ max_object_size: 100, bucket_policies: { releases: { quota_bytes: 1000 } } }));
  http.on('GET', '/_/api/admin/usage/bucket/releases', json({ bucket: 'releases', stored_bytes: 950, logical_bytes: 950, object_count: 1, never_scanned: false }));
  const user = userEvent.setup();
  renderWithQuery(
    <UploadPage
      prefix="builds/"
      checkLimits
      initialFiles={[new File(['x'.repeat(200)], 'big.bin'), new File(['x'.repeat(60)], 'mid.bin'), new File(['x'.repeat(10)], 'ok.bin')]}
      onBack={() => {}}
      onDone={() => {}}
    />,
  );
  // The refusal is on the file's own row, and the counts are of the files
  // that CAN go (D25, D30): never "3 files ready" above "Upload 1 file".
  expect(await screen.findByRole('listitem', { name: 'big.bin: refused' })).toHaveTextContent(/200 B is over the 100 B object size limit/);
  expect(await screen.findByRole('listitem', { name: 'mid.bin: refused' })).toHaveTextContent(/60 B does not fit in the bucket quota/);
  expect(screen.getByText(/^3 files staged · 1 can be uploaded \(10 B\) · 2 refused$/)).toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: /Upload 1 file to builds\// }));
  await waitFor(() => expect(uploadObject).toHaveBeenCalledTimes(1));
  expect(uploadObject.mock.calls[0][0]).toBe('builds/ok.bin');
  // The refused files stay in the list with their reason.
  expect(screen.getByRole('listitem', { name: 'big.bin: refused' })).toBeInTheDocument();
  vi.unstubAllGlobals();
});
