/**
 * Browser-review item 22: Download read the whole object into a Blob in page
 * memory (a 2 GB file = 2 GB of tab memory, and nothing on disk until the
 * last byte), then asked for a second click. It now hands a short-lived
 * presigned URL to the browser's own downloader, which streams to disk.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import type { S3Object } from '../types';
import { renderWithQuery } from '../test/render';

const s3 = vi.hoisted(() => ({
  downloadObject: vi.fn(async () => new Blob(['x'])),
  getDownloadUrl: vi.fn(async (_key: string, _name: string) => 'http://proxy/releases/builds/app.zip?X-Amz-Signature=abc'),
}));
vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  deleteObject: vi.fn(),
  headObject: async () => ({ headers: {}, storageType: 'delta', storedSize: 5 }),
  downloadObject: s3.downloadObject,
  getDownloadUrl: s3.getDownloadUrl,
  getPresignedUrl: async () => 'http://example/presigned',
  getObjectUrl: () => 'http://example/object',
}));

import InspectorPanel from '../components/InspectorPanel';

afterEach(() => vi.restoreAllMocks());

test('Download streams through a presigned URL, not a Blob', async () => {
  const clicked: { href: string; download: string }[] = [];
  vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function (this: HTMLAnchorElement) {
    clicked.push({ href: this.href, download: this.download });
  });
  const user = userEvent.setup();
  const obj = { key: 'builds/app.zip', size: 5, lastModified: '2026-01-01T00:00:00Z' } as S3Object;
  renderWithQuery(<InspectorPanel object={obj} onClose={() => {}} onDeleted={() => {}} hasAdminSession={false} />);
  await user.click(await screen.findByRole('button', { name: /^(download )?Download$/ }));
  await waitFor(() => expect(clicked).toHaveLength(1));
  expect(clicked[0]).toEqual({ href: 'http://proxy/releases/builds/app.zip?X-Amz-Signature=abc', download: 'app.zip' });
  expect(s3.getDownloadUrl).toHaveBeenCalledWith('builds/app.zip', 'app.zip');
  expect(s3.downloadObject).not.toHaveBeenCalled();
});
