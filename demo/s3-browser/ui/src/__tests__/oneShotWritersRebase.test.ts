/**
 * F47 class guard (epic #146): a panel that stages edits in `useSectionEditor`
 * holds the section's version (ETag). A one-shot config write from the same
 * panel (it does not go through the editor's Apply) moves that version on the
 * server, so the panel must call `rebase()` after it, or its next Apply gets
 * a false 409 ("changed in another tab").
 */
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

/** Functions that write config outside a section editor's own Apply. */
const ONE_SHOT_WRITERS = ['applySection', 'putSection', 'removeBootstrapCredentials', 'applyConfigYaml', 'updateAdminConfig'];

function components(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? components(path) : /\.tsx$/.test(name) ? [path] : [];
  });
}

describe('a panel with a section editor rebases after its one-shot config writes', () => {
  const root = join(__dirname, '..', 'components');
  const editors = components(root)
    // Comments do not count: a commented-out `rebase()` is no rebase.
    .map((file) => ({ file, text: readFileSync(file, 'utf8').replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$|\s\/\/.*$/gm, '') }))
    .filter(({ text }) => /\buseSectionEditor\s*[<(]/.test(text));

  test('the guard sees the panels that stage section edits', () => {
    expect(editors.length).toBeGreaterThanOrEqual(5);
  });

  test.each(editors.map(({ file, text }) => [file.slice(root.length + 1), text] as const))('%s', (_name, text) => {
    const writers = ONE_SHOT_WRITERS.filter((fn) => new RegExp(`\\b${fn}\\s*\\(`).test(text));
    if (writers.length === 0) return;
    expect(text, `calls ${writers.join(', ')} but never rebase()`).toMatch(/\brebase\s*\(/);
  });
});
