// A findBy / waitFor with a budget of its own below ASYNC_WAIT_MS is the
// flake that a loaded host finds: each one was fixed site by site before
// (157c452c, d9748fa5), until setup.ts gave every wait one budget.
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { expect, test } from 'vitest';
import { ASYNC_WAIT_MS } from '../test/render';

test('no test waits less than the suite-wide budget', async () => {
  const dir = new URL('./', import.meta.url).pathname;
  const hits: string[] = [];
  for (const name of await readdir(dir)) {
    if (!/\.test\.tsx?$/.test(name)) continue;
    (await readFile(join(dir, name), 'utf8')).split('\n').forEach((l, i) => {
      for (const m of l.matchAll(/\btimeout\s*:\s*([\d_]+)/g)) {
        if (Number(m[1].replace(/_/g, '')) < ASYNC_WAIT_MS) hits.push(`${name}:${i + 1}`);
      }
    });
  }
  expect(hits).toEqual([]);
});
