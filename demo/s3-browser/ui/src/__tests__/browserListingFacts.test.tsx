/**
 * The browser table reads the storage facts of each row from the listing
 * (ListObjectsV2 `metadata=true`, P8.2), so a folder visit sends no HEAD.
 * The real s3client runs here: the AWS SDK sends through fetch (its browser
 * build), and mockFetch fails every request without a route, a HEAD included.
 */
import { act, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch, type RecordedRequest } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { initFromSession, parseListFacts } from '../s3client';
import useS3Browser from '../useS3Browser';
import ObjectTable from '../components/ObjectTable';
import StorageTypeTag from '../components/StorageTypeTag';

interface Entry {
  key: string;
  note?: string;
  deltaSize?: number;
  baseline?: boolean;
}

const escapeXml = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

function listXml(prefix: string, entries: Entry[], next?: string): string {
  const contents = entries
    .map((e) => {
      const items = [
        e.deltaSize !== undefined
          ? `<Items><Key>x-amz-meta-dg-delta-size</Key><Value>${e.deltaSize}</Value></Items>`
          : '',
        e.note ? `<Items><Key>x-amz-meta-dg-note</Key><Value>${e.note}</Value></Items>` : '',
        e.baseline ? '<Items><Key>x-amz-meta-dg-baseline</Key><Value>true</Value></Items>' : '',
      ].join('');
      return `<Contents><Key>${escapeXml(e.key)}</Key><LastModified>2026-10-01T00:00:00.000Z</LastModified>`
        + `<ETag>"abc"</ETag><Size>100</Size><StorageClass>STANDARD</StorageClass>`
        + (items ? `<UserMetadata>${items}</UserMetadata>` : '')
        + `</Contents>`;
    })
    .join('');
  return `<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">`
    + `<Name>releases</Name><Prefix>${prefix}</Prefix><KeyCount>${entries.length}</KeyCount><MaxKeys>1000</MaxKeys>`
    + `<Delimiter>/</Delimiter><IsTruncated>${next ? 'true' : 'false'}</IsTruncated>`
    + (next ? `<NextContinuationToken>${next}</NextContinuationToken>` : '')
    + contents + `</ListBucketResult>`;
}

const deltas = (folder: string, n: number): Entry[] =>
  Array.from({ length: n }, (_, i) => ({ key: `${folder}app-${i}.zip`, note: 'delta', deltaSize: 7 }));

let http: ReturnType<typeof mockFetch>;
/** The listing of each folder, by prefix; a page token selects a later page. */
let folders: Record<string, string[]>;
/** The facts the table renders from, as the hook last handed them over. */
let cache: Record<string, { storageType?: string; storedSize?: number; isBaseline?: boolean }> = {};

const lists = () => http.calls.filter((c) => c.method === 'GET' && /^\/releases\/?\?/.test(c.path));
const heads = () => http.calls.filter((c) => c.method === 'HEAD');
const query = (c: RecordedRequest) => new URLSearchParams(c.path.split('?')[1]);

beforeEach(async () => {
  http = mockFetch();
  folders = {};
  cache = {};
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://proxy.test', region: 'us-east-1', access_key_id: 'ak', secret_access_key: 'sk', bucket: 'releases',
  }));
  http.on('GET', /^\/releases\/?\?/, (req) => {
    const q = query(req);
    const pages = folders[q.get('prefix') ?? ''] ?? [listXml(q.get('prefix') ?? '', [])];
    const page = Number(q.get('continuation-token') ?? 0);
    return new Response(pages[page], { status: 200, headers: { 'content-type': 'application/xml' } });
  });
  http.on('GET', /\/_\/api\/admin\/usage/, json({ cached: false }));
  Object.defineProperty(document, 'hidden', { configurable: true, value: false });
  await initFromSession();
});
afterEach(() => vi.unstubAllGlobals());

function Harness({ prefix }: { prefix: string }) {
  const s3 = useS3Browser({ bucket: 'releases', prefix, q: '', object: '', navigateUrl: () => {}, adminSession: false });
  cache = s3.headCache;
  return (
    <ObjectTable
      listing={{
        objects: s3.objects,
        folders: s3.folders,
        prefix: s3.prefix,
        isTruncated: false,
        refreshing: false,
        headCache: s3.headCache,
        virtualFolders: [],
      }}
      selection={{ inspected: null, selectedKeys: s3.selectedKeys, onToggleKey: () => {}, onToggleAll: () => {} }}
      actions={{ onSelect: () => {}, onNavigate: () => {} }}
      folderSizes={{ sizes: {}, canScan: false, onCompute: () => {}, onCancel: () => {}, onAutoPopulate: () => {} }}
      isMobile={false}
    />
  );
}

async function settle(ms = 300) {
  await act(async () => {
    await new Promise((r) => setTimeout(r, ms));
  });
}

