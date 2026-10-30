// deriveBrowserCapabilities: the one decision behind every file-browser
// affordance (upload, drop zone, bulk bar, inspector, Settings, refresh).
import { describe, expect, test } from 'vitest';
import type { IamPermission, WhoamiResponse } from '../adminApi';
import { deriveBrowserCapabilities, type BrowserCapabilityInput } from '../browserCapabilities';
import { deriveSessionCapabilities } from '../sessionCapabilities';

const ADMIN_SESSION = deriveSessionCapabilities({ valid: true, admin_gui: true });
const FILES_SESSION = deriveSessionCapabilities({ valid: true, admin_gui: false });
const NO_SESSION = deriveSessionCapabilities({ valid: false, admin_gui: false });

const iam = (rules: Omit<IamPermission, 'id'>[], isAdmin = false): WhoamiResponse => ({
  mode: 'iam',
  user: { name: 'dana', access_key_id: 'AK', is_admin: isAdmin, permissions: rules.map((r, id) => ({ id, ...r })) },
});
const BOOTSTRAP: WhoamiResponse = { mode: 'bootstrap', user: { name: 'admin', access_key_id: 'AK', is_admin: true } };
const OPEN: WhoamiResponse = { mode: 'open', user: null };
const SSO_NON_ADMIN = iam([]);
const READER = iam([{ actions: ['read', 'list'], resources: ['releases/*'] }]);
// Writes only under releases/ci/, deletes nothing.
const CI_UPLOADER = iam([
  { actions: ['read', 'list'], resources: ['releases/*'] },
  { actions: ['write'], resources: ['releases/ci/*'] },
]);
const DELETER = iam([{ actions: ['read', 'list', 'write', 'delete'], resources: ['releases/*'] }]);
const DENY_DELETE_ARCHIVE = iam([
  { actions: ['read', 'list', 'write', 'delete'], resources: ['releases/*'] },
  { effect: 'Deny', actions: ['delete'], resources: ['releases/archive/*'] },
]);

function input(over: Partial<BrowserCapabilityInput>): BrowserCapabilityInput {
  return {
    identity: null,
    session: FILES_SESSION,
    bucket: 'releases',
    prefix: '',
    bucketBusy: false,
    writablePrefixes: [],
    selectedKeys: [],
    inspectedKey: null,
    ...over,
  };
}

describe('canAdmin and canCreateBucket', () => {
  test.each([
    ['no identity', null, false, false],
    ['bootstrap', BOOTSTRAP, true, true],
    ['open access', OPEN, true, true],
    ['IAM admin', iam([], true), true, true],
    ['SSO user without admin', SSO_NON_ADMIN, false, false],
    ['reader', READER, false, false],
  ])('%s', (_name, identity, canAdmin, canCreateBucket) => {
    const caps = deriveBrowserCapabilities(input({ identity }));
    expect(caps.canAdmin).toBe(canAdmin);
    expect(caps.canCreateBucket).toBe(canCreateBucket);
  });
});

describe('writing into the current folder', () => {
  test.each([
    // identity, prefix, writablePrefixes, busy, canWriteFolder, canUpload
    ['reader at root', READER, '', [], false, false, false],
    ['ci-uploader at root, with a writable prefix', CI_UPLOADER, '', ['ci/'], false, false, true],
    ['ci-uploader inside ci/', CI_UPLOADER, 'ci/', ['ci/'], false, true, true],
    ['deleter at root', DELETER, '', [''], false, true, true],
    ['deleter, bucket write gate closed', DELETER, '', [''], true, false, false],
  ] as const)('%s', (_name, identity, prefix, writablePrefixes, bucketBusy, canWriteFolder, canUpload) => {
    const caps = deriveBrowserCapabilities(
      input({ identity, prefix, writablePrefixes: [...writablePrefixes], bucketBusy }),
    );
    expect(caps.canWriteFolder).toBe(canWriteFolder);
    expect(caps.canUpload).toBe(canUpload);
  });

  test('no bucket yet: nothing to write into, but refresh stays on', () => {
    const caps = deriveBrowserCapabilities(input({ identity: BOOTSTRAP, bucket: '', writablePrefixes: [''] }));
    expect(caps.canWriteFolder).toBe(false);
    expect(caps.canUpload).toBe(false);
    expect(caps.canReadFolder).toBe(true);
  });
});

