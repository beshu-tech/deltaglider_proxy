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

const uploadObject = vi.hoisted(() => vi.fn());
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
