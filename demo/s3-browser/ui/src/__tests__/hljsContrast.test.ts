/**
 * UX harvest D43: the docs code blocks failed axe colour-contrast in the
 * light theme (YAML keys #e11d48 4.28:1, strings #059669 3.43:1). Every
 * syntax token must reach 4.5:1 on the code-block background, in both
 * themes. Read from theme.css (the roles), resolved through the palette.
 */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test } from 'vitest';
import { DEFAULT_THEME_STYLE, type ColorTokens, type ThemeStyle } from '../ThemeContext';
import { cssVarName } from '../themeVars';
import { ALL_THEMES } from '../test/themes';

const css = readFileSync(join(__dirname, '..', 'theme.css'), 'utf8');

function luminance(hex: string): number {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function ratio(a: string, b: string): number {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
}

/** The custom properties of the first block that starts with `selector {` ({} when there is none). */
function block(selector: string): Record<string, string> {
  const start = css.indexOf(`${selector} {`);
  if (start < 0) return {};
  const body = css.slice(start, css.indexOf('\n}', start));
  return Object.fromEntries([...body.matchAll(/(--[\w-]+):\s*([^;]+);/g)].map((m) => [m[1], m[2].trim()]));
}

/**
 * The resolved hex of every role in one theme, in cascade order: the `:root`
 * roles, the mode block, then the style's blocks; `var(--dg-…)` is looked up
 * in the palette (themeVars.ts).
 */
function resolved(style: ThemeStyle, mode: 'dark' | 'light', palette: ColorTokens): Record<string, string> {
  const dg = Object.fromEntries(Object.entries(palette).map(([k, v]) => [cssVarName(k), v]));
  const styleBlocks =
    style === DEFAULT_THEME_STYLE
      ? []
      : [block(`[data-theme-style="${style}"]`), block(`[data-theme-style="${style}"][data-theme="${mode}"]`)];
  const vars: Record<string, string> = Object.assign({}, block(':root'), block(`[data-theme="${mode}"]`), ...styleBlocks);
  expect(vars['--docs-code-bg']).toBeDefined();
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(vars)) {
    const ref = /^var\((--dg-[\w-]+)\)$/.exec(v);
    const hex = ref ? dg[ref[1]] : v;
    if (/^#[0-9a-fA-F]{6}$/.test(hex ?? '')) out[k] = hex;
  }
  return out;
}

test.each(ALL_THEMES.map((t) => [t.name, t] as const))('%s: every hljs token reaches 4.5:1 on the code background', (_, t) => {
  const vars = resolved(t.style, t.mode, t.colors);
  const bg = vars['--docs-code-bg'];
  expect(bg).toBeDefined();
  const tokens = Object.entries(vars).filter(([k]) => k.startsWith('--hljs-'));
  expect(tokens.length).toBe(7);
  const fails = tokens.filter(([, c]) => ratio(c, bg) < 4.5).map(([k, c]) => `${k} ${c}: ${ratio(c, bg).toFixed(2)}`);
  expect(fails).toEqual([]);
});