describe('canReadFolder (refresh)', () => {
  test('read or list on the folder', () => {
    expect(deriveBrowserCapabilities(input({ identity: READER })).canReadFolder).toBe(true);
    const listOnly = iam([{ actions: ['list'], resources: ['releases'] }]);
    expect(deriveBrowserCapabilities(input({ identity: listOnly })).canReadFolder).toBe(true);
  });
  test('no grant on the bucket', () => {
    const other = iam([{ actions: ['read', 'list'], resources: ['downloads/*'] }]);
    expect(deriveBrowserCapabilities(input({ identity: other })).canReadFolder).toBe(false);
  });
});

describe('bulk actions on the selection', () => {
  const SELECTION = ['a.zip', 'folder:archive/'];

  test.each([
    // identity, session, prefix, busy, delete, copy, move, zip
    ['nothing selected', DELETER, FILES_SESSION, '', false, [], false, false, false, false],
    ['reader: zip only', READER, FILES_SESSION, '', false, SELECTION, false, false, false, true],
    ['deleter: everything', DELETER, FILES_SESSION, '', false, SELECTION, true, true, true, true],
    ['deleter, bucket busy: zip only', DELETER, FILES_SESSION, '', true, SELECTION, false, false, false, true],
    ['deleter, no session: nothing', DELETER, NO_SESSION, '', false, SELECTION, false, false, false, false],
    ['deny on archive/: no delete, no move', DENY_DELETE_ARCHIVE, ADMIN_SESSION, '', false, SELECTION, false, true, false, true],
    // Copy writes into the current folder: ci-uploader may copy only from inside ci/.
    ['ci-uploader at root: zip only', CI_UPLOADER, FILES_SESSION, '', false, SELECTION, false, false, false, true],
    ['ci-uploader in ci/: copy and zip', CI_UPLOADER, FILES_SESSION, 'ci/', false, SELECTION, false, true, false, true],
  ] as const)('%s', (_name, identity, session, prefix, bucketBusy, selectedKeys, del, copy, move, zip) => {
    const caps = deriveBrowserCapabilities(input({ identity, session, prefix, bucketBusy, selectedKeys }));
    expect(caps.bulk).toEqual({ delete: del, copy, move, zip });
  });

  test('a folder key is checked as its prefix', () => {
    const archiveOnly = iam([{ actions: ['read'], resources: ['releases/archive/*'] }]);
    expect(deriveBrowserCapabilities(input({ identity: archiveOnly, selectedKeys: ['folder:archive/'] })).bulk.zip).toBe(true);
    expect(deriveBrowserCapabilities(input({ identity: archiveOnly, selectedKeys: ['folder:builds/'] })).bulk.zip).toBe(false);
  });

  test('no bucket: no bulk action', () => {
    const caps = deriveBrowserCapabilities(input({ identity: BOOTSTRAP, session: ADMIN_SESSION, bucket: '', selectedKeys: SELECTION }));
    expect(caps.bulk).toEqual({ delete: false, copy: false, move: false, zip: false });
  });
});

describe('the inspected object', () => {
  test.each([
    ['nothing inspected', DELETER, null, false, false],
    ['reader', READER, 'a.zip', true, false],
    ['deleter', DELETER, 'a.zip', true, true],
    ['deny on archive/', DENY_DELETE_ARCHIVE, 'archive/old.zip', true, false],
  ] as const)('%s', (_name, identity, inspectedKey, read, del) => {
    const caps = deriveBrowserCapabilities(input({ identity, inspectedKey }));
    expect(caps.canReadInspected).toBe(read);
    expect(caps.canDeleteInspected).toBe(del);
  });

  test('a busy bucket still lets the inspector delete (the server refuses it)', () => {
    // App never gated the inspector on the write gate; this pins that.
    const caps = deriveBrowserCapabilities(input({ identity: DELETER, inspectedKey: 'a.zip', bucketBusy: true }));
    expect(caps.canDeleteInspected).toBe(true);
  });
});
