/** UX harvest D48: mermaid diagrams used the default palette and 9 px text. */
import { describe, expect, test } from 'vitest';
import { darkColors, lightColors } from '../ThemeContext';
import { mermaidConfig, mermaidDisplayWidth, mixHex } from '../mermaidTheme';

describe('mermaidConfig', () => {
  test.each([
    ['light', lightColors, false],
    ['dark', darkColors, true],
  ] as const)('%s: brand colours and readable text', (_, colors, isDark) => {
    const c = mermaidConfig(colors, isDark);
    expect(c.theme).toBe('base');
    expect(parseInt(c.themeVariables.fontSize, 10)).toBeGreaterThanOrEqual(12);
    expect(c.themeVariables.primaryBorderColor).toBe(colors.ACCENT_BLUE);
    expect(c.themeVariables.primaryTextColor).toBe(colors.TEXT_PRIMARY);
    // Every colour is a plain #rrggbb (mermaid derives shades from it).
    for (const [k, v] of Object.entries(c.themeVariables)) {
      if (/Color$|Bkg$|Border$|^background$/.test(k)) expect(v).toMatch(/^#[0-9a-f]{6}$/i);
    }
  });
});

test('mixHex', () => {
  expect(mixHex('#000000', '#ffffff', 0.5)).toBe('#808080');
  expect(mixHex('#ffffff', '#0f766e', 0)).toBe('#ffffff');
});

describe('mermaidDisplayWidth', () => {
  test('a small diagram is not scaled up', () => {
    expect(mermaidDisplayWidth(400, 800)).toBe(400);
  });
  test('a wide diagram fits when its text stays readable', () => {
    expect(mermaidDisplayWidth(900, 800)).toBe(800);
  });
  test('a very wide diagram keeps 12 px text and scrolls', () => {
    // 2000 px into 600 px would make 14 px text 4.2 px.
    expect(mermaidDisplayWidth(2000, 600)).toBe(Math.ceil(2000 * 12 / 14));
  });
});
