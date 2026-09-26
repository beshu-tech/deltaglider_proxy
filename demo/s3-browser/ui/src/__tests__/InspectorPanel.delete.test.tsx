/**
 * InspectorPanel "Delete object": it asks before it deletes (fe975c7d), a
 * cancel deletes nothing, a failure keeps the drawer open and says so.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, test, vi } from 'vitest';
import type { S3Object } from '../types';
import { renderWithQuery } from '../test/render';

const s3 = vi.hoisted(() => ({
  deleteObject: vi.fn<(key: string) => Promise<void>>(),
}));
vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  deleteObject: s3.deleteObject,
  headObject: async () => ({ headers: {}, storageType: 'passthrough', storedSize: 5 }),
  downloadObject: async () => new Blob(['x']),
  getPresignedUrl: async () => 'http://example/presigned',
  getObjectUrl: () => 'http://example/object',
}));

import InspectorPanel from '../components/InspectorPanel';

const OBJ: S3Object = { key: 'builds/app-1.0.zip', size: 5, lastModified: '2026-01-01T00:00:00Z' } as S3Object;

beforeEach(() => {
  s3.deleteObject.mockReset();
});

function inspector(props: Partial<Parameters<typeof InspectorPanel>[0]> = {}) {
  const onClose = vi.fn();
  const onDeleted = vi.fn();
  renderWithQuery(
    <InspectorPanel object={OBJ} onClose={onClose} onDeleted={onDeleted} hasAdminSession={false} {...props} />,
  );
  return { onClose, onDeleted };
}

async function openConfirm(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole('button', { name: /Delete object/ }));
  // The drawer is a dialog too: wait for the one that holds the confirmation.
  const confirm = await waitFor(() => {
    const found = screen.getAllByRole('dialog').find((d) => within(d).queryAllByText('Delete permanently?').length > 0);
    expect(found).toBeDefined();
    return found;
  });
  expect(within(confirm!).getByText('Delete "builds/app-1.0.zip"? This cannot be undone.')).toBeInTheDocument();
  return confirm!;
}

test('asks first; Cancel deletes nothing', async () => {
  const user = userEvent.setup();
  const { onClose, onDeleted } = inspector();
  const confirm = await openConfirm(user);
  expect(s3.deleteObject).not.toHaveBeenCalled();
  await user.click(within(confirm).getByRole('button', { name: 'Cancel' }));
  expect(s3.deleteObject).not.toHaveBeenCalled();
  expect(onClose).not.toHaveBeenCalled();
  expect(onDeleted).not.toHaveBeenCalled();
});

test('confirming deletes that key, closes the drawer and refreshes the list', async () => {
  s3.deleteObject.mockResolvedValue(undefined);
  const user = userEvent.setup();
  const { onClose, onDeleted } = inspector();
  const confirm = await openConfirm(user);
  await user.click(within(confirm).getByRole('button', { name: 'Delete' }));
  await waitFor(() => expect(onDeleted).toHaveBeenCalledTimes(1));
  expect(s3.deleteObject).toHaveBeenCalledWith('builds/app-1.0.zip');
  expect(onClose).toHaveBeenCalledTimes(1);
});

test('a failed delete keeps the drawer open and reports it', async () => {
  s3.deleteObject.mockRejectedValue(new Error('AccessDenied'));
  const user = userEvent.setup();
  const { onClose, onDeleted } = inspector();
  const confirm = await openConfirm(user);
  await user.click(within(confirm).getByRole('button', { name: 'Delete' }));
  expect(await screen.findByText('Failed to delete object')).toBeInTheDocument();
  expect(onClose).not.toHaveBeenCalled();
  expect(onDeleted).not.toHaveBeenCalled();
});

test('no delete button without delete permission', async () => {
  inspector({ canDelete: false });
  expect((await screen.findAllByText('app-1.0.zip')).length).toBeGreaterThan(0);
  expect(screen.queryByRole('button', { name: /Delete object/ })).toBeNull();
});
