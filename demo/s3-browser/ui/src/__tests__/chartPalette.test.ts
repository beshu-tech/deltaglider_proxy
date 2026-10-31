/** UX harvest D21: the request-rate chart was AntD default blue. */
import { expect, test } from 'vitest';
import { darkColors, lightColors } from '../ThemeContext';
import { chartPalette, statusColors } from '../components/dashboard/chartDefaults';

test.each([
  ['dark', darkColors],
  ['light', lightColors],
])('%s: chart colours are theme tokens, teal first and amber second', (_, colors) => {
  const p = chartPalette(colors);
  expect(p.primary).toBe(colors.ACCENT_BLUE);
  expect(p.secondary).toBe(colors.ACCENT_AMBER);
  const tokens = new Set(Object.values(colors).filter((v) => typeof v === 'string'));
  for (const c of [...p.series, ...Object.values(statusColors(colors))]) {
    expect(tokens.has(c)).toBe(true);
    expect(c.toLowerCase()).not.toMatch(/^#(1677ff|60a5fa)$/);
  }
});
