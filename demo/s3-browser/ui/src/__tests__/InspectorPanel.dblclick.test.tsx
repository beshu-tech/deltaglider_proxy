/**
 * Double-clicking a file row opens the preview.
 *
 * Pins the browser-review finding: the first click of the double-click opens
 * the inspector drawer, so the second click lands on the drawer's mask. The
 * mask closed the drawer and the row never saw a dblclick. The second click
 * of a double-click (event.detail 2) on the mask now opens the preview of the
 * object the first click selected; a single click on the mask still closes.
 */
import { fireEvent, screen } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import type { S3Object } from '../types';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  deleteObject: vi.fn(),
  headObject: async () => ({ headers: {}, storageType: 'passthrough', storedSize: 5 }),
  downloadObject: async () => new Blob(['x']),
  getPresignedUrl: async () => 'http://example/presigned',
  getObjectUrl: () => 'http://example/object',
}));

import InspectorPanel from '../components/InspectorPanel';

const OBJ: S3Object = { key: 'docs/README.txt', size: 5, lastModified: '2026-01-01T00:00:00Z' } as S3Object;

function setup() {
  const onClose = vi.fn();
  const onPreview = vi.fn();
  renderWithQuery(
    <InspectorPanel object={OBJ} onClose={onClose} onDeleted={() => {}} onPreview={onPreview} hasAdminSession={false} />,
  );
  const mask = document.querySelector('.ant-drawer-mask');
  if (!mask) throw new Error('no drawer mask');
  return { onClose, onPreview, mask };
}

test('the second click of a double-click on the mask opens the preview', async () => {
  const { onClose, onPreview, mask } = setup();
  await screen.findByText('README.txt');
  fireEvent.click(mask, { detail: 2 });
  expect(onPreview).toHaveBeenCalledWith(OBJ);
  expect(onClose).not.toHaveBeenCalled();
});

test('a single click on the mask closes the drawer', async () => {
  const { onClose, onPreview, mask } = setup();
  await screen.findByText('README.txt');
  fireEvent.click(mask, { detail: 1 });
  expect(onClose).toHaveBeenCalled();
  expect(onPreview).not.toHaveBeenCalled();
});

// Browser-review item 16: README.txt on a compressing bucket said
// "Compression disabled for this bucket". It is a passthrough object: the
// bucket compresses, this file type is simply stored as-is.
test('a passthrough object on a compressing bucket says it is stored as-is', async () => {
  setup();
  expect(await screen.findByText(/Stored as-is/)).toBeInTheDocument();
  expect(screen.queryByText('Compression disabled for this bucket')).not.toBeInTheDocument();
});
