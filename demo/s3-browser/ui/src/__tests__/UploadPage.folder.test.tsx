/**
 * Upload page: the "New folder" name and the destination path refuse a `.`
 * or `..` folder before any request (the browser URL parser resolves `..`,
 * so the signed S3 request would target another key).
 */
import { screen, waitFor, within } from '@testing-library/react';
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

function page(initialFiles?: File[]) {
  return renderWithQuery(
    <UploadPage prefix="builds/" onBack={() => {}} onDone={() => {}} initialFiles={initialFiles} />,
  );
}

const destInput = () => screen.getByRole('combobox', { name: /Destination path prefix/ });

async function openNewFolder() {
  const user = userEvent.setup();
  await user.click(screen.getByRole('button', { name: /New folder/ }));
  const dialog = await screen.findByRole('dialog');
  const name = within(dialog).getByLabelText('Folder name');
  const create = within(dialog).getByRole('button', { name: 'Create' });
  return { user, dialog, name, create };
}

test.each(['..', '.', 'a/../b', 'x/.'])('a new folder named %j is refused, by click and by Enter', async (bad) => {
  page();
  const { user, dialog, name, create } = await openNewFolder();
  await user.type(name, bad);
  expect(within(dialog).getByRole('alert')).toHaveTextContent('Folder names cannot be "." or "..".');
  expect(create).toBeDisabled();
  await user.type(name, '{Enter}');
  // Still open, and the destination is unchanged.
  expect(within(dialog).getByLabelText('Folder name')).toBeInTheDocument();
  expect(destInput()).toHaveValue('builds/');
});

test('a valid folder name is appended to the destination', async () => {
  page();
  const { user, name, create } = await openNewFolder();
  await user.type(name, '/v2/');
  expect(create).toBeEnabled();
  await user.click(create);
  await waitFor(() => expect(destInput()).toHaveValue('builds/v2/'));
});

test('a destination with ".." shows the error and blocks the staged upload', async () => {
  const user = userEvent.setup();
  page([new File(['x'], 'app.zip')]);
  const upload = await screen.findByRole('button', { name: /Upload 1 file to builds\// });
  expect(upload).toBeEnabled();
  await user.clear(destInput());
  await user.type(destInput(), 'builds/../etc/');
  expect(screen.getByRole('alert')).toHaveTextContent('Folder names cannot be "." or "..".');
  const blocked = screen.getByRole('button', { name: /Upload 1 file to/ });
  expect(blocked).toBeDisabled();
  await user.click(blocked);
  expect(uploadObject).not.toHaveBeenCalled();
});
