/**
 * The inspector seeds its storage stats from the listing's facts and sends
 * one HEAD per opened object for the full header view (P8.3). A refresh that
 * hands over a new row object for the same key sends no second HEAD.
 */
import { screen, waitFor } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import type { S3Object } from '../types';
import { renderWithQuery } from '../test/render';

const s3 = vi.hoisted(() => ({
  heads: [] as string[],
  answer: null as null | (() => void),
}));
vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  deleteObject: vi.fn(),
  headObject: (key: string) => {
    s3.heads.push(key);
    // Held until the test answers: the seeded stats must render before it.
    return new Promise((resolve) => {
      s3.answer = () => resolve({ headers: { 'x-amz-meta-dg-note': 'delta' }, storageType: 'delta', storedSize: 7 });
    });
  },
  downloadObject: vi.fn(),
  getPresignedUrl: async () => 'http://example/presigned',
  getObjectUrl: () => 'http://example/object',
}));

import InspectorPanel from '../components/InspectorPanel';

const row = (key: string): S3Object => ({
  key, size: 100, lastModified: '2026-01-01T00:00:00Z', etag: '"e"', storageType: 'delta', storedSize: 7, headers: {},
});
const props = { onClose: () => {}, onDeleted: () => {}, hasAdminSession: false };

test('one HEAD per opened object; a refreshed row for the same key sends none', async () => {
  s3.heads = [];
  const view = renderWithQuery(<InspectorPanel {...props} object={row('builds/app.zip')} />);
  await waitFor(() => expect(s3.heads).toEqual(['builds/app.zip']));
  // Seeded from the listing while the HEAD is still in flight.
  expect(await screen.findByText(/93(\.0)?%/)).toBeInTheDocument();

  view.rerender(<InspectorPanel {...props} object={row('builds/app.zip')} />);
  await new Promise((r) => setTimeout(r, 50));
  expect(s3.heads).toHaveLength(1);

  view.rerender(<InspectorPanel {...props} object={row('builds/other.zip')} />);
  await waitFor(() => expect(s3.heads).toEqual(['builds/app.zip', 'builds/other.zip']));
});
