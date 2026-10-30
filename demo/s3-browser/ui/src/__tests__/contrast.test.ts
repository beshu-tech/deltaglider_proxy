// Browser-review item 23: WCAG AA contrast (4.5:1) of the text tokens on the
// surfaces they sit on, in both themes, and of the primary button text.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { theme, type ThemeConfig } from 'antd';
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

// B37: links use the brand teal and reach 4.5:1 on every surface.
for (const [name, c, t] of [['dark', darkColors, darkTheme], ['light', lightColors, lightTheme]] as const) {
  test(`${name}: link colour is the brand teal and reaches 4.5:1`, () => {
    const link = (t.token as { colorLink?: string }).colorLink;
    assert.ok(link, 'colorLink is set (AntD default is blue #1677ff)');
    assert.equal(link, c.ACCENT_BLUE);
    for (const surface of [c.BG_BASE, c.BG_CARD, c.BG_ELEVATED]) {
      assert.ok(ratio(link, surface) >= 4.5, `${link} on ${surface}: ${ratio(link, surface).toFixed(2)}`);
    }
  });
}

// B38: every AntD tag colour (preset and status), after the theme.css
// overrides, reaches 4.5:1 on its own tint in both themes.
const PRESETS = ['blue', 'purple', 'cyan', 'green', 'magenta', 'red', 'orange', 'yellow', 'volcano', 'geekblue', 'lime', 'gold'];
const STATUSES: Record<string, string> = { success: 'Success', warning: 'Warning', error: 'Error', processing: 'Info' };

function tagOverrides(mode: 'dark' | 'light'): Record<string, string> {
  const css = readFileSync(join(__dirname, '../theme.css'), 'utf8');
  const re = new RegExp(`\\[data-theme="${mode}"\\] \\.ant-tag:not\\(\\.ant-tag-solid\\)\\.ant-tag-([a-z]+) \\{ color: (#[0-9a-f]{6}) !important; \\}`, 'g');
  return Object.fromEntries([...css.matchAll(re)].map((m) => [m[1], m[2]]));
}

for (const [name, t] of [['dark', darkTheme], ['light', lightTheme]] as const) {
  test(`${name}: tag text reaches 4.5:1 on its tint`, () => {
    const tok = theme.getDesignToken(t as ThemeConfig) as unknown as Record<string, string>;
    const overrides = tagOverrides(name);
    const fails: string[] = [];
    for (const p of PRESETS) {
      const text = overrides[p] ?? tok[`${p}7`];
      const r = ratio(text, tok[`${p}1`]);
      if (r < 4.5) fails.push(`${p} ${text} on ${tok[`${p}1`]}: ${r.toFixed(2)}`);
    }
    for (const [cls, key] of Object.entries(STATUSES)) {
      const text = overrides[cls] ?? tok[`color${key}`];
      const r = ratio(text, tok[`color${key}Bg`]);
      if (r < 4.5) fails.push(`${cls} ${text} on ${tok[`color${key}Bg`]}: ${r.toFixed(2)}`);
    }
    assert.deepEqual(fails, []);
  });
}

// B38: a BucketCard chip is tone text on a tint of the same tone, on the
// card (BG_ELEVATED) or on the amber wash of a public bucket's card. The
// tint alpha comes from the source, so a stronger tint fails here.
function over(fg: string, bg: string, alpha: number): string {
  const ch = (h: string, i: number) => parseInt(h.slice(i, i + 2), 16);
  return '#' + [1, 3, 5]
    .map((i) => Math.round(ch(fg, i) * alpha + ch(bg, i) * (1 - alpha)).toString(16).padStart(2, '0'))
    .join('');
}

for (const [name, c] of [['dark', darkColors], ['light', lightColors]] as const) {
  test(`${name}: BucketCard chip text reaches 4.5:1 on its tint`, () => {
    const src = readFileSync(join(__dirname, '../components/BucketCard.tsx'), 'utf8');
    const tint = /background: `\$\{tone\}([0-9a-f]{2})`/.exec(src);
    assert.ok(tint, 'Chip background is `${tone}<alpha>`');
    const alpha = parseInt(tint[1], 16) / 255;
    const publicCard = over(c.ACCENT_AMBER, c.BG_ELEVATED, 0x0a / 255);
    const fails: string[] = [];
    for (const [tn, tone] of Object.entries({
      TEXT_MUTED: c.TEXT_MUTED, ACCENT_BLUE: c.ACCENT_BLUE, ACCENT_GREEN: c.ACCENT_GREEN,
      ACCENT_PURPLE: c.ACCENT_PURPLE, ACCENT_AMBER: c.ACCENT_AMBER,
    })) {
      for (const [sn, card] of Object.entries({ card: c.BG_ELEVATED, publicCard })) {
        const r = ratio(tone, over(tone, card, alpha));
        if (r < 4.5) fails.push(`${tn} on its tint on ${sn}: ${r.toFixed(2)}`);
      }
    }
    assert.deepEqual(fails, []);
  });
}
