/**
 * Upload page (UX harvest C13): picking files stages them. Nothing uploads
 * until the user sets the folder, reviews the list and presses Upload. Five
 * picked files used to land in the bucket root before the folder was typed.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test, vi, beforeEach } from 'vitest';
import { renderWithQuery } from '../test/render';

const uploadObject = vi.hoisted(() => vi.fn(async (..._args: unknown[]) => ({})));
vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  listCommonPrefixes: async () => [],
  uploadObject,
}));

import UploadPage from '../components/UploadPage';

beforeEach(() => uploadObject.mockClear());

const destInput = () => screen.getByRole('combobox', { name: /Destination path prefix/ });
const fileInput = (container: HTMLElement) =>
  container.querySelector<HTMLInputElement>('input[type="file"]:not([webkitdirectory])')!;

test('picked files are staged: the folder can be set after the pick, and Upload sends them there', async () => {
  const user = userEvent.setup();
  const { container } = renderWithQuery(<UploadPage prefix="" onBack={() => {}} onDone={() => {}} />);
  await user.upload(fileInput(container), [new File(['a'], 'a.txt'), new File(['bb'], 'b.txt')]);

  const list = await screen.findByRole('list', { name: 'Staged files' });
  expect(within(list).getByText('a.txt')).toBeInTheDocument();
  expect(within(list).getByText('b.txt')).toBeInTheDocument();
  expect(uploadObject).not.toHaveBeenCalled();

  await user.type(destInput(), 'builds/');
  await user.click(screen.getByRole('button', { name: /Upload 2 files to builds\// }));
  await waitFor(() => expect(uploadObject).toHaveBeenCalledTimes(2));
  expect(uploadObject.mock.calls.map((c) => c[0])).toEqual(['builds/a.txt', 'builds/b.txt']);
});

test('a staged file can be removed, and picking it twice lists it once', async () => {
  const user = userEvent.setup();
  const { container } = renderWithQuery(<UploadPage prefix="builds/" onBack={() => {}} onDone={() => {}} />);
  const a = new File(['a'], 'a.txt', { lastModified: 1 });
  const b = new File(['b'], 'b.txt', { lastModified: 1 });
  await user.upload(fileInput(container), [a, b]);
  await user.upload(fileInput(container), [a]);
  expect(await screen.findByText(/2 files ready to upload/)).toBeInTheDocument();

  await user.click(screen.getByRole('button', { name: 'Remove a.txt from the list' }));
  await user.click(screen.getByRole('button', { name: /Upload 1 file to builds\// }));
  await waitFor(() => expect(uploadObject).toHaveBeenCalledTimes(1));
  expect(uploadObject.mock.calls[0][0]).toBe('builds/b.txt');
});