test('a folder of 100 files: one signed LIST with metadata=true, 0 HEADs', async () => {
  folders['a/'] = [listXml('a/', deltas('a/', 100))];
  renderWithQuery(<Harness prefix="a/" />);
  await waitFor(() => expect(Object.keys(cache)).toHaveLength(100));
  expect(cache['a/app-99.zip']).toEqual({ storageType: 'delta', storedSize: 7 });
  await settle();
  expect(heads()).toHaveLength(0);
  expect(lists()).toHaveLength(1);
  const list = lists()[0];
  expect(query(list).get('metadata')).toBe('true');
  expect(list.headers.authorization).toMatch(/^AWS4-HMAC-SHA256 /);
});

test('A -> B -> back to A: 0 HEADs', async () => {
  folders['a/'] = [listXml('a/', deltas('a/', 100))];
  folders['b/'] = [listXml('b/', deltas('b/', 100))];
  const view = renderWithQuery(<Harness prefix="a/" />);
  await waitFor(() => expect(lists()).toHaveLength(1));
  view.rerender(<Harness prefix="b/" />);
  await waitFor(() => expect(lists()).toHaveLength(2));
  view.rerender(<Harness prefix="a/" />);
  await waitFor(() => expect(lists()).toHaveLength(3));
  await settle();
  expect(heads()).toHaveLength(0);
});

test('every page of a paginated listing carries its own facts', async () => {
  folders['p/'] = [
    listXml('p/', [{ key: 'p/one.zip', note: 'delta', deltaSize: 7 }], '1'),
    listXml('p/', [{ key: 'p/two.zip', note: 'reference' }]),
  ];
  renderWithQuery(<Harness prefix="p/" />);
  await waitFor(() => expect(Object.keys(cache)).toHaveLength(2));
  expect(cache['p/one.zip']).toEqual({ storageType: 'delta', storedSize: 7 });
  expect(cache['p/two.zip']).toEqual({ storageType: 'reference', storedSize: undefined });
  expect(lists()).toHaveLength(2);
  expect(lists().every((c) => query(c).get('metadata') === 'true')).toBe(true);
  expect(heads()).toHaveLength(0);
});

test('a row without facts shows the neutral tag, not a spinner', async () => {
  folders['m/'] = [listXml('m/', [{ key: 'm/odd.zip' }, { key: 'm/a.txt', note: 'passthrough' }])];
  renderWithQuery(<Harness prefix="m/" />);
  await waitFor(() => expect(Object.keys(cache)).toHaveLength(2));
  // The row is in the cache with no type: the table renders a tag for it.
  expect(cache['m/odd.zip']).toEqual({ storageType: undefined, storedSize: undefined });
  expect(cache['m/a.txt']).toEqual({ storageType: 'passthrough', storedSize: undefined });
  renderWithQuery(<StorageTypeTag storageType={undefined} />);
  expect(screen.getByText('—')).toBeTruthy();
  await settle();
  expect(heads()).toHaveLength(0);
});

test('parseListFacts reads the facts of each Contents, not the Items keys', () => {
  const facts = parseListFacts(listXml('', [
    { key: 'r&d/a.zip', note: 'delta', deltaSize: 42 },
    { key: 'b.bin', note: 'passthrough' },
    { key: 'c.zip' },
  ]));
  expect([...facts.keys()]).toEqual(['r&d/a.zip', 'b.bin']);
  expect(facts.get('r&d/a.zip')).toEqual({ storageType: 'delta', storedSize: 42 });
  expect(facts.get('b.bin')).toEqual({ storageType: 'passthrough', storedSize: undefined });
});

test('the listing marks the baseline delta, and the row facts carry it to the table', async () => {
  const facts = parseListFacts(listXml('', [
    { key: 'v1.zip', note: 'delta', deltaSize: 9, baseline: true },
    { key: 'v2.zip', note: 'delta', deltaSize: 7 },
  ]));
  expect(facts.get('v1.zip')).toEqual({ storageType: 'delta', storedSize: 9, isBaseline: true });
  expect(facts.get('v2.zip')?.isBaseline).toBeUndefined();

  folders['r/'] = [listXml('r/', [
    { key: 'r/v1.zip', note: 'delta', deltaSize: 9, baseline: true },
    { key: 'r/v2.zip', note: 'delta', deltaSize: 7 },
  ])];
  renderWithQuery(<Harness prefix="r/" />);
  await waitFor(() => expect(Object.keys(cache)).toHaveLength(2));
  // The table's chip reads these facts (storageChip: "Baseline" vs "Delta").
  expect(cache['r/v1.zip']).toEqual({ storageType: 'delta', storedSize: 9, isBaseline: true });
  expect(cache['r/v2.zip'].isBaseline).toBeUndefined();
  expect(heads()).toHaveLength(0);
});
