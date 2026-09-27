// The headless section apply (review4 §3): one PUT with If-Match, the
// sibling-version bus, and the config cache — for every caller, including
// BackendsPanel's one-shot storage PUTs.
import { QueryClient } from '@tanstack/react-query';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { ConfigConflictError } from '../adminApi';
import { applySection, sectionApplyErrorText } from '../applySection';
import { qk } from '../queries/keys';
import { onSectionVersionAdvanced } from '../sectionVersionBus';
import { mockFetch } from '../test/fetchMock';

const SECTION = '/_/api/admin/config/section/storage';
let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

function reply(status: number, body: unknown, etag?: string): Response {
  const headers: Record<string, string> = { 'content-type': 'application/json' };
  if (etag) headers.etag = etag;
  return new Response(JSON.stringify(body), { status, headers });
}

test('a PUT sends If-Match, moves sibling editors and invalidates the config cache', async () => {
  http.on('PUT', SECTION, () => reply(200, { ok: true }, '"v2"'));
  const qc = new QueryClient();
  const invalidate = vi.spyOn(qc, 'invalidateQueries');
  const moves: Array<[string, string]> = [];
  const off = onSectionVersionAdvanced('storage', (from, to) => moves.push([from, to]));
  const resp = await applySection(qc, 'storage', { backends: [] }, '"v1"');
  off();
  expect(resp.ok).toBe(true);
  expect(resp.version).toBe('"v2"');
  expect(http.callsTo('PUT', SECTION)[0].headers['if-match']).toBe('"v1"');
  expect(moves).toEqual([['"v1"', '"v2"']]);
  expect(invalidate).toHaveBeenCalledWith({ queryKey: qk.config() });
});

test('a refused PUT moves nothing', async () => {
  http.on('PUT', SECTION, () => reply(200, { ok: false, error: 'bad key' }, '"v2"'));
  const qc = new QueryClient();
  const invalidate = vi.spyOn(qc, 'invalidateQueries');
  const moves: unknown[] = [];
  const off = onSectionVersionAdvanced('storage', (...a) => moves.push(a));
  const resp = await applySection(qc, 'storage', {}, '"v1"');
  off();
  expect(resp).toMatchObject({ ok: false, error: 'bad key' });
  expect(moves).toEqual([]);
  expect(invalidate).not.toHaveBeenCalled();
});

test('a 409 throws ConfigConflictError, and its text says a retry is safe', async () => {
  http.on('PUT', SECTION, () => reply(409, { error: 'stale' }, '"v3"'));
  const err = await applySection(new QueryClient(), 'storage', {}, '"v1"').catch((e: unknown) => e);
  expect(err).toBeInstanceOf(ConfigConflictError);
  expect(sectionApplyErrorText(err, 'Failed')).toMatch(/changed in another tab or by another admin.*Try again/);
  expect(sectionApplyErrorText(new Error('boom'), 'Failed')).not.toMatch(/another tab/);
});
