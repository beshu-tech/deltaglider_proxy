// A refused config write answers 400/422 WITH its result body (`ok: false`,
// error, warnings, diff). The client returns that body to the caller's
// `ok: false` UI instead of throwing it away as a generic error.
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import {
  ApiError,
} from '../errorHandling';
import {
  applyConfigYaml,
  changeAdminPassword,
  putSection,
  recoverDb,
  validateConfigYaml,
  validateSection,
} from '../adminApi';
import { restoreSummaryLines } from '../hooks/useBackupImportExport';
import { json, mockFetch } from '../test/fetchMock';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

const refusal = {
  ok: false,
  error: 'buckets.releases: unknown backend hetzner-fsn2',
  warnings: ['a new warning'],
  existing_warnings: ['an old warning'],
  diff: { storage: { 'buckets.releases.backend': { before: 'hetzner-fsn1', after: 'hetzner-fsn2' } } },
};

test('a 422 section validate refusal reaches the caller with its warnings and diff', async () => {
  http.on('POST', '/_/api/admin/config/section/storage/validate', json(refusal, 422));
  await expect(validateSection('storage', {})).resolves.toEqual(refusal);
});

test('a 400 section PUT refusal reaches the caller', async () => {
  http.on('PUT', '/_/api/admin/config/section/storage', json(refusal, 400));
  await expect(putSection('storage', {})).resolves.toMatchObject({ ok: false, error: refusal.error });
});

test('a 400 document validate and apply refusal reach the caller', async () => {
  http.on('POST', '/_/api/admin/config/validate', json({ ok: false, error: 'bad yaml', warnings: ['w'] }, 400));
  await expect(validateConfigYaml('x: [')).resolves.toEqual({ ok: false, error: 'bad yaml', warnings: ['w'] });
  http.on('POST', '/_/api/admin/config/apply', json({ applied: false, persisted: false, requires_restart: false, warnings: [], error: 'refused' }, 422));
  await expect(applyConfigYaml('x: 1')).resolves.toMatchObject({ applied: false, error: 'refused' });
});

test('a failure without a result body still throws', async () => {
  http.on('POST', '/_/api/admin/config/section/storage/validate', json({ error: 'internal' }, 500));
  await expect(validateSection('storage', {})).rejects.toBeInstanceOf(ApiError);
});

test('a refused password change returns the server sentence', async () => {
  http.on('PUT', '/_/api/admin/password', json({ ok: false, error: 'Current password is incorrect' }, 403));
  await expect(changeAdminPassword('a', 'b')).resolves.toEqual({ ok: false, error: 'Current password is incorrect' });
});

test('a wrong recovery key returns the server sentence, without a status prefix', async () => {
  http.on('POST', '/_/api/admin/recover-db', json({ success: false, error: 'The key does not open the encrypted database' }, 401));
  await expect(recoverDb('k')).resolves.toEqual({ success: false, error: 'The key does not open the encrypted database' });
});

test('the restore summary names renamed users and contained rules', () => {
  const base = { users_created: 2, users_skipped: 0, groups_created: 1, groups_skipped: 0, memberships_created: 1 };
  expect(restoreSummaryLines(base)).toHaveLength(2);
  const lines = restoreSummaryLines({
    ...base,
    users_renamed: ['dana -> dana-2'],
    contained_rules: ['user dana: rule 1'],
  });
  expect(lines.join('\n')).toContain('dana -> dana-2');
  expect(lines.join('\n')).toContain('user dana: rule 1');
});
