// Browser-review item 23: WCAG AA contrast (4.5:1) of the text tokens on the
// surfaces they sit on, in both themes, and of the primary button text.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { theme, type ThemeConfig } from 'antd';
import { test } from 'vitest';
import { flatten, meetsAA, ratio } from '../test/contrastMath';
import { ALL_THEMES } from '../test/themes';

for (const { name, colors: c, antd: t } of ALL_THEMES) {
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
        if (!meetsAA(r)) fails.push(`${tn} ${tc} on ${sn} ${sc}: ${r.toFixed(2)}`);
      }
    }
    assert.deepEqual(fails, []);
  });

  test(`${name}: primary button text reaches 4.5:1`, () => {
    const onPrimary = 'colorTextLightSolid' in t.token ? (t.token as { colorTextLightSolid: string }).colorTextLightSolid : '#ffffff';
    assert.ok(meetsAA(ratio(onPrimary, t.token.colorPrimary)), `${onPrimary} on ${t.token.colorPrimary}`);
  });
}

// B37: links use the brand teal and reach 4.5:1 on every surface.
for (const { name, colors: c, antd: t } of ALL_THEMES) {
  test(`${name}: link colour is the brand teal and reaches 4.5:1`, () => {
    const link = (t.token as { colorLink?: string }).colorLink;
    assert.ok(link, 'colorLink is set (AntD default is blue #1677ff)');
    assert.equal(link, c.ACCENT_BLUE);
    for (const surface of [c.BG_BASE, c.BG_CARD, c.BG_ELEVATED]) {
      assert.ok(meetsAA(ratio(link, surface)), `${link} on ${surface}: ${ratio(link, surface).toFixed(2)}`);
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

for (const { name, mode, antd: t } of ALL_THEMES) {
  test(`${name}: tag text reaches 4.5:1 on its tint`, () => {
    const tok = theme.getDesignToken(t as ThemeConfig) as unknown as Record<string, string>;
    const overrides = tagOverrides(mode);
    const fails: string[] = [];
    for (const p of PRESETS) {
      const text = overrides[p] ?? tok[`${p}7`];
      const r = ratio(text, tok[`${p}1`]);
      if (!meetsAA(r)) fails.push(`${p} ${text} on ${tok[`${p}1`]}: ${r.toFixed(2)}`);
    }
    for (const [cls, key] of Object.entries(STATUSES)) {
      const text = overrides[cls] ?? tok[`color${key}`];
      const r = ratio(text, tok[`color${key}Bg`]);
      if (!meetsAA(r)) fails.push(`${cls} ${text} on ${tok[`color${key}Bg`]}: ${r.toFixed(2)}`);
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

for (const { name, colors: c } of ALL_THEMES) {
  test(`${name}: BucketCard chip text reaches 4.5:1 on its tint`, () => {
    const src = readFileSync(join(__dirname, '../components/BucketCard.tsx'), 'utf8');
    const tint = /background: tint\(tone, (\d+)\)/.exec(src);
    assert.ok(tint, 'Chip background is tint(tone, <percent>)');
    const alpha = Number(tint[1]) / 100;
    const wash = /isPublic \? tint\(colors\.ACCENT_AMBER, (\d+)\) : colors\.BG_ELEVATED/.exec(src);
    assert.ok(wash, 'Public card background is tint(colors.ACCENT_AMBER, <percent>)');
    const publicCard = over(c.ACCENT_AMBER, c.BG_ELEVATED, Number(wash[1]) / 100);
    const fails: string[] = [];
    for (const [tn, tone] of Object.entries({
      TEXT_MUTED: c.TEXT_MUTED, ACCENT_BLUE: c.ACCENT_BLUE, ACCENT_GREEN: c.ACCENT_GREEN,
      ACCENT_PURPLE: c.ACCENT_PURPLE, ACCENT_AMBER: c.ACCENT_AMBER,
    })) {
      for (const [sn, card] of Object.entries({ card: c.BG_ELEVATED, publicCard })) {
        const r = ratio(tone, over(tone, card, alpha));
        if (!meetsAA(r)) fails.push(`${tn} on its tint on ${sn}: ${r.toFixed(2)}`);
      }
    }
    assert.deepEqual(fails, []);
  });
}

// Ink on fills: text drawn ON a coloured fill reaches 4.5:1 on every stop of
// that fill (white on the dark-theme accents was 1.7-2.5:1 in ActionChips,
// and white on the green saved bar 2:1 in the analytics hero).
const stops = (gradient: string) => [...gradient.matchAll(/#[0-9a-f]{6}/gi)].map((m) => m[0]);
for (const { name, colors: c } of ALL_THEMES) {
  test(`${name}: ON_ACCENT, ON_KEPT and ON_SAVED reach 4.5:1 on their fills`, () => {
    const fails: string[] = [];
    const check = (ink: string, inkName: string, fills: Record<string, string>) => {
      for (const [fn, fill] of Object.entries(fills)) {
        const r = ratio(ink, fill);
        if (!meetsAA(r)) fails.push(`${inkName} on ${fn} ${fill}: ${r.toFixed(2)}`);
      }
    };
    check(c.ON_ACCENT, 'ON_ACCENT', {
      ACCENT_BLUE: c.ACCENT_BLUE, ACCENT_GREEN: c.ACCENT_GREEN, ACCENT_RED: c.ACCENT_RED,
      ACCENT_AMBER: c.ACCENT_AMBER, ACCENT_PURPLE: c.ACCENT_PURPLE,
    });
    check(c.ON_KEPT, 'ON_KEPT', Object.fromEntries(stops(c.BAR_KEPT).map((h, i) => [`BAR_KEPT[${i}]`, h])));
    check(c.ON_SAVED, 'ON_SAVED', Object.fromEntries(stops(c.BAR_SAVED).map((h, i) => [`BAR_SAVED[${i}]`, h])));
    assert.deepEqual(fails, []);
  });
}

// A selected or hovered option of a Select, Menu or Dropdown is AntD's text
// (colorText, an alpha over the fill) on controlItemBgActive /
// controlItemBgHover, flattened onto the popup (colorBgElevated). The dark
// algorithm derives the selected fill from the primary: in Instrument dark it
// read 4.26:1 (axe, /admin/system Section select).
for (const { name, antd: t } of ALL_THEMES) {
  test(`${name}: a selected or hovered list item reaches 4.5:1`, () => {
    const tok = theme.getDesignToken(t as ThemeConfig) as unknown as Record<string, string>;
    const fails: string[] = [];
    for (const key of ['controlItemBgActive', 'controlItemBgActiveHover', 'controlItemBgHover']) {
      const fill = flatten(tok[key], tok.colorBgElevated);
      const ink = flatten(tok.colorText, fill);
      const r = ratio(ink, fill);
      if (!meetsAA(r)) fails.push(`${key}: ${ink} on ${fill}: ${r.toFixed(2)}`);
    }
    assert.deepEqual(fails, []);
  });
}

// A primary button keeps its ink (colorTextLightSolid) through hover and
// press: the hover fill is lighter than the rest fill (Editorial light "New
// job" failed axe while the pointer was on it).
for (const { name, antd: t } of ALL_THEMES) {
  test(`${name}: primary button text reaches 4.5:1 at rest, hover and press`, () => {
    const tok = theme.getDesignToken(t as ThemeConfig) as unknown as Record<string, string>;
    const fails: string[] = [];
    for (const key of ['colorPrimary', 'colorPrimaryHover', 'colorPrimaryActive']) {
      const fill = flatten(tok[key], tok.colorBgContainer);
      const r = ratio(flatten(tok.colorTextLightSolid, fill), fill);
      if (!meetsAA(r)) fails.push(`${key}: ${tok.colorTextLightSolid} on ${fill}: ${r.toFixed(2)}`);
    }
    assert.deepEqual(fails, []);
  });
}
