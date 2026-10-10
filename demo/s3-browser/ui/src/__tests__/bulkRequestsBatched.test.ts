// Cockroach scan 2026-10-10: bulk copy and move sent the whole selection as
// ONE request (no progress, no cancel, a loop of hours inside one HTTP
// request). Every bulk object request now goes through useS3Browser's bulk
// runner, which sends batches (useS3Browser.bulk*.test.tsx prove it). This
// guard keeps a new caller from sending a whole selection again.
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { expect, test } from 'vitest';

async function sources(dir: string): Promise<string[]> {
  const out: string[] = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    if (e.name === '__tests__' || e.name === 'test') continue;
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...(await sources(p)));
    else if (/\.tsx?$/.test(e.name)) out.push(p);
  }
  return out;
}

test('only the bulk runner in useS3Browser.ts sends bulk copy, move and delete requests', async () => {
  const root = new URL('../', import.meta.url).pathname;
  const BULK_REQUEST = /\bbulk(Copy|Move|Delete)Objects\b/;
  const ALLOWED = ['src/adminApi/bulkObjects.ts', 'src/useS3Browser.ts'];
  const hits: string[] = [];
  for (const f of await sources(root)) {
    const rel = f.replace(root, 'src/');
    if (ALLOWED.includes(rel)) continue;
    (await readFile(f, 'utf8')).split('\n').forEach((l, i) => {
      const t = l.trim();
      if (t.startsWith('//') || t.startsWith('*')) return;
      if (BULK_REQUEST.test(l)) hits.push(`${rel}:${i + 1}`);
    });
  }
  expect(hits).toEqual([]);
  // In the hook, the requests are made only inside the batch senders.
  const hook = await readFile(join(root, 'useS3Browser.ts'), 'utf8');
  expect(hook).toMatch(/deleteInBatches\(\s*[\s\S]{0,200}bulkDeleteObjects\(/);
  expect(hook).toMatch(/const request = action === 'copy' \? bulkCopyObjects : bulkMoveObjects;[\s\S]{0,1500}transferInBatches\(/);
});
