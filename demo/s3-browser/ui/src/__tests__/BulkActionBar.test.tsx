/**
 * The bulk action bar and its destination picker: delete counts, then asks
 * with the exact number, copy /
 * move send the chosen destination, and a destination path with a `.` or
 * `..` folder (or the selection's own folder) cannot be confirmed.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, test, vi } from 'vitest';
import type { DeleteJobOutcome, DeletePlanSummary } from '../bulkDelete';
import type { BulkTransferOutcome } from '../bulkTransfer';
import { ApiError } from '../errorHandling';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  listBuckets: async () => [{ name: 'releases' }, { name: 'archive' }, { name: 'broken', unavailable: true }],
}));

import BulkActionBar from '../components/BulkActionBar';

type Op = (b: string, p: string) => Promise<BulkTransferOutcome>;
const transferred = (action: 'copy' | 'move', over: Partial<BulkTransferOutcome> = {}): BulkTransferOutcome => ({
  action, total: 2, succeeded: 2, failed: 0, deleted: action === 'move' ? 2 : 0, failures: [], cancelled: false, ...over,
});
const done = async (): Promise<DeleteJobOutcome> => ({ total: 3, cancelled: false, failed: 0, jobId: 1 });
type Confirm = (plan: DeletePlanSummary) => Promise<boolean>;
/** A delete as the hook runs it: count, ask, then run `then` or end with nothing done. */
const askingDelete =
  (plan: DeletePlanSummary, then: () => Promise<DeleteJobOutcome>) =>
  async (confirm: Confirm): Promise<DeleteJobOutcome> =>
    (await confirm(plan)) ? then() : { total: 0, cancelled: true, failed: 0 };

function bar(props: Partial<Parameters<typeof BulkActionBar>[0]> = {}) {
  return renderWithQuery(
    <BulkActionBar
      selectedCount={2}
      currentPrefix="builds/"
      selectionKeys={['builds/app.zip', 'folder:builds/nightly/']}
      {...props}
    />,
  );
}

