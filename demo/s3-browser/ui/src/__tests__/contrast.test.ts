// Browser-review item 23: WCAG AA contrast (4.5:1) of the text tokens on the
// surfaces they sit on, in both themes, and of the primary button text.
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { darkColors, lightColors } from '../ThemeContext';
import { darkTheme, lightTheme } from '../theme';

function luminance(hex: string): number {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function ratio(a: string, b: string): number {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
}

for (const [name, c, t] of [['dark', darkColors, darkTheme], ['light', lightColors, lightTheme]] as const) {
  test(`${name}: text tokens reach 4.5:1 on every surface`, () => {
    const surfaces = { BG_BASE: c.BG_BASE, BG_SIDEBAR: c.BG_SIDEBAR, BG_CARD: c.BG_CARD, BG_ELEVATED: c.BG_ELEVATED };
    const texts = {
      TEXT_PRIMARY: c.TEXT_PRIMARY, TEXT_SECONDARY: c.TEXT_SECONDARY, TEXT_MUTED: c.TEXT_MUTED, TEXT_FAINT: c.TEXT_FAINT,
      ACCENT_BLUE: c.ACCENT_BLUE, ACCENT_GREEN: c.ACCENT_GREEN, ACCENT_RED: c.ACCENT_RED, ACCENT_AMBER: c.ACCENT_AMBER, ACCENT_PURPLE: c.ACCENT_PURPLE, colorTextSecondary: t.token.colorTextSecondary, colorTextPlaceholder: t.token.colorTextPlaceholder,
    };
    const fails: string[] = [];
    for (const [tn, tc] of Object.entries(texts)) {
      for (const [sn, sc] of Object.entries(surfaces)) {
        const r = ratio(tc, sc);
        if (r < 4.5) fails.push(`${tn} ${tc} on ${sn} ${sc}: ${r.toFixed(2)}`);
      }
    }
    assert.deepEqual(fails, []);
  });

  test(`${name}: primary button text reaches 4.5:1`, () => {
    const onPrimary = 'colorTextLightSolid' in t.token ? (t.token as { colorTextLightSolid: string }).colorTextLightSolid : '#ffffff';
    assert.ok(ratio(onPrimary, t.token.colorPrimary) >= 4.5, `${onPrimary} on ${t.token.colorPrimary}`);
  });
}
