// designSystem.test.ts — source guards for the site's visual system
// (docs/plan/site-design-audit-2026-09.md). They fail when a colour or an
// emoji comes back at one call site instead of through the system.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { describe, expect, it } from 'vitest';

const SRC = join(__dirname, '..');

function files(dir: string, exts: string[]): string[] {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) return files(p, exts);
    return exts.some((e) => p.endsWith(e)) ? [p] : [];
  });
}

/** CSS text of a file: the whole .css file, or the <style> blocks of a
 *  component. Comments are removed. */
function cssOf(path: string): string {
  const text = readFileSync(path, 'utf8');
  const css = path.endsWith('.css')
    ? text
    : [...text.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map((m) => m[1]).join('\n');
  return css.replace(/\/\*[\s\S]*?\*\//g, '');
}

const COLOUR_LITERAL = /#[0-9a-fA-F]{3,8}\b|rgba?\(/;

describe('design system source guards', () => {
  it('spells colours out only in theme.css', () => {
    const offenders: string[] = [];
    for (const f of files(SRC, ['.css', '.astro'])) {
      if (f.endsWith(join('styles', 'theme.css'))) continue;
      cssOf(f)
        .split('\n')
        .forEach((line, i) => {
          if (COLOUR_LITERAL.test(line)) offenders.push(`${relative(SRC, f)}:${i + 1}: ${line.trim()}`);
        });
    }
    expect(offenders, 'use a token from styles/theme.css').toEqual([]);
  });

  it('uses no emoji in page or component markup', () => {
    // Emoji presentation, or a pictograph forced to emoji with VS16. Plain
    // text symbols (©, ↔, ✓) are fine.
    const emoji = /\p{Emoji_Presentation}|\p{Extended_Pictographic}\uFE0F/u;
    const offenders = files(SRC, ['.astro', '.tsx'])
      .filter((f) => emoji.test(readFileSync(f, 'utf8')))
      .map((f) => relative(SRC, f));
    expect(offenders, 'use a Glyph instead of an emoji').toEqual([]);
  });
});
