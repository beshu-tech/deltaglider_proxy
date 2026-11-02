/**
 * Source guards for the theme layers (see the header of theme.css and the
 * "Styling" section of CLAUDE.md). ESLint guards the TS side (no colour
 * literal, no `${color}22`, no onMouseEnter style write); this file guards
 * the CSS side.
 */
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { expect, test } from 'vitest';
import { darkColors, lightColors, PALETTES, parseThemeStyle, THEME_STYLES, type ThemeStyle } from '../ThemeContext';
import { cssVarName, FONT_DISPLAY, FONT_MONO, FONT_UI, paletteDeclarations, themeSelector, themeStylesheet, tint } from '../themeVars';
import { BREAKPOINT } from '../components/shared-styles';

const SRC = join(__dirname, '..');

function files(dir: string, re: RegExp): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return e.name === '__tests__' || e.name === 'test' ? [] : files(p, re);
    return re.test(e.name) ? [p] : [];
  });
}
const read = (p: string) => readFileSync(p, 'utf8');
const stripComments = (css: string) => css.replace(/\/\*[\s\S]*?\*\//g, '');

const CSS = files(SRC, /\.css$/);
const CODE = files(SRC, /\.tsx?$/);
const THEME_CSS = join(SRC, 'theme.css');

/** Custom properties a stylesheet declares (`--x:`). */
function declared(css: string): Set<string> {
  return new Set([...stripComments(css).matchAll(/(--[a-z][\w-]*)\s*:/g)].map((m) => m[1]));
}
/** Custom properties an inline style sets (`'--x':` in TSX). */
function setInline(src: string): Set<string> {
  return new Set([...src.matchAll(/['"](--[a-z][\w-]*)['"]\s*:/g)].map((m) => m[1]));
}
function used(src: string): string[] {
  return [...stripComments(src).matchAll(/var\((--[a-z][\w-]*)/g)].map((m) => m[1]);
}

const generated = declared(themeStylesheet());
const definedInCss = new Set(CSS.flatMap((f) => [...declared(read(f))]));
const definedInline = new Set(CODE.flatMap((f) => [...setInline(read(f))]));
/** AntD's own variables: they are AntD's to define. */
const isAntd = (name: string) => name.startsWith('--ant-') || name === '--notification-top';

test('the palette publishes every token to CSS, in both themes', () => {
  expect(Object.keys(lightColors).sort()).toEqual(Object.keys(darkColors).sort());
  for (const token of Object.keys(darkColors)) expect(generated.has(cssVarName(token))).toBe(true);
  expect(paletteDeclarations(darkColors)).toContain('--dg-bg-card: #111827;');
  expect(themeStylesheet()).toContain(`--font-ui: ${FONT_UI};`);
  expect(themeStylesheet()).toContain(`--font-mono: ${FONT_MONO};`);
});

test('every style publishes both palettes and its display face', () => {
  const css = themeStylesheet();
  for (const style of Object.keys(PALETTES) as ThemeStyle[]) {
    for (const mode of ['dark', 'light'] as const) {
      expect(css).toContain(`${themeSelector(style, mode)} { ${paletteDeclarations(PALETTES[style][mode])} }`);
    }
  }
  expect(css).toContain('[data-theme-style="editorial"] { --font-display: ' + FONT_DISPLAY.editorial + '; }');
  expect(themeSelector('instrument', 'dark')).toBe('[data-theme="dark"]');
  expect(parseThemeStyle('editorial')).toBe('editorial');
  expect(parseThemeStyle('neon')).toBe('instrument');
  expect(parseThemeStyle(null)).toBe('instrument');
});

test('the pre-paint script in index.html knows every style', () => {
  const html = read(join(SRC, '..', 'index.html'));
  for (const s of THEME_STYLES.filter((x) => x.id !== 'instrument')) expect(html).toContain(`'${s.id}'`);
});

test('tint() works on every colour form', () => {
  expect(tint('#2dd4bf', 13)).toBe('color-mix(in srgb, #2dd4bf 13%, transparent)');
  expect(tint('var(--dg-accent-red)', 30)).toBe('color-mix(in srgb, var(--dg-accent-red) 30%, transparent)');
});

test('every var(--x) the app reads is defined somewhere', () => {
  const missing = new Set<string>();
  for (const f of [...CSS, ...CODE]) {
    for (const name of used(read(f))) {
      if (isAntd(name) || generated.has(name) || definedInCss.has(name) || definedInline.has(name)) continue;
      missing.add(`${relative(SRC, f)}: ${name}`);
    }
  }
  // An undefined var falls back silently: --docs-muted was never defined, and
  // its #8a93a6 fallback read 3:1 on the light docs page.
  expect([...missing]).toEqual([]);
});

test('every custom property a stylesheet declares is read', () => {
  const reads = new Set([...CSS, ...CODE].flatMap((f) => used(read(f))));
  const dead = [...definedInCss].filter((name) => !reads.has(name) && !isAntd(name));
  expect(dead).toEqual([]);
});

test('no var() of ours carries a fallback (it hides an undefined var)', () => {
  const found = [...CSS, ...CODE].flatMap((f) =>
    [...stripComments(read(f)).matchAll(/var\((--[a-z][\w-]*)\s*,/g)]
      .filter((m) => !isAntd(m[1]))
      .map((m) => `${relative(SRC, f)}: ${m[1]}`),
  );
  expect(found).toEqual([]);
});

test('component stylesheets hold no colour literal (palette vars only)', () => {
  const found = CSS.filter((f) => f !== THEME_CSS).flatMap((f) =>
    stripComments(read(f))
      .split('\n')
      .filter((l) => /#[0-9a-fA-F]{3,8}\b|(rgb|hsl)a?\(/.test(l))
      .map((l) => `${relative(SRC, f)}: ${l.trim()}`),
  );
  expect(found).toEqual([]);
});

test('theme.css: colour literals live only in custom-property declarations (and the tested tag overrides)', () => {
  const stray: string[] = [];
  let inValue = false;
  for (const line of stripComments(read(THEME_CSS)).split('\n')) {
    const t = line.trim();
    if (t.startsWith('--')) {
      inValue = !t.endsWith(';');
      continue;
    }
    if (inValue) {
      inValue = !t.endsWith(';');
      continue;
    }
    if (t.includes('.ant-tag')) continue; // contrast.test.ts checks these values
    if (/#[0-9a-fA-F]{3,8}\b|(rgb|hsl)a?\(/.test(t)) stray.push(t);
  }
  expect(stray).toEqual([]);
});

test('no text colour is faded with transparency (contrast is tested on the opaque tokens)', () => {
  // `color: color-mix(..., transparent)` blends the ink with whatever lies
  // behind it, so contrast.test.ts never sees the result: the account-menu
  // labels read 3.8:1 and the login hint 3:1. Use a TEXT_* token. Placeholder
  // and disabled ink are exempt (WCAG 1.4.3 does not cover them).
  const found = CSS.flatMap((f) => {
    const out: string[] = [];
    for (const m of stripComments(read(f)).matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      const selector = m[1].trim();
      if (/::?placeholder|:disabled|\[disabled\]/.test(selector)) continue;
      for (const d of m[2].split(';'))
        if (/^\s*color\s*:\s*color-mix\([^;]*transparent/.test(d)) out.push(`${relative(SRC, f)}: ${selector} { ${d.trim()} }`);
    }
    return out;
  });
  expect(found).toEqual([]);
});

test('font families are named only in themeVars.ts', () => {
  const found = [...CSS, ...CODE]
    .filter((f) => !f.endsWith('themeVars.ts'))
    .flatMap((f) =>
      stripComments(read(f))
        .split('\n')
        .filter((l) => /['"](Manrope|JetBrains Mono|Fira Code|Fraunces)/.test(l) && !/^\s*(\*|\/\/)/.test(l))
        .map((l) => `${relative(SRC, f)}: ${l.trim()}`),
    );
  expect(found).toEqual([]);
});

test('no stylesheet loads anything from another origin (fonts are bundled)', () => {
  const found = CSS.filter((f) => /@import\s+url\(\s*['"]?https?:|url\(\s*['"]?https?:/.test(read(f)));
  expect(found.map((f) => relative(SRC, f))).toEqual([]);
});

test('no CSS selector matches on the text of an inline style', () => {
  // `[style*="font-size: 9px"]` repaired an inline value instead of fixing it.
  const found = CSS.flatMap((f) =>
    [...stripComments(read(f)).matchAll(/\[style\*=[^\]]*\]/g)].map((m) => `${relative(SRC, f)}: ${m[0]}`),
  );
  expect(found).toEqual(ALLOWED_STYLE_SNIFFS);
});

/**
 * The one sanctioned sniff: mono text keeps its normal word spacing. Mono is
 * set inline (`fontFamily: 'var(--font-mono)'`) in ~170 places; the rule
 * goes when those move to a class.
 */
const ALLOWED_STYLE_SNIFFS = [
  'theme.css: [style*="font-mono"]',
  'theme.css: [style*="monospace"]',
  'theme.css: [style*="font-mono"]',
  'theme.css: [style*="font-ui"]',
  'theme.css: [style*="monospace"]',
  'theme.css: [style*="font-ui"]',
];

test('every class and keyframes a stylesheet defines is used', () => {
  // Comments do not count as a use: a class named only in a comment is dead.
  const code = CODE.map((f) => stripComments(read(f)).replace(/^\s*\/\/.*$/gm, '')).join('\n') + read(join(SRC, '..', 'index.html'));
  const dead: string[] = [];
  for (const f of CSS) {
    const css = stripComments(read(f)).replace(/url\([^)]*\)/g, '');
    const selectors = [...css.matchAll(/([^{}]+)\{/g)].map((m) => m[1]).join(' ');
    const classes = new Set([...selectors.matchAll(/\.(-?[a-zA-Z_][\w-]*)/g)].map((m) => m[1]));
    for (const c of classes) {
      if (/^(ant-|anticon|hljs)/.test(c)) continue; // AntD's and highlight.js's own classes
      if (!new RegExp(`(^|[^\\w-])${c}(?![\\w-])`).test(code)) dead.push(`${relative(SRC, f)}: .${c}`);
    }
    for (const [, k] of css.matchAll(/@keyframes\s+([\w-]+)/g)) {
      const uses = (code + css).match(new RegExp(`(^|[^\\w-])${k}(?![\\w-])`, 'g'))?.length ?? 0;
      if (uses <= 1) dead.push(`${relative(SRC, f)}: @keyframes ${k}`);
    }
  }
  expect(dead).toEqual([]);
});

test('media queries sit on the BREAKPOINT edges (or a named exception)', () => {
  const edges = new Set(Object.values(BREAKPOINT).flatMap((bp) => [`max-width: ${bp - 1}px`, `min-width: ${bp}px`]));
  // The login card: its own width (480 px) plus the shell padding.
  const exceptions = new Set(['theme.css: max-width: 560px']);
  const off = CSS.flatMap((f) =>
    [...stripComments(read(f)).matchAll(/@media[^{]*?\((m(?:ax|in)-width:\s*\d+px)\)/g)]
      .map((m) => `${relative(SRC, f)}: ${m[1].replace(/\s+/, ' ')}`)
      .filter((q) => !edges.has(q.split(': ').slice(1).join(': ')) && !exceptions.has(q)),
  );
  expect(off).toEqual([]);
  const literal = CODE.flatMap((f) =>
    [...read(f).matchAll(/useIsNarrow\((\d+)\)/g)].map((m) => `${relative(SRC, f)}: useIsNarrow(${m[1]})`),
  );
  expect(literal).toEqual([]);
});

test('every zIndex is a value of Z (theme.ts)', () => {
  const found = CODE.flatMap((f) =>
    [...read(f).matchAll(/zIndex:\s*(\d+)/g)].map((m) => `${relative(SRC, f)}: zIndex: ${m[1]}`),
  );
  expect(found).toEqual([]);
});
