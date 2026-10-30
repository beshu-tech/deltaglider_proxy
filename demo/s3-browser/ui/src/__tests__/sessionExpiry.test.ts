import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { test } from 'vitest';
import { throwApiError, isSessionExpired, ApiError } from '../errorHandling';
import * as core from '../adminApi/core';
import * as api from '../adminApi/whoami';

async function thrown(status: number, body: string, contentType = 'application/json'): Promise<unknown> {
  const res = new Response(body, { status, headers: { 'content-type': contentType } });
  try {
    await throwApiError(res, 'Load users');
  } catch (e) {
    return e;
  }
  throw new Error('throwApiError must throw');
}

const body = (code: string, message: string) => JSON.stringify({ error: code, message });

test('the decision rides on the HTTP status and the code, not the message text', async () => {
  assert.equal(isSessionExpired(await thrown(401, body('unauthorized', 'sign in to use the admin API'))), true);
  // A message that merely CONTAINS "401" (a user id, a request id) is not expiry.
  assert.equal(isSessionExpired(await thrown(404, body('not_found', 'user 4015 not found'))), false);
  assert.equal(isSessionExpired(await thrown(503, body('unavailable', 'backend 401 unreachable'))), false);
  // "session" in the text of a non-auth failure must not log the user out.
  assert.equal(isSessionExpired(await thrown(409, body('conflict', 'session store full'))), false);
  // A browser-lift session on an admin route needs an admin sign-in.
  assert.equal(isSessionExpired(await thrown(403, body('admin_session_required', 'sign in as an admin'))), true);
  // A plain permission denial is not expiry, also when its text names the code.
  assert.equal(isSessionExpired(await thrown(403, body('forbidden', 'access denied'))), false);
  assert.equal(isSessionExpired(await thrown(403, body('forbidden', 'admin_session_required'))), false);
  // Non-API errors never count, whatever their text.
  assert.equal(isSessionExpired(new Error('failed (401)')), false);
  assert.equal(isSessionExpired('401'), false);
  assert.equal(isSessionExpired(undefined), false);
});

test('the thrown message keeps its operator-facing shape', async () => {
  const e = await thrown(401, body('unauthorized', 'sign in to use the admin API'));
  assert.ok(e instanceof Error);
  assert.equal(e.message, 'Load users failed (401) [unauthorized]: sign in to use the admin API');
  assert.ok(e instanceof ApiError);
  assert.equal(e.status, 401);
});

test('{error: <code>, message: <text>} keeps the code separate from the message', async () => {
  // `{error: <code>, message: <text>}` (admin input errors, CSRF, declarative
  // IAM): show the message and keep the code as the code. The UI used to show
  // only the bare code, e.g. "invalid_path".
  const shaped = (await thrown(
    400,
    '{"error":"invalid_path","message":"dest_prefix: invalid path \\"../x/\\": \'.\' and \'..\' segments are not allowed"}',
  )) as ApiError;
  assert.equal(shaped.code, 'invalid_path');
  assert.equal(
    shaped.message,
    'Load users failed (400) [invalid_path]: dest_prefix: invalid path "../x/": \'.\' and \'..\' segments are not allowed',
  );
  assert.equal(shaped.detail, 'dest_prefix: invalid path "../x/": \'.\' and \'..\' segments are not allowed');
  // A lone `error` (the config write surfaces: `{ok: false, error}`) is the text.
  const refusal = (await thrown(409, '{"ok":false,"error":"the section changed"}')) as ApiError;
  assert.equal(refusal.code, undefined);
  assert.equal(refusal.detail, 'the section changed');
  assert.equal(refusal.message, 'Load users failed (409): the section changed');
});

test('a body that is not the admin JSON shape falls back to its text or the status hint', async () => {
  // A reverse proxy in front of the proxy answers in HTML or plain text.
  const html = (await thrown(502, '<html><body>Bad Gateway</body></html>', 'text/html')) as ApiError;
  assert.equal(html.status, 502);
  assert.equal(html.code, undefined);
  assert.match(html.message, /^Load users failed \(502\): <html><body>Bad Gateway<\/body><\/html>$/);
  const empty = (await thrown(504, '', 'text/plain')) as ApiError;
  assert.match(empty.message, /Load users failed \(504\): Gateway timeout \(504\)/);
  const tooLarge = (await thrown(413, '', 'text/html')) as ApiError;
  assert.match(tooLarge.message, /Payload too large/);
  // JSON without a string `error`/`message` keeps its own text.
  const odd = (await thrown(400, '{"error":{"nested":true}}')) as ApiError;
  assert.equal(odd.detail, '{"error":{"nested":true}}');
  // An S3 XML error is never read as JSON (the unit project has no
  // DOMParser, so its text stands in for <Message>).
  const xml = (await thrown(
    403,
    '<?xml version="1.0"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>',
    'application/xml',
  )) as ApiError;
  assert.match(xml.detail ?? '', /Access Denied/);
});

