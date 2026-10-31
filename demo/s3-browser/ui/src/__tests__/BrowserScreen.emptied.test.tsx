/**
 * UX harvest D13: a freshly created bucket said "qa-created-bucket is now
 * empty." — the wording for a folder the user just emptied. On the first
 * render at a new bucket the listing still holds the previous bucket's rows
 * (it resets after the render), and those counted as the new bucket's.
 */
import { screen } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import type { ComponentProps } from 'react';
import BrowserScreen from '../screens/BrowserScreen';
import { renderWithQuery } from '../test/render';
import type { S3Object } from '../types';

type Props = ComponentProps<typeof BrowserScreen>;

const row: S3Object = { key: 'a.zip', size: 1, lastModified: '2026-10-31T00:00:00Z' } as S3Object;

function s3(objects: S3Object[]): Props['s3'] {
  return {
    objects, folders: [], allFolders: [], virtualFolders: [], prefix: '', loading: false, refreshing: false,
    isTruncated: false, restListedAt: null, headCache: {}, error: null, searchQuery: '', showHidden: false,
    selectedKeys: new Set<string>(), bulkRunning: false, bulkProgress: null, inspectorObject: null,
    mutate: vi.fn(), navigate: vi.fn(), toggleKey: vi.fn(), toggleAll: vi.fn(), openInspector: vi.fn(),
  } as unknown as Props['s3'];
}

function props(bucket: string, objects: S3Object[]): Props {
  return {
    s3: s3(objects),
    bucket,
    caps: { canAdmin: false, canUpload: true, canCreateBucket: true, bulk: {} } as unknown as Props['caps'],
    identity: null,
    jobs: { active: null, deletes: [], writesBlocked: false, refetch: vi.fn() },
    bucketCount: 2,
    isMobile: false,
    folderSizes: {} as Props['folderSizes'],
    cursor: {} as Props['cursor'],
    onPreview: vi.fn(),
    onUpload: vi.fn(),
    onCreateBucket: vi.fn(),
    onSessionExpired: vi.fn(),
  };
}

test('a new bucket opened right after one with rows is "No objects yet", not "is now empty"', () => {
  const { rerender } = renderWithQuery(<BrowserScreen {...props('releases', [row])} />);
  // First render at the new bucket: the old rows are still there.
  rerender(<BrowserScreen {...props('qa-created-bucket', [row])} />);
  // The listing resets: the new bucket is empty.
  rerender(<BrowserScreen {...props('qa-created-bucket', [])} />);
  expect(screen.queryByText(/is now empty/)).toBeNull();
  expect(screen.getByText('No objects yet')).toBeInTheDocument();
});

test('a folder the user emptied still says so', () => {
  const { rerender } = renderWithQuery(<BrowserScreen {...props('releases', [row])} />);
  rerender(<BrowserScreen {...props('releases', [row])} />);
  rerender(<BrowserScreen {...props('releases', [])} />);
  expect(screen.getByText('releases is now empty.')).toBeInTheDocument();
});
