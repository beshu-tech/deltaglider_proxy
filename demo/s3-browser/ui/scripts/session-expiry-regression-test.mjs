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

console.log('session-expiry regression checks passed');