test('source guard: no component classifies expiry on message text', async () => {
  // Application source only — test files legitimately exercise the literal
  // strings this guard forbids in real components (this very test does), so
  // `__tests__`/`test` directories are excluded rather than self-scanned.
  async function* files(dir: URL): AsyncGenerator<URL> {
    for (const d of await readdir(dir, { withFileTypes: true })) {
      if (d.isDirectory() && (d.name === '__tests__' || d.name === 'test')) continue;
      const p = new URL(d.name + (d.isDirectory() ? '/' : ''), dir);
      if (d.isDirectory()) yield* files(p);
      else if (/\.(ts|tsx)$/.test(d.name)) yield p;
    }
  }
  const offenders: string[] = [];
  for await (const f of files(new URL('../', import.meta.url))) {
    const text = await readFile(f, 'utf8');
    text.split('\n').forEach((line, i) => {
      if (/includes\(['"]401['"]\)|\/401\|/.test(line)) offenders.push(`${f.pathname}:${i + 1}: ${line.trim()}`);
    });
  }
  assert.deepEqual(offenders, [], 'use isSessionExpired(e), not message text');
});

function stubFetch(status: number | 'network', body?: unknown): void {
  globalThis.fetch = async () => {
    if (status === 'network') throw new TypeError('Failed to fetch');
    return new Response(body === undefined ? null : JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json' },
    });
  };
}
async function rejection(p: Promise<unknown>): Promise<unknown> {
  try {
    await p;
  } catch (e) {
    return e;
  }
  throw new Error('expected a rejection');
}

test('checkSession: "not valid" only for a real answer or an expired session', async () => {
  stubFetch(200, { valid: true, admin_gui: true });
  assert.deepEqual(await core.checkSession(), { valid: true, admin_gui: true });
  stubFetch(200, { valid: false, admin_gui: false });
  assert.deepEqual(await core.checkSession(), { valid: false, admin_gui: false });
  stubFetch(401, { error: 'unauthorized', message: 'sign in' });
  assert.deepEqual(await core.checkSession(), { valid: false, admin_gui: false });
  stubFetch(403, { error: 'admin_session_required', message: 'sign in as an admin' });
  assert.deepEqual(await core.checkSession(), { valid: false, admin_gui: false });
  // A gateway blip or a dropped connection is NOT "your session expired".
  for (const status of [500, 502, 503, 'network' as const]) {
    stubFetch(status, { error: 'upstream' });
    const e = await rejection(core.checkSession());
    assert.ok(e instanceof ApiError, `checkSession must throw ApiError on ${status}`);
    assert.equal(isSessionExpired(e), false);
  }
});

test('getAdminConfig: a failed load throws (never a null that reads as GUI mode)', async () => {
  stubFetch(200, { iam_mode: 'declarative' });
  assert.equal((await core.getAdminConfig()).iam_mode, 'declarative');
  stubFetch(503, { error: 'backend down' });
  const cfgErr = (await rejection(core.getAdminConfig())) as ApiError;
  assert.ok(cfgErr instanceof ApiError);
  assert.equal(cfgErr.status, 503);
  stubFetch(401, { error: 'unauthorized' });
  assert.equal(isSessionExpired(await rejection(core.getAdminConfig())), true);
});

test('loginAs: 403 means "not an admin"; a rate limit or a server error must surface', async () => {
  stubFetch(200, { ok: true });
  assert.deepEqual(await api.loginAs('AK', 'SK'), { ok: true });
  stubFetch(403);
  const denied = await api.loginAs('AK', 'SK');
  assert.equal(denied.ok, false);
  assert.ok(!denied.ok);
  assert.equal(denied.status, 403);
  assert.equal(api.isNotAdminDenial(denied), true);
  for (const status of [429, 500, 503]) {
    stubFetch(status);
    const r = await api.loginAs('AK', 'SK');
    assert.equal(r.ok, false);
    assert.ok(!r.ok);
    assert.equal(r.status, status);
    assert.equal(api.isNotAdminDenial(r), false, `${status} is not a not-admin answer`);
    // A lockout names itself (loginError.ts); other failures carry the status.
    assert.match(r.error, status === 429 ? /Too many sign-in attempts/ : new RegExp(`\\(${status}\\)`));
  }
});

test('source guard: ONE admin error shape', async () => {
  // Every adminApi module goes through adminJson / adminRequest (core.ts), which
  // throw an ApiError with a status. A module that hand-rolls `fetch(` or
  // `throwApiError(` can drift back to a bare Error or a hard-coded '/_' base.
  const adminApiDir = new URL('../adminApi/', import.meta.url);
  const shapeOffenders: string[] = [];
  for (const d of await readdir(adminApiDir)) {
    if (d === 'core.ts') continue;
    const text = await readFile(new URL(d, adminApiDir), 'utf8');
    text.split('\n').forEach((line, i) => {
      if (/\bthrowApiError\(|(^|[^.\w])fetch\(|['"`]\/_\//.test(line)) {
        shapeOffenders.push(`${d}:${i + 1}: ${line.trim()}`);
      }
    });
  }
  assert.deepEqual(shapeOffenders, [], 'use adminJson/adminRequest from adminApi/core.ts');
});
