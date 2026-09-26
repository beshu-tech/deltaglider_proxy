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