describe('delete', () => {
  test('counts first, then asks with the exact number; deletes only on OK', async () => {
    const user = userEvent.setup();
    const deleted = vi.fn(done);
    bar({ onDelete: askingDelete({ objects: 1_201, folders: 1, files: 1 }, deleted) });
    await user.click(screen.getByRole('button', { name: 'Delete 2 selected items' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getAllByText('Delete 1,201 objects permanently?').length).toBeGreaterThan(0);
    expect(
      within(dialog).getByText(
        'You selected 1 folder and 1 object. With everything inside the folder, that is 1,201 objects. This cannot be undone.',
      ),
    ).toBeInTheDocument();
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    expect(await screen.findByText('Delete cancelled. Nothing was deleted.')).toBeInTheDocument();
    expect(deleted).not.toHaveBeenCalled();
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());

    await user.click(screen.getByRole('button', { name: 'Delete 2 selected items' }));
    await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(deleted).toHaveBeenCalledTimes(1));
  });

  test('a large delete explains how it runs and names the count on its button', async () => {
    const user = userEvent.setup();
    bar({ onDelete: askingDelete({ objects: 230_412, folders: 2, files: 0 }, done) });
    await user.click(screen.getByRole('button', { name: 'Delete 2 selected items' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/It runs on the server, so you can close this tab\./)).toBeInTheDocument();
    expect(
      within(dialog).getByText('Cancel stops it after the page in flight. Objects deleted before that stay deleted.'),
    ).toBeInTheDocument();
    expect(within(dialog).getByRole('button', { name: 'Delete 230,412 objects' })).toBeInTheDocument();
  });

  test('actions without a handler are not offered; the hint explains why', () => {
    bar({ hint: 'Sign in as an administrator for bulk actions.' });
    expect(screen.queryByRole('button', { name: /Delete|Copy|Move|ZIP/ })).toBeNull();
    expect(screen.getByText('Sign in as an administrator for bulk actions.')).toBeInTheDocument();
  });

  test('while a delete runs the bar shows its phase, a named progress bar and Cancel instead of the actions', async () => {
    const user = userEvent.setup();
    const op: Op = async () => transferred('copy');
    const onCancel = vi.fn();
    const props = { onDelete: done, onCopy: op, onMove: op, onDownloadZip: async () => {}, onCancel };
    const view = bar({
      ...props,
      progress: { action: 'delete', phase: 'listing', listed: 12, folders: 50, keysFound: 4310, stopping: false },
    });
    const toolbar = screen.getByRole('toolbar', { name: 'Selection actions' });
    expect(within(toolbar).getByRole('status')).toHaveTextContent(
      'Counting objects to delete in folder 13 of 50… 4,310 found. Nothing is deleted yet.',
    );
    // A folder's size is unknown until it is listed: no percent while counting.
    expect(within(toolbar).queryByRole('progressbar')).toBeNull();
    expect(within(toolbar).queryByRole('button', { name: /^(Copy|Move|Download|Delete) 2/ })).toBeNull();
    await user.click(within(toolbar).getByRole('button', { name: 'Cancel delete' }));
    expect(onCancel).toHaveBeenCalledTimes(1);

    // A delete only starts its server job in the bar: the bucket shows it then.
    view.rerender(<BulkActionBar selectedCount={2} {...props} progress={{ action: 'delete', phase: 'sending', done: 0, total: 9800, stopping: false }} />);
    expect(within(toolbar).getByRole('status')).toHaveTextContent('Starting the delete of 9,800 objects…');
    expect(within(toolbar).queryByRole('progressbar')).toBeNull();

    view.rerender(<BulkActionBar selectedCount={2} {...props} progress={{ action: 'copy', phase: 'sending', done: 3400, total: 9800, stopping: true }} />);
    expect(within(toolbar).getByRole('status')).toHaveTextContent('Stopping after this batch… 3,400 of 9,800 done');
    expect(within(toolbar).getByRole('button', { name: 'Cancel copy' })).toBeDisabled();

    // A copy or move shows the same, with its own verb.
    view.rerender(<BulkActionBar selectedCount={2} {...props} progress={{ action: 'move', phase: 'sending', done: 500, total: 1201, stopping: false }} />);
    expect(within(toolbar).getByRole('status')).toHaveTextContent('Moving 500 of 1,201 · 41%…');
    expect(within(toolbar).getByRole('progressbar', { name: 'Bulk move progress' })).toHaveAttribute('aria-valuenow', '41');
    await user.click(within(toolbar).getByRole('button', { name: 'Cancel move' }));
    expect(onCancel).toHaveBeenCalledTimes(2);
  });

  async function confirmDeleteWith(run: () => Promise<DeleteJobOutcome>, extra: Parameters<typeof bar>[0] = {}) {
    const user = userEvent.setup();
    bar({ onDelete: askingDelete({ objects: 9_800, folders: 1, files: 1 }, run), ...extra });
    await user.click(screen.getByRole('button', { name: 'Delete 2 selected items' }));
    await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Delete' }));
  }

  test('a started delete says it runs on the server', async () => {
    await confirmDeleteWith(async () => ({ total: 9800, cancelled: false, failed: 0, jobId: 4 }));
    expect(
      await screen.findByText('Deleting 9,800 objects on the server. You can close this tab: the bucket shows the progress.'),
    ).toBeInTheDocument();
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
    const handler = vi.fn<Op>(async () => transferred('move'));
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
    const { user, path, ok } = await openPicker('Copy', async () =>
      transferred('copy', {
        succeeded: 1,
        failed: 1,
        failures: [{ source_key: 'builds/app.zip', dest_key: 'elsewhere/app.zip', error: 'AccessDenied' }],
      }),
    );
    await user.clear(path);
    await user.type(path, 'elsewhere');
    await user.click(ok);
    expect(
      await screen.findByText('1 of 2 items copied. 1 failed, for example builds/app.zip: AccessDenied.'),
    ).toBeInTheDocument();
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
