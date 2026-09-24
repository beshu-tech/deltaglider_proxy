import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import ts from 'typescript';

// Transpile errorHandling.ts to an importable data: URL (no relative imports).
const sourceUrl = new URL('../src/errorHandling.ts', import.meta.url);
const source = await readFile(sourceUrl, 'utf8');
const transpiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'errorHandling.ts',
}).outputText;
const moduleUrl = `data:text/javascript;base64,${Buffer.from(transpiled).toString('base64')}`;
const { throwApiError, isSessionExpired } = await import(moduleUrl);

async function thrown(status, body, contentType = 'application/json') {
  const res = new Response(body, { status, headers: { 'content-type': contentType } });
  try {
    await throwApiError(res, 'Load users');
  } catch (e) {
    return e;
  }
  throw new Error('throwApiError must throw');
}

// --- The decision rides on the HTTP status, not the message text -----------
assert.equal(isSessionExpired(await thrown(401, '{"error":"unauthorized"}')), true);
// A message that merely CONTAINS "401" (a user id, a request id) is not expiry.
assert.equal(isSessionExpired(await thrown(404, '{"error":"user 4015 not found"}')), false);
assert.equal(isSessionExpired(await thrown(500, '{"error":"backend 401 unreachable"}')), false);
// "session" in the text of a non-auth failure must not log the user out.
assert.equal(isSessionExpired(await thrown(500, '{"error":"session store full"}')), false);
// A browser-lift session on an admin route needs an admin sign-in.
assert.equal(isSessionExpired(await thrown(403, '{"error":"admin_session_required"}')), true);
// A plain permission denial is not expiry.
assert.equal(isSessionExpired(await thrown(403, '{"error":"access denied"}')), false);
// Non-API errors never count, whatever their text.
assert.equal(isSessionExpired(new Error('failed (401)')), false);
assert.equal(isSessionExpired('401'), false);
assert.equal(isSessionExpired(undefined), false);

// The thrown message keeps its operator-facing shape.
const e = await thrown(401, '{"error":"unauthorized"}');
assert.ok(e instanceof Error);
assert.equal(e.message, 'Load users failed (401): unauthorized');
assert.equal(e.status, 401);

