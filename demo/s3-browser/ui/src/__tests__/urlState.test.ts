/** src/urlState.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  parseViewLocation,
  parseBrowserLocation,
  buildBrowserUrl,
  buildViewUrl,
  parseAdminQuery,
  BASE,
  isAdminPageLeave,
  type BrowserLocation,
} from '../urlState';

test('parseViewLocation', () => {
  assert.deepEqual(parseViewLocation('/_/'), { view: 'browser', subPath: '' });
  assert.deepEqual(parseViewLocation('/_/browse'), { view: 'browser', subPath: '' });
  assert.deepEqual(parseViewLocation('/_/admin/configuration/access/credentials'), {
    view: 'admin', subPath: 'configuration/access/credentials',
  });
  assert.deepEqual(parseViewLocation('/_/docs/configuration'), { view: 'docs', subPath: 'configuration' });
  // /_/metrics belongs to the Prometheus endpoint; the SPA has no view there.
  assert.deepEqual(parseViewLocation('/_/metrics'), { view: 'browser', subPath: '' });
  assert.deepEqual(parseViewLocation('/_/unknownthing'), { view: 'browser', subPath: '' });
});

test('parseBrowserLocation', () => {
  assert.deepEqual(parseBrowserLocation('/_/', ''), { bucket: '', prefix: '', q: '', object: '', preview: '' });
  assert.deepEqual(parseBrowserLocation('/_/browse/beshu/', ''), { bucket: 'beshu', prefix: '', q: '', object: '', preview: '' });
  assert.deepEqual(parseBrowserLocation('/_/browse/beshu/ror/builds/', ''), {
    bucket: 'beshu', prefix: 'ror/builds/', q: '', object: '', preview: '',
  });
  // query params
  assert.deepEqual(parseBrowserLocation('/_/browse/beshu/ror/', '?q=zip'), {
    bucket: 'beshu', prefix: 'ror/', q: 'zip', object: '', preview: '',
  });
  assert.deepEqual(parseBrowserLocation('/_/browse/beshu/ror/', '?object=ror/app.zip'), {
    bucket: 'beshu', prefix: 'ror/', q: '', object: 'ror/app.zip', preview: '',
  });
  // preview flag
  assert.deepEqual(parseBrowserLocation('/_/browse/beshu/ror/', '?object=ror/app.zip&preview=1'), {
    bucket: 'beshu', prefix: 'ror/', q: '', object: 'ror/app.zip', preview: '1',
  });
});

test('buildBrowserUrl', () => {
  assert.equal(buildBrowserUrl({ bucket: '', prefix: '' }), '/_/browse');
  assert.equal(buildBrowserUrl({ bucket: 'beshu' }), '/_/browse/beshu/');
  assert.equal(buildBrowserUrl({ bucket: 'beshu', prefix: 'ror/builds/' }), '/_/browse/beshu/ror/builds/');
  assert.equal(buildBrowserUrl({ bucket: 'beshu', prefix: 'ror/', q: 'zip' }), '/_/browse/beshu/ror/?q=zip');
  assert.equal(
    buildBrowserUrl({ bucket: 'beshu', prefix: 'ror/', object: 'ror/app.zip' }),
    '/_/browse/beshu/ror/?object=ror%2Fapp.zip',
  );
  // preview flag: ?preview=1 is added alongside ?object=
  assert.equal(
    buildBrowserUrl({ bucket: 'beshu', prefix: 'ror/', object: 'ror/app.zip', preview: '1' }),
    '/_/browse/beshu/ror/?object=ror%2Fapp.zip&preview=1',
  );
});

test('buildViewUrl', () => {
  assert.equal(buildViewUrl('admin', 'configuration/access/credentials'), '/_/admin/configuration/access/credentials');
  assert.equal(buildViewUrl('browser'), '/_/browse');
  assert.equal(buildViewUrl('docs', '/configuration/'), '/_/docs/configuration');
  // buildViewUrl with query params (deep-linking)
  assert.equal(buildViewUrl('admin', 'jobs', { job: 'replication:foo' }), '/_/admin/jobs?job=replication%3Afoo');
  assert.equal(buildViewUrl('admin', 'jobs', { job: 'replication:foo', tab: 'runs' }), '/_/admin/jobs?job=replication%3Afoo&tab=runs');
  assert.equal(buildViewUrl('admin', 'jobs', { job: 'replication:foo', tab: 'definition' }), '/_/admin/jobs?job=replication%3Afoo&tab=definition');
  assert.equal(buildViewUrl('admin', 'jobs'), '/_/admin/jobs');
  assert.equal(buildViewUrl('admin', 'jobs', {}), '/_/admin/jobs');
});

test('parseAdminQuery', () => {
  assert.deepEqual(parseAdminQuery('?job=replication%3Afoo&tab=runs'), { job: 'replication:foo', tab: 'runs' });
  assert.deepEqual(parseAdminQuery('job=replication%3Afoo'), { job: 'replication:foo' });
  assert.deepEqual(parseAdminQuery(''), {});
  assert.deepEqual(parseAdminQuery('?'), {});
});

test('malformed percent-escapes never throw', () => {
  // A stray `%` (a hand-typed URL, a truncated paste) made decodeURIComponent
  // throw a URIError during render, which blanked the whole app. Such a segment
  // falls back to its raw text.
  assert.deepEqual(parseBrowserLocation('/_/browse/beshu/100%/', ''), {
    bucket: 'beshu', prefix: '100%/', q: '', object: '', preview: '',
  });
  assert.deepEqual(parseBrowserLocation('/_/browse/bad%zzname/', ''), {
    bucket: 'bad%zzname', prefix: '', q: '', object: '', preview: '',
  });
  assert.deepEqual(parseBrowserLocation('/_/browse/beshu/ok%20dir/50%off/', ''), {
    bucket: 'beshu', prefix: 'ok dir/50%off/', q: '', object: '', preview: '',
  });
  // A lone surrogate escape is also a URIError.
  assert.deepEqual(parseBrowserLocation('/_/browse/beshu/%E0%A4%A/', ''), {
    bucket: 'beshu', prefix: '%E0%A4%A/', q: '', object: '', preview: '',
  });
});

test('isAdminPageLeave: when unsaved admin edits need a confirm', () => {
  // Only mounted panels hold dirty state, so leaving the admin PAGE (another
  // leaf, or out of Settings) unmounts them and drops the edits. A query-only
  // change on the same page (?user=, ?group=, ?modal=) keeps the panel mounted.
  assert.equal(isAdminPageLeave('/_/admin/access/users', '/_/admin/access/groups'), true);
  assert.equal(isAdminPageLeave('/_/admin/storage/buckets', '/_/browse/'), true);
  assert.equal(isAdminPageLeave('/_/admin/jobs', '/_/docs/configuration'), true);
  assert.equal(isAdminPageLeave('/_/admin/access/users', '/_/admin/access/users?user=4'), false);
  assert.equal(isAdminPageLeave('/_/admin/access/users?user=4', '/_/admin/access/users?user=7'), false);
  assert.equal(isAdminPageLeave('/_/admin/access/groups?group=2', '/_/admin/access/groups'), false);
  assert.equal(isAdminPageLeave('/_/admin/system', '/_/admin/system?modal=yaml'), false);
  assert.equal(isAdminPageLeave('/_/admin/system/', '/_/admin/system'), false);
  // Not on an admin page: nothing to lose.
  assert.equal(isAdminPageLeave('/_/browse/b/', '/_/admin/system'), false);
  assert.equal(isAdminPageLeave('/_/docs/x', '/_/browse/'), false);
});

test('round trip: parse(build(x)) === x (the core invariant)', () => {
  const cases: BrowserLocation[] = [
    { bucket: '', prefix: '', q: '', object: '', preview: '' },
    { bucket: 'beshu', prefix: '', q: '', object: '', preview: '' },
    { bucket: 'beshu', prefix: 'ror/', q: '', object: '', preview: '' },
    { bucket: 'beshu', prefix: 'ror/builds/1.70.0-pre6/', q: '', object: '', preview: '' },
    { bucket: 'beshu', prefix: 'ror/', q: 'sha512', object: '', preview: '' },
    { bucket: 'beshu', prefix: 'ror/', q: '', object: 'ror/readonlyrest-1.70.0_es7.8.1.zip.sha512', preview: '' },
    // nasty keys: spaces, plus, unicode
    { bucket: 'my-bucket', prefix: 'folder with spaces/sub+dir/', q: '', object: '', preview: '' },
    { bucket: 'b', prefix: 'café/数据/', q: 'a+b c', object: 'café/数据/x.txt', preview: '' },
    // preview flag round-trips with object
    { bucket: 'beshu', prefix: 'ror/', q: '', object: 'ror/app.zip', preview: '1' },
  ];
  for (const c of cases) {
    const url = buildBrowserUrl(c);
    const [path, search = ''] = url.split('?');
    const parsed = parseBrowserLocation(path, search);
    assert.deepEqual(parsed, c, `round-trip failed for ${JSON.stringify(c)} -> ${url} -> ${JSON.stringify(parsed)}`);
  }
});

test('BASE export sanity', () => {
  assert.equal(BASE, '/_/');
});
