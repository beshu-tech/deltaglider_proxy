/**
 * The folder picker lists folders with `listCommonPrefixes`. It sent one
 * request of 100 keys and ignored `IsTruncated`, so a folder past the first
 * page was missing. It now reads every page.
 */
import { beforeEach, expect, test } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { initFromSession, listCommonPrefixes } from '../s3client';

function pageXml(folders: string[], next?: string): string {
  return `<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">`
    + `<Name>releases</Name><Prefix></Prefix><KeyCount>${folders.length}</KeyCount><MaxKeys>1000</MaxKeys>`
    + `<Delimiter>/</Delimiter><IsTruncated>${next ? 'true' : 'false'}</IsTruncated>`
    + (next ? `<NextContinuationToken>${next}</NextContinuationToken>` : '')
    + folders.map((f) => `<CommonPrefixes><Prefix>${f}</Prefix></CommonPrefixes>`).join('')
    + `</ListBucketResult>`;
}

let http: ReturnType<typeof mockFetch>;

beforeEach(async () => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://proxy.test', region: 'us-east-1', access_key_id: 'ak', secret_access_key: 'sk', bucket: 'releases',
  }));
  const pages: Record<string, string> = {
    '': pageXml(['a/', 'b/'], 'p2'),
    p2: pageXml(['c/'], 'p3'),
    p3: pageXml(['d/']),
  };
  http.on('GET', /^\/releases\/?\?/, (req) => {
    const token = new URLSearchParams(req.path.split('?')[1]).get('continuation-token') ?? '';
    return new Response(pages[token], { status: 200, headers: { 'content-type': 'application/xml' } });
  });
  await initFromSession();
});

test('listCommonPrefixes reads every page', async () => {
  expect(await listCommonPrefixes('releases')).toEqual(['a/', 'b/', 'c/', 'd/']);
  const lists = http.calls.filter((c) => c.method === 'GET' && /^\/releases\/?\?/.test(c.path));
  expect(lists).toHaveLength(3);
});