// --- Source guard: no component classifies expiry on message text ----------
async function* files(dir) {
  for (const d of await readdir(dir, { withFileTypes: true })) {
    const p = new URL(d.name + (d.isDirectory() ? '/' : ''), dir);
    if (d.isDirectory()) yield* files(p);
    else if (/\.(ts|tsx)$/.test(d.name)) yield p;
  }
}
const offenders = [];
for await (const f of files(new URL('../src/', import.meta.url))) {
  const text = await readFile(f, 'utf8');
  text.split('\n').forEach((line, i) => {
    if (/includes\(['"]401['"]\)|\/401\|/.test(line)) offenders.push(`${f.pathname}:${i + 1}: ${line.trim()}`);
  });
}
assert.deepEqual(offenders, [], 'use isSessionExpired(e), not message text');

// --- The admin API's session/config/login-as calls classify failures -------
// Transpile a module and its relative imports into data: URLs, so the REAL
// checkSession / getAdminConfig / loginAs run here against a stubbed fetch.
const dataUrls = new Map();
async function loadTs(fileUrl) {
  const key = fileUrl.href;
  if (dataUrls.has(key)) return dataUrls.get(key);
  let out = ts.transpileModule(await readFile(fileUrl, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
    fileName: fileUrl.pathname,
  }).outputText;
  for (const [, spec] of [...out.matchAll(/from '(\.{1,2}\/[^']+)'/g)]) {
    const dep = await loadTs(new URL(`${spec}.ts`, fileUrl));
    out = out.replaceAll(`from '${spec}'`, `from '${dep}'`);
  }
  const url = `data:text/javascript;base64,${Buffer.from(out).toString('base64')}`;
  dataUrls.set(key, url);
  return url;
}
const api = await import(await loadTs(new URL('../src/adminApi/whoami.ts', import.meta.url)));
const core = await import(await loadTs(new URL('../src/adminApi/core.ts', import.meta.url)));
const errs = await import(await loadTs(new URL('../src/errorHandling.ts', import.meta.url)));

function stubFetch(status, body) {
  globalThis.fetch = async () => {
    if (status === 'network') throw new TypeError('Failed to fetch');
    return new Response(body === undefined ? null : JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json' },
    });
  };
}
async function rejection(p) {
  try {
    await p;
  } catch (e) {
    return e;
  }
  throw new Error('expected a rejection');
}

// checkSession: "not valid" only for a real answer or an expired session.
stubFetch(200, { valid: true, admin_gui: true });
assert.deepEqual(await core.checkSession(), { valid: true, admin_gui: true });
stubFetch(200, { valid: false, admin_gui: false });
assert.deepEqual(await core.checkSession(), { valid: false, admin_gui: false });
stubFetch(401, { error: 'unauthorized' });
assert.deepEqual(await core.checkSession(), { valid: false, admin_gui: false });
stubFetch(403, { error: 'admin_session_required' });
assert.deepEqual(await core.checkSession(), { valid: false, admin_gui: false });
// A gateway blip or a dropped connection is NOT "your session expired".
for (const status of [500, 502, 503, 'network']) {
  stubFetch(status, { error: 'upstream' });
  const e = await rejection(core.checkSession());
  assert.ok(e instanceof errs.ApiError, `checkSession must throw ApiError on ${status}`);
  assert.equal(errs.isSessionExpired(e), false);
}

// getAdminConfig: a failed load throws (never a null that reads as GUI mode).
stubFetch(200, { iam_mode: 'declarative' });
assert.equal((await core.getAdminConfig()).iam_mode, 'declarative');
stubFetch(503, { error: 'backend down' });
const cfgErr = await rejection(core.getAdminConfig());
assert.ok(cfgErr instanceof errs.ApiError);
assert.equal(cfgErr.status, 503);
stubFetch(401, { error: 'unauthorized' });
assert.equal(errs.isSessionExpired(await rejection(core.getAdminConfig())), true);

// loginAs: 403 means "not an admin" (a files-only session is right); a rate
// limit or a server error must surface, not silently downgrade.
stubFetch(200, { ok: true });
assert.deepEqual(await api.loginAs('AK', 'SK'), { ok: true });
stubFetch(403);
const denied = await api.loginAs('AK', 'SK');
assert.equal(denied.ok, false);
assert.equal(denied.status, 403);
assert.equal(api.isNotAdminDenial(denied), true);
for (const status of [429, 500, 503]) {
  stubFetch(status);
  const r = await api.loginAs('AK', 'SK');
  assert.equal(r.ok, false);
  assert.equal(r.status, status);
  assert.equal(api.isNotAdminDenial(r), false, `${status} is not a not-admin answer`);
  assert.match(r.error, new RegExp(`\\(${status}\\)`));
}

// --- Source guard: ONE admin error shape ------------------------------------
// Every adminApi module goes through adminJson / adminRequest (core.ts), which
// throw an ApiError with a status. A module that hand-rolls `fetch(` or
// `throwApiError(` can drift back to a bare Error or a hard-coded '/_' base.
// bulkObjects.ts is owned by a separate change and is exempt until it migrates.
const adminApiDir = new URL('../src/adminApi/', import.meta.url);
const shapeOffenders = [];
for (const d of await readdir(adminApiDir)) {
  if (d === 'core.ts' || d === 'bulkObjects.ts') continue;
  const text = await readFile(new URL(d, adminApiDir), 'utf8');
  text.split('\n').forEach((line, i) => {
    if (/\bthrowApiError\(|(^|[^.\w])fetch\(|['"`]\/_\//.test(line)) {
      shapeOffenders.push(`${d}:${i + 1}: ${line.trim()}`);
    }
  });
}
assert.deepEqual(shapeOffenders, [], 'use adminJson/adminRequest from adminApi/core.ts');

console.log('session-expiry regression checks passed');
