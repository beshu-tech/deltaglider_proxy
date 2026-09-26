// Browser-review item 23: every confirmation goes through confirmDialog.
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { expect, test } from 'vitest';

async function sources(dir: string): Promise<string[]> {
  const out: string[] = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    if (e.name === '__tests__') continue;
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...(await sources(p)));
    else if (/\.tsx?$/.test(e.name)) out.push(p);
  }
  return out;
}

test('no window.confirm anywhere in src', async () => {
  const root = new URL('../', import.meta.url).pathname;
  const hits: string[] = [];
  for (const f of await sources(root)) {
    (await readFile(f, 'utf8')).split('\n').forEach((l, i) => {
      if (/\bwindow\.confirm\s*\(/.test(l) && !l.trim().startsWith('//') && !l.trim().startsWith('*')) hits.push(`${f.replace(root, 'src/')}:${i + 1}`);
    });
  }
  expect(hits).toEqual([]);
});


// Lead follow-up to item 18: an expired session is handled in ONE place,
// fetchWithRelogin (adminApi/core.ts). No other module calls the global
// fetch (it would skip the sign-in prompt), and no module outside the
// session plumbing tests for a 401 by itself.
test('only adminApi/core.ts calls fetch; nobody else checks for 401', async () => {
  const root = new URL('../', import.meta.url).pathname;
  const RAW_FETCH = /(^|[^\w.])fetch\s*\(/;
  const OWN_401 = /\bstatus\s*[!=]==?\s*401\b|\b401\s*[!=]==?\s*\w*\.?status\b/;
  const SESSION_PLUMBING = ['src/adminApi/core.ts', 'src/errorHandling.ts'];
  const hits: string[] = [];
  for (const f of await sources(root)) {
    const rel = f.replace(root, 'src/');
    (await readFile(f, 'utf8')).split('\n').forEach((l, i) => {
      const t = l.trim();
      if (t.startsWith('//') || t.startsWith('*')) return;
      if (RAW_FETCH.test(l) && rel !== 'src/adminApi/core.ts') hits.push(`${rel}:${i + 1} raw fetch`);
      if (OWN_401.test(l) && !SESSION_PLUMBING.includes(rel)) hits.push(`${rel}:${i + 1} own 401 check`);
    });
  }
  expect(hits).toEqual([]);
});
