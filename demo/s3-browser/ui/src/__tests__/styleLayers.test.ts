/**
 * A style paints; it never lays out. A `[data-theme-style="…"]` rule may set
 * colours, fills, borders, radii, shadows, fonts and role variables. Box
 * geometry (padding, margin, size, position, display, flex, overflow, type
 * size and spacing) lives once in the base rule of a component, or in an AntD
 * token (theme.ts). A style that changes geometry by selector misses every
 * variant it did not foresee (the XP modal rule un-hid the confirm dialog's
 * header and zeroed its padding), and a style that needs `!important` is
 * fighting an inline style that should be a class.
 *
 * STYLE_LAYER_DEBT lists the rules written before this guard. It may only
 * shrink: a new entry fails, and so does an entry that no longer exists.
 */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test } from 'vitest';
import { STYLE_LAYER_DEBT } from './styleLayerDebt';

const GEOMETRY = new RegExp(
  '^(padding|margin|width|height|min-width|min-height|max-width|max-height|gap|row-gap|column-gap|top|right|bottom|left|inset' +
    '|position|display|flex|flex-[a-z]+|grid|grid-[a-z-]+|overflow|overflow-[xy]|font-size|line-height|letter-spacing' +
    '|word-spacing|white-space|box-sizing)(-(top|right|bottom|left|inline|block|inline-start|inline-end|block-start|block-end))?$',
);

/** Pure: every geometry or `!important` declaration in a style-only rule, as `<selector> { <prop>[ !important] }`. */
export function styleLayerViolations(css: string): string[] {
  const out: string[] = [];
  const bare = css.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const m of bare.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const selectors = m[1].split(',').map((s) => s.trim()).filter(Boolean);
    if (selectors.length === 0 || !selectors.every((s) => s.startsWith('[data-theme-style='))) continue;
    const sel = m[1].split(/\s+/).join(' ').trim();
    for (const decl of m[2].split(';')) {
      const i = decl.indexOf(':');
      if (i < 0) continue;
      const prop = decl.slice(0, i).trim();
      if (prop.startsWith('--')) continue;
      if (GEOMETRY.test(prop)) out.push(`${sel} { ${prop} }`);
      if (decl.includes('!important')) out.push(`${sel} { ${prop} !important }`);
    }
  }
  return out;
}

test('a style rule paints: no box geometry, no !important (the debt only shrinks)', () => {
  const found = styleLayerViolations(readFileSync(join(__dirname, '..', 'theme.css'), 'utf8'));
  const debt = new Set(STYLE_LAYER_DEBT);
  expect(found.filter((v) => !debt.has(v)), 'new geometry or !important in a style rule').toEqual([]);
  expect(STYLE_LAYER_DEBT.filter((v) => !found.includes(v)), 'paid debt: delete these entries').toEqual([]);
});

test('the checker sees geometry and !important, and ignores base rules and role variables', () => {
  const css = `
    [data-theme-style="x"] .a { padding: 4px; color: red; --role: 3px; }
    [data-theme-style="x"] .b { border-radius: 0 !important; }
    .base { padding: 4px !important; }
    [data-theme-style="x"] .c, .d { margin: 0; }`;
  expect(styleLayerViolations(css)).toEqual([
    '[data-theme-style="x"] .a { padding }',
    '[data-theme-style="x"] .b { border-radius !important }',
  ]);
});
