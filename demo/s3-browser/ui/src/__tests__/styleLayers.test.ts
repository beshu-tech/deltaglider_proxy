/**
 * A style paints; it never lays out. A `[data-theme-style="…"]` rule may set
 * colours, fills, borders, radii, shadows, fonts and role variables. Box
 * geometry (padding, margin, size, position, display, flex, overflow, type
 * size, line height, wrapping) lives once in the base rule of a component, or in an
 * AntD token (theme.ts). Tracking (letter- and word-spacing) belongs to the face, so
 * a style may set it; it may also hide a decoration (`display: none`). A style that changes geometry by selector misses every
 * variant it did not foresee (the XP modal rule un-hid the confirm dialog's
 * header and zeroed its padding), and a style that needs `!important` is
 * fighting an inline style that should be a class.
 *
 * EXCEPTIONS names the few rules that must break this, each with its reason;
 * an exception that no longer exists fails too.
 */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test } from 'vitest';

const GEOMETRY = new RegExp(
  '^(padding|margin|width|height|min-width|min-height|max-width|max-height|gap|row-gap|column-gap|top|right|bottom|left|inset' +
    '|position|display|flex|flex-[a-z]+|grid|grid-[a-z-]+|overflow|overflow-[xy]|font-size|line-height' +
    '|white-space|box-sizing)(-(top|right|bottom|left|inline|block|inline-start|inline-end|block-start|block-end))?$',
);

/** Pure: a selector list split at its top-level commas (not the ones inside `:is(…)`). */
function splitTopLevel(list: string): string[] {
  const parts: string[] = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < list.length; i++) {
    const c = list[i];
    if (c === '(') depth++;
    else if (c === ')') depth--;
    else if (c === ',' && depth === 0) {
      parts.push(list.slice(start, i));
      start = i + 1;
    }
  }
  parts.push(list.slice(start));
  return parts;
}

/** Pure: every geometry or `!important` declaration in a style-only rule, as `<selector> { <prop>[ !important] }`. */
export function styleLayerViolations(css: string): string[] {
  const out: string[] = [];
  const bare = css.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const m of bare.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const selectors = splitTopLevel(m[1]).map((s) => s.trim()).filter(Boolean);
    if (selectors.length === 0 || !selectors.every((s) => s.startsWith('[data-theme-style='))) continue;
    const sel = m[1].split(/\s+/).join(' ').trim();
    for (const decl of m[2].split(';')) {
      const i = decl.indexOf(':');
      if (i < 0) continue;
      const prop = decl.slice(0, i).trim();
      if (prop.startsWith('--')) continue;
      const hides = prop === 'display' && decl.slice(i + 1).trim().startsWith('none');
      if (GEOMETRY.test(prop) && !hides) out.push(`${sel} { ${prop} }`);
      if (decl.includes('!important')) out.push(`${sel} { ${prop} !important }`);
    }
  }
  return out;
}

const EXCEPTIONS: Record<string, string> = {
  // The cells of a table carry inline text colours (sizes, savings, muted keys); only !important
  // turns them white on the Luna selection fill. A base role cannot: its default would recolour them.
  "[data-theme-style=\"xp\"] .ant-table-wrapper :is(.ant-table-tbody > tr, .ant-table-tbody-virtual .ant-table-row):is(.ant-table-row-selected, [aria-selected='true']) > :is(td, .ant-table-cell) :not(.ant-tag, .ant-tag *, .ant-btn, .ant-btn *, .ant-checkbox, .ant-checkbox *) { color !important }":
    'selected-row ink over inline cell colours',
};

test('a style rule paints: no box geometry, no !important', () => {
  const found = styleLayerViolations(readFileSync(join(__dirname, '..', 'theme.css'), 'utf8'));
  expect(found.filter((v) => !(v in EXCEPTIONS)), 'geometry or !important in a style rule').toEqual([]);
  expect(Object.keys(EXCEPTIONS).filter((v) => !found.includes(v)), 'stale exception').toEqual([]);
});

test('the checker sees geometry and !important, and ignores base rules and role variables', () => {
  const css = `
    [data-theme-style="x"] .a { padding: 4px; color: red; --role: 3px; }
    [data-theme-style="x"] .b { border-radius: 0 !important; }
    .base { padding: 4px !important; }
    [data-theme-style="x"] .c, .d { margin: 0; }
    [data-theme-style="x"] .e { display: none; letter-spacing: 0; }
    [data-theme-style="x"] :is(.f, .g) { height: 1px; }`;
  expect(styleLayerViolations(css)).toEqual([
    '[data-theme-style="x"] .a { padding }',
    '[data-theme-style="x"] .b { border-radius !important }',
    '[data-theme-style="x"] :is(.f, .g) { height }',
  ]);
});
