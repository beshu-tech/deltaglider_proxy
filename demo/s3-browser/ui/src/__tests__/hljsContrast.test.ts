/**
 * UX harvest D43: the docs code blocks failed axe colour-contrast in the
 * light theme (YAML keys #e11d48 4.28:1, strings #059669 3.43:1). Every
 * syntax token must reach 4.5:1 on the code-block background, in both
 * themes. Read from theme.css, the only place the tokens are defined.
 */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test } from 'vitest';

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

/** The custom properties of the first block that starts with `selector {`. */
function block(selector: string): Record<string, string> {
  const start = css.indexOf(`${selector} {`);
  expect(start).toBeGreaterThanOrEqual(0);
  const body = css.slice(start, css.indexOf('\n}', start));
  return Object.fromEntries([...body.matchAll(/(--[\w-]+):\s*(#[0-9a-fA-F]{6})\b/g)].map((m) => [m[1], m[2]]));
}

test.each([['[data-theme="dark"]'], ['[data-theme="light"]']])('%s: every hljs token reaches 4.5:1 on the code background', (sel) => {
  const vars = block(sel);
  const bg = vars['--docs-code-bg'];
  expect(bg).toBeDefined();
  const tokens = Object.entries(vars).filter(([k]) => k.startsWith('--hljs-'));
  expect(tokens.length).toBe(7);
  const fails = tokens.filter(([, c]) => ratio(c, bg) < 4.5).map(([k, c]) => `${k} ${c}: ${ratio(c, bg).toFixed(2)}`);
  expect(fails).toEqual([]);
});
