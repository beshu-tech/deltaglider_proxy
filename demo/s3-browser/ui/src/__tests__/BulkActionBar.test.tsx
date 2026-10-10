/**
 * The bulk action bar and its destination picker: delete asks first, copy /
 * move send the chosen destination, and a destination path with a `.` or
 * `..` folder (or the selection's own folder) cannot be confirmed.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, test, vi } from 'vitest';
import type { BulkDeleteOutcome } from '../bulkDelete';
import { ApiError } from '../errorHandling';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  listBuckets: async () => [{ name: 'releases' }, { name: 'archive' }, { name: 'broken', unavailable: true }],
}));

import BulkActionBar from '../components/BulkActionBar';

type Op = (b: string, p: string) => Promise<{ succeeded: number; failed: number }>;
const done = async (): Promise<BulkDeleteOutcome> => ({ total: 3, deleted: 3, failed: 0, failures: [], cancelled: false });

function bar(props: Partial<Parameters<typeof BulkActionBar>[0]> = {}) {
  return renderWithQuery(
    <BulkActionBar
      selectedCount={2}
      selectedFolderCount={1}
      currentPrefix="builds/"
      selectionKeys={['builds/app.zip', 'folder:builds/nightly/']}
      {...props}
    />,
  );
}

describe('delete', () => {
  test('asks for confirmation naming the folder, and deletes only on OK', async () => {
    const user = userEvent.setup();
    const onDelete = vi.fn(done);
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

    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());

    await user.click(screen.getByRole('button', { name: 'Delete 2 selected items' }));
    const again = await screen.findByRole('dialog');
    await user.click(within(again).getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(onDelete).toHaveBeenCalledTimes(1));
  });

  test('actions without a handler are not offered; the hint explains why', () => {
    bar({ hint: 'Sign in as an administrator for bulk actions.' });
    expect(screen.queryByRole('button', { name: /Delete|Copy|Move|ZIP/ })).toBeNull();
    expect(screen.getByText('Sign in as an administrator for bulk actions.')).toBeInTheDocument();
  });

  test('while a delete runs the bar shows its phase, a named progress bar and Cancel instead of the actions', async () => {
    const user = userEvent.setup();
    const op: Op = async () => ({ succeeded: 0, failed: 0 });
    const onCancelDelete = vi.fn();
    const props = { onDelete: done, onCopy: op, onMove: op, onDownloadZip: async () => {}, onCancelDelete };
    const view = bar({
      ...props,
      deleteProgress: { phase: 'listing', listed: 12, folders: 50, keysFound: 4310, stopping: false },
    });
    const toolbar = screen.getByRole('toolbar', { name: 'Selection actions' });
    expect(within(toolbar).getByRole('status')).toHaveTextContent('Listing folders 12 of 50… 4,310 objects found');
    const progress = within(toolbar).getByRole('progressbar', { name: 'Bulk delete progress' });
    expect(progress).toHaveAttribute('aria-valuenow', '24');
    expect(within(toolbar).queryByRole('button', { name: /^(Copy|Move|Download|Delete) 2/ })).toBeNull();
    await user.click(within(toolbar).getByRole('button', { name: 'Cancel delete' }));
    expect(onCancelDelete).toHaveBeenCalledTimes(1);

    view.rerender(<BulkActionBar selectedCount={2} {...props} deleteProgress={{ phase: 'deleting', done: 3400, total: 9800, stopping: false }} />);
    expect(within(toolbar).getByRole('status')).toHaveTextContent('Deleting 3,400 of 9,800…');
    expect(within(toolbar).getByRole('progressbar', { name: 'Bulk delete progress' })).toHaveAttribute('aria-valuenow', '34');

    view.rerender(<BulkActionBar selectedCount={2} {...props} deleteProgress={{ phase: 'deleting', done: 3400, total: 9800, stopping: true }} />);
    expect(within(toolbar).getByRole('status')).toHaveTextContent('Stopping after this batch… 3,400 of 9,800 done');
    expect(within(toolbar).getByRole('button', { name: 'Cancel delete' })).toBeDisabled();
  });

  async function confirmDeleteWith(onDelete: () => Promise<BulkDeleteOutcome>, extra: Parameters<typeof bar>[0] = {}) {
    const user = userEvent.setup();
    bar({ onDelete, ...extra });
    await user.click(screen.getByRole('button', { name: 'Delete 2 selected items' }));
    await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Delete' }));
  }

  test('a finished delete reports what it did', async () => {
    await confirmDeleteWith(async () => ({ total: 9800, deleted: 9800, failed: 0, failures: [], cancelled: false }));
    expect(await screen.findByText('9,800 objects deleted')).toBeInTheDocument();
  });

  test('a cancelled delete reports how far it got', async () => {
    await confirmDeleteWith(async () => ({ total: 9800, deleted: 3400, failed: 0, failures: [], cancelled: true }));
    expect(await screen.findByText('Delete stopped. 3,400 of 9,800 objects deleted.')).toBeInTheDocument();
  });

  test('a delete that stops on an error shows the error with its count', async () => {
    await confirmDeleteWith(async () => {
      throw new Error('Bulk delete failed (500): disk full. 500 of 1,201 objects were deleted before the failure.');
    });
    expect(
      await screen.findByText('Bulk delete failed (500): disk full. 500 of 1,201 objects were deleted before the failure.'),
    ).toBeInTheDocument();
  });

  test('an expired session during a delete hands off instead of showing an error', async () => {
    const onSessionExpired = vi.fn();
    await confirmDeleteWith(
      async () => {
        throw new ApiError('Bulk delete failed (401): unauthorized', 401, 'unauthorized');
      },
      { onSessionExpired },
    );
    await waitFor(() => expect(onSessionExpired).toHaveBeenCalledTimes(1));
    expect(screen.queryByText(/unauthorized/)).toBeNull();
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
