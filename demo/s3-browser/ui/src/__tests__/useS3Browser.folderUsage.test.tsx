/**
 * Folder-size autopopulate (GET /usage?prefix)
 * as App wires it: useS3Browser().folders -> ObjectTable -> useComputeSize.
 * A usage hit re-renders the parent. When the folder list was a fresh array per
 * render, the effect sent GET /usage again on each render: a request loop.
 */
import { act } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => 'releases',
  setBucket: () => {},
  listObjects: async () => ({
    objects: Array.from({ length: 100 }, (_, i) => ({ key: `app-${i}.zip`, size: 1, lastModified: '2026-01-01T00:00:00Z' })),
    folders: ['builds/', 'nightly/'],
    isTruncated: false,
  }),
  headObject: async () => ({ storageType: 'delta', storedSize: 1 }),
}));

import useS3Browser from '../useS3Browser';
import useComputeSize from '../useComputeSize';
import ObjectTable from '../components/ObjectTable';

function Harness({ freshArray = false }: { freshArray?: boolean }) {
  const s3 = useS3Browser({ bucket: 'releases', prefix: '', q: '', object: '', navigateUrl: () => {}, adminSession: false });
  const folderSize = useComputeSize();
  return (
    <ObjectTable
      objects={s3.objects}
      folders={freshArray ? [...s3.folders] : s3.folders}
      prefix={s3.prefix}
      selected={null}
      onSelect={() => {}}
      onNavigate={() => {}}
      selectedKeys={s3.selectedKeys}
      onToggleKey={() => {}}
      onToggleAll={() => {}}
      isMobile={false}
      isTruncated={false}
      refreshing={false}
      headCache={s3.headCache}
      onEnrichKeys={s3.enrichKeys}
      folderSizes={folderSize.sizes}
      virtualFolders={[]}
      hasAdminSession
      onComputeSize={() => {}}
      onCancelSize={() => {}}
      onAutoPopulateSizes={folderSize.autoPopulate}
    />
  );
}

const USAGE = '/_/api/admin/usage';
const CACHED_SCAN = {
  prefix: '',
  bucket: 'releases',
  total_size: 10,
  total_objects: 2,
  children: { 'builds/': { size: 5, objects: 1 }, 'nightly/': { size: 5, objects: 1 } },
  computed_at: '2026-10-12T00:00:00Z',
  age_seconds: 1,
  stale_seconds: 0,
  truncated: false,
};
let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  Object.defineProperty(document, 'hidden', { configurable: true, value: false });
});
afterEach(() => vi.unstubAllGlobals());

async function settle(ms: number) {
  await act(async () => {
    await new Promise((r) => setTimeout(r, ms));
  });
}

test('no cached scan for the folder: one GET per folder visit', async () => {
  http.on('GET', USAGE, json({ cached: false }));
  renderWithQuery(<Harness />);
  await settle(1000);
  expect(http.callsTo('GET', USAGE)).toHaveLength(1);
});

test('a cached scan with children: one GET, not a request loop', async () => {
  http.on('GET', USAGE, json(CACHED_SCAN));
  renderWithQuery(<Harness />);
  await settle(1000);
  expect(http.callsTo('GET', USAGE)).toHaveLength(1);
});

test('a parent that rebuilds the folder array on each render: still one GET', async () => {
  http.on('GET', USAGE, json(CACHED_SCAN));
  renderWithQuery(<Harness freshArray />);
  await settle(1000);
  expect(http.callsTo('GET', USAGE)).toHaveLength(1);
});
