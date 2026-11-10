/**
 * F60 (epic #146): when the presigned URL could not be made, the share dialog
 * showed the bare object URL under "Signed link ready — Expires in 7 days".
 * That URL is not signed and does not expire; it works only for a public
 * prefix. A failed signing is now an error, never a link that claims to be signed.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import type { S3Object } from '../types';
import { renderWithQuery } from '../test/render';

const s3 = vi.hoisted(() => ({
  getPresignedUrl: vi.fn(async (_key: string, _seconds?: number) => 'http://proxy/releases/builds/app.zip?X-Amz-Signature=abc'),
}));
vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  deleteObject: vi.fn(),
  headObject: async () => ({ headers: {}, storageType: 'delta', storedSize: 5 }),
  downloadObject: vi.fn(),
  getDownloadUrl: vi.fn(),
  getPresignedUrl: s3.getPresignedUrl,
  getObjectUrl: () => 'http://proxy/releases/builds/app.zip',
}));

import InspectorPanel from '../components/InspectorPanel';

afterEach(() => vi.restoreAllMocks());

const OBJ = { key: 'builds/app.zip', size: 5, lastModified: '2026-01-01T00:00:00Z' } as S3Object;

async function share() {
  const user = userEvent.setup();
  renderWithQuery(<InspectorPanel object={OBJ} onClose={() => {}} onDeleted={() => {}} hasAdminSession={false} />);
  await user.click(await screen.findByRole('button', { name: 'Share link (7 days)' }));
}

test('a signed link is shown with its expiry', async () => {
  await share();
  expect(await screen.findByText('Signed link ready')).toBeInTheDocument();
  expect(screen.getByTestId('share-url')).toHaveTextContent('X-Amz-Signature=abc');
  expect(screen.getByText(/Expires in 7 days/)).toBeInTheDocument();
});

test('when signing fails there is an error and no link that claims to be signed', async () => {
  s3.getPresignedUrl.mockRejectedValueOnce(new Error('no credentials'));
  await share();
  expect(await screen.findByText('Failed to generate link')).toBeInTheDocument();
  expect(screen.queryByText('Signed link ready')).toBeNull();
  expect(screen.queryByTestId('share-url')).toBeNull();
  expect(screen.queryByText(/Expires in/)).toBeNull();
});
