/**
 * The bulk action bar and its destination picker: delete asks first, copy /
 * move send the chosen destination, and a destination path with a `.` or
 * `..` folder (or the selection's own folder) cannot be confirmed.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, test, vi } from 'vitest';
import { ApiError } from '../errorHandling';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  listBuckets: async () => [{ name: 'releases' }, { name: 'archive' }, { name: 'broken', unavailable: true }],
}));

import BulkActionBar from '../components/BulkActionBar';

type Op = (b: string, p: string) => Promise<{ succeeded: number; failed: number }>;

function bar(props: Partial<Parameters<typeof BulkActionBar>[0]> = {}) {
  return renderWithQuery(
    <BulkActionBar
      selectedCount={2}
      selectedFolderCount={1}
      deleting={false}
      currentPrefix="builds/"
      selectionKeys={['builds/app.zip', 'folder:builds/nightly/']}
      {...props}
    />,
  );
}

describe('delete', () => {
  test('asks for confirmation naming the folder, and deletes only on OK', async () => {
    const user = userEvent.setup();
    const onDelete = vi.fn();
    bar({ onDelete });
    await user.click(screen.getByRole('button', { name: 'Delete 2 selected items' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getAllByText('Delete permanently?').length).toBeGreaterThan(0);
    expect(
      within(dialog).getByText(
        'Delete 2 selected items? 1 of them is a folder: everything inside is deleted too. This cannot be undone.',
      ),
    ).toBeInTheDocument();
    expect(onDelete).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    expect(onDelete).not.toHaveBeenCalled();

    await user.click(screen.getByRole('button', { name: 'Delete 2 selected items' }));
    // The first dialog may still be animating out: act on the newest one.
    const dialogs = await screen.findAllByRole('dialog');
    const again = dialogs[dialogs.length - 1];
    await user.click(within(again).getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(onDelete).toHaveBeenCalledTimes(1));
  });

  test('actions without a handler are not offered; the hint explains why', () => {
    bar({ hint: 'Sign in as an administrator for bulk actions.' });
    expect(screen.queryByRole('button', { name: /Delete|Copy|Move|ZIP/ })).toBeNull();
    expect(screen.getByText('Sign in as an administrator for bulk actions.')).toBeInTheDocument();
  });

  test('every action is disabled while a delete runs', () => {
    const op: Op = async () => ({ succeeded: 0, failed: 0 });
    bar({ deleting: true, onDelete: () => {}, onCopy: op, onMove: op, onDownloadZip: async () => {} });
    for (const name of [/^Copy 2/, /^Move 2/, /^Download 2/, /^Delete 2/]) {
      expect(screen.getByRole('button', { name })).toBeDisabled();
    }
  });
});

describe('copy / move destination', () => {
  async function openPicker(op: 'Copy' | 'Move', handler: Op) {
    const user = userEvent.setup();
    bar(op === 'Copy' ? { onCopy: handler } : { onMove: handler });
    await user.click(screen.getByRole('button', { name: `${op} 2 selected items` }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getAllByText(`${op} 2 items`).length).toBeGreaterThan(0);
    const path = within(dialog).getByPlaceholderText('/ (bucket root)');
    const ok = within(dialog).getByRole('button', { name: `${op} 2 items` });
    return { user, dialog, path, ok };
  }

  test('starts at the browsed folder, which is the source: confirm is refused', async () => {
    const { dialog, path, ok } = await openPicker('Copy', vi.fn());
    expect(path).toHaveValue('builds');
    expect(within(dialog).getByText(/already in this folder/)).toBeInTheDocument();
    expect(ok).toBeDisabled();
  });

  test.each(['..', 'a/../b', './x', 'builds/.'])('refuses the path %j', async (bad) => {
    const handler = vi.fn<Op>();
    const { user, dialog, path, ok } = await openPicker('Copy', handler);
    await user.clear(path);
    await user.type(path, bad);
    expect(within(dialog).getByRole('alert')).toHaveTextContent('Folder names cannot be "." or "..".');
    expect(path).toHaveAttribute('aria-invalid', 'true');
    expect(ok).toBeDisabled();
    expect(handler).not.toHaveBeenCalled();
  });

  test('a valid path is normalized and sent; success is reported', async () => {
    const handler = vi.fn<Op>(async () => ({ succeeded: 2, failed: 0 }));
    const { user, dialog, path, ok } = await openPicker('Move', handler);
    expect(within(dialog).getByText('Source files will be deleted after successful copy.')).toBeInTheDocument();
    await user.clear(path);
    await user.type(path, '/old//builds/');
    expect(within(dialog).getByText('releases/old/builds/')).toBeInTheDocument();
    expect(ok).toBeEnabled();
    await user.click(ok);
    await waitFor(() => expect(handler).toHaveBeenCalledWith('releases', 'old/builds/'));
    expect(await screen.findByText('2 items moved')).toBeInTheDocument();
  });

  test('a partial failure warns with the counts', async () => {
    const { user, path, ok } = await openPicker('Copy', async () => ({ succeeded: 1, failed: 1 }));
    await user.clear(path);
    await user.type(path, 'elsewhere');
    await user.click(ok);
    expect(await screen.findByText('1 succeeded, 1 failed')).toBeInTheDocument();
  });

  test('an expired admin session hands off instead of showing an error', async () => {
    const onSessionExpired = vi.fn();
    const user = userEvent.setup();
    bar({
      onSessionExpired,
      onCopy: async () => {
        throw new ApiError('Bulk copy failed (401): unauthorized', 401, 'unauthorized');
      },
    });
    await user.click(screen.getByRole('button', { name: 'Copy 2 selected items' }));
    const dialog = await screen.findByRole('dialog');
    const path = within(dialog).getByPlaceholderText('/ (bucket root)');
    await user.clear(path);
    await user.type(path, 'elsewhere');
    await user.click(within(dialog).getByRole('button', { name: 'Copy 2 items' }));
    await waitFor(() => expect(onSessionExpired).toHaveBeenCalledTimes(1));
    expect(screen.queryByText(/unauthorized/)).toBeNull();
  });
});
