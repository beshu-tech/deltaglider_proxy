/** UX harvest D21: the request-rate chart was AntD default blue. */
import { expect, test } from 'vitest';
import { ALL_THEMES } from '../test/themes';
import { chartPalette, statusColors } from '../components/dashboard/chartDefaults';

test.each(ALL_THEMES.map((t) => [t.name, t.colors] as const))('%s: chart colours are theme tokens, the primary first and amber second', (_, colors) => {
  const p = chartPalette(colors);
  expect(p.primary).toBe(colors.ACCENT_BLUE);
  expect(p.secondary).toBe(colors.ACCENT_AMBER);
  const tokens = new Set(Object.values(colors).filter((v) => typeof v === 'string'));
  for (const c of [...p.series, ...Object.values(statusColors(colors))]) {
    expect(tokens.has(c)).toBe(true);
    expect(c.toLowerCase()).not.toMatch(/^#(1677ff|60a5fa)$/);
  }
});

// A success is the success colour in every style: in Editorial the primary
// is coral, and coral 2xx bars read as an alarm.
test.each(ALL_THEMES.map((t) => [t.name, t.colors] as const))('%s: 2xx is green, 5xx red', (_, colors) => {
  expect(statusColors(colors)['2xx']).toBe(colors.ACCENT_GREEN);
  expect(statusColors(colors)['5xx']).toBe(colors.ACCENT_RED);
});
