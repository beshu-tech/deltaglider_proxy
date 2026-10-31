/**
 * UX audit P3 item 1: one type scale (TYPE in components/shared-styles.ts).
 * Measured before: 25 font sizes in the admin, including 9, 10, 10.5, 11.5,
 * 12.5 and 13.5 px, and a theme.css rule that repaired 9-10 px tags by
 * matching the text of their inline style. This test refuses a font size off
 * the scale in TS/TSX and in the app stylesheets. An icon's size is not text:
 * a line that renders an AntD icon is not checked.
 */
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { expect, test } from 'vitest';
import { TYPE_SCALE } from '../components/shared-styles';

const SRC = join(__dirname, '..');
const ALLOWED = new Set(TYPE_SCALE);

/**
 * Files with their own scale, each for a reason:
 * - docs.css: the long-form reading scale of the docs (15 px body, 30/22/19 headings).
 * - the Slack mocks draw Slack's own UI at Slack's sizes.
 */
const EXEMPT = new Set(['docs.css', 'components/SlackConnectorCard.tsx', 'components/SlackSetupGuide.tsx']);

/** Off-scale font sizes in one source. Pure. */
export function offScaleFontSizes(source: string, isCss: boolean): string[] {
  const out: string[] = [];
  if (isCss) {
    const css = source.replace(/\/\*[\s\S]*?\*\//g, '');
    for (const m of css.matchAll(/font-size:\s*(\d+(?:\.\d+)?)px|font:\s*\d+\s+(\d+(?:\.\d+)?)px/g)) {
      const px = Number(m[1] ?? m[2]);
      if (!ALLOWED.has(px)) out.push(`${px}px`);
    }
    return out;
  }
  for (const line of source.split('\n')) {
    if (/<\w+(Outlined|Filled|TwoTone)\b/.test(line)) continue;
    // The whole value expression, so `cond ? 13.5 : 15` is checked too.
    for (const m of line.matchAll(/fontSize:\s*([^,}\n]+)/g)) {
      for (const n of m[1].matchAll(/(^|[^\w.$[])(\d+(?:\.\d+)?)(?![\w.%\]])/g)) {
        if (!ALLOWED.has(Number(n[2]))) out.push(`fontSize: ${n[2]}`);
      }
    }
  }
  return out;
}

function files(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return e.name === '__tests__' || e.name === 'test' ? [] : files(p);
    return /\.(tsx?|css)$/.test(e.name) ? [p] : [];
  });
}

test('the scanner finds off-scale sizes and skips icons', () => {
  expect(offScaleFontSizes("fontSize: 12.5, x: 1\n<CheckOutlined style={{ fontSize: 9 }} />\nfontSize: 13", false)).toEqual(['fontSize: 12.5']);
  expect(offScaleFontSizes('fontSize: compact ? 13.5 : 16, fontSize: TYPE.sm, fontSize: `${px}px`, fontSize: "0.9em"', false)).toEqual(['fontSize: 13.5']);
  expect(offScaleFontSizes('a { font-size: 15px; font: 800 16px/1 x; } /* font-size: 9px */', true)).toEqual(['15px']);
});

test('every font size is on the type scale', () => {
  const found = files(SRC)
    .map((f) => [relative(SRC, f), f] as const)
    .filter(([rel]) => !EXEMPT.has(rel))
    .flatMap(([rel, f]) => offScaleFontSizes(readFileSync(f, 'utf8'), f.endsWith('.css')).map((v) => `${rel}: ${v}`));
  expect(found).toEqual([]);
});
