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
import { installDialogTabWrap } from '../dialogTabWrap';

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
test('a passthrough object on a compressing bucket says it is stored whole', async () => {
  setup();
  expect(await screen.findByText(/Stored whole/)).toBeInTheDocument();
  expect(screen.queryByText(/compression is off/i)).not.toBeInTheDocument();
});

// UX harvest C31: a double-click on an object with no preview opened the
// drawer and closed it again at once. The drawer now stays open.
test('the second click of a double-click keeps the drawer open for an object with no preview', async () => {
  const onClose = vi.fn();
  const onPreview = vi.fn();
  const zip = { key: 'builds/app-v1.zip', size: 5, lastModified: '2026-01-01T00:00:00Z' } as S3Object;
  renderWithQuery(
    <InspectorPanel object={zip} onClose={onClose} onDeleted={() => {}} onPreview={onPreview} hasAdminSession={false} />,
  );
  await screen.findByText('app-v1.zip');
  fireEvent.click(document.querySelector('.ant-drawer-mask')!, { detail: 2 });
  expect(onClose).not.toHaveBeenCalled();
  expect(onPreview).not.toHaveBeenCalled();
});

// UX harvest C11: Tab from the last control stopped once on <body> before
// the focus lock pulled it back. Tab now wraps inside the drawer.
// The app-wide dialogTabWrap (ThemeProvider installs it) covers the drawer:
// it shows its mask, so it traps focus.
test('Tab from the last control goes to the first one, Shift+Tab back', async () => {
  const uninstall = installDialogTabWrap();
  setup();
  const close = await screen.findByRole('button', { name: 'Close inspector' });
  const del = screen.getByRole('button', { name: /Delete object/ });
  del.focus();
  expect(fireEvent.keyDown(del, { key: 'Tab' })).toBe(false); // default prevented
  expect(document.activeElement).toBe(close);
  fireEvent.keyDown(close, { key: 'Tab', shiftKey: true });
  expect(document.activeElement).toBe(del);
  uninstall();
});

// UX harvest D1: the share link sat in a fixed-height box that cut its last
// line in half with no scrollbar. It now wraps at any character in a box
// that scrolls (with a visible scrollbar) past its height.
test('the share link box wraps the whole URL and scrolls', async () => {
  setup();
  fireEvent.click(await screen.findByRole('button', { name: /share|copy link/i }));
  const options = await screen.findAllByRole('menuitem').catch(() => []);
  if (options.length > 0) fireEvent.click(options[options.length - 1]);
  const box = await screen.findByRole('textbox', { name: 'Share URL' });
  expect(box).toHaveTextContent('http://example/presigned');
  expect(box).toHaveStyle({ overflowY: 'auto', overflowWrap: 'anywhere' });
  expect(box).toHaveAttribute('tabindex', '0');
});
