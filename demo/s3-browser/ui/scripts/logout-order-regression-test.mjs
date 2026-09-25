import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

// Sign-out ends the server session (`adminLogout`). Any admin request after
// that is answered 401, which the browser logs as a failed resource. The QA
// pass saw exactly that: `disconnect()` sent DELETE /session/s3-credentials
// after the logout. `disconnect()` must stay local-only.

const src = (p) => readFile(new URL(p, import.meta.url), 'utf8');

/** Body of `export function <name>(...) { ... }` (brace-matched). */
function functionBody(text, name) {
  const start = text.indexOf(`export function ${name}(`);
  assert.ok(start >= 0, `${name} not found`);
  const open = text.indexOf('{', start);
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === '{') depth++;
    if (text[i] === '}' && --depth === 0) return text.slice(open + 1, i);
  }
  throw new Error(`unbalanced ${name}`);
}

const disconnect = functionBody(await src('../src/s3client.ts'), 'disconnect');
for (const call of ['clearSessionCredentials(', 'adminFetch(', 'fetch(', 'adminRequest(']) {
  assert.ok(!disconnect.includes(call), `disconnect() must not send requests (found ${call})`);
}

// handleLogout: the server-side credential clear happens BEFORE the logout.
const app = await src('../src/App.tsx');
const h = app.slice(app.indexOf('const handleLogout = async'));
const body = h.slice(0, h.indexOf('\n  };'));
const clear = body.indexOf('clearSessionCredentials()');
const logout = body.indexOf('adminLogout()');
assert.ok(clear >= 0 && logout >= 0 && clear < logout, 'clear S3 creds, then log out');

console.log('logout order regression checks passed');
