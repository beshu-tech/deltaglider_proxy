/**
 * UX harvest D67: letter-spaced caps were a third heading system next to
 * sentence-case labels and page titles. Caps are for the eyebrow only: a
 * small group label over a list (sidebar groups, menu sections, table
 * headers) and the brand wordmark. Every other label is sentence case.
 *
 * This test refuses `textTransform: 'uppercase'` in components and
 * `text-transform: uppercase` in a CSS rule whose selector is not an eyebrow.
 */
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { expect, test } from 'vitest';

const SRC = join(__dirname, '..');

/** CSS selectors that may use caps: the eyebrow, the table headers and the
 *  brand wordmark (login, admin header lockup, sidebar footer). */
const EYEBROW_SELECTORS = [
  '.dg-eyebrow', '.account-menu-section-label', '.ant-table-thead', '.dg-record-hcell',
  '.dg-wordmark', '.dg-login-brand', '.dg-login-product',
];

function files(dir: string, ext: RegExp): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return files(p, ext);
    return ext.test(e.name) && !/\.test\./.test(e.name) ? [p] : [];
  });
}

/** Selectors of the innermost CSS rules that set uppercase. Pure. */
function uppercaseCssSelectors(css: string): string[] {
  const noComments = css.replace(/\/\*[\s\S]*?\*\//g, '');
  const out: string[] = [];
  for (const m of noComments.matchAll(/([^{};]+)\{([^{}]*)\}/g)) {
    if (/text-transform\s*:\s*uppercase/.test(m[2])) out.push(m[1].trim());
  }
  return out;
}

test('the CSS scanner finds the selector of an uppercase rule', () => {
  expect(uppercaseCssSelectors('.a { color: red } /* x */ .b span { text-transform: uppercase; }')).toEqual(['.b span']);
});

test('components never set uppercase inline', () => {
  const hits = files(SRC, /\.tsx?$/)
    .filter((f) => /textTransform\s*:\s*['"]uppercase['"]/.test(readFileSync(f, 'utf8')))
    .map((f) => relative(SRC, f));
  expect(hits).toEqual([]);
});

test('CSS sets uppercase only on eyebrow selectors', () => {
  const bad = files(SRC, /\.css$/).flatMap((f) =>
    uppercaseCssSelectors(readFileSync(f, 'utf8'))
      .filter((sel) => !EYEBROW_SELECTORS.some((e) => sel.includes(e)))
      .map((sel) => `${relative(SRC, f)}: ${sel}`),
  );
  expect(bad).toEqual([]);
});

// The brand is a wordmark, not a word: capitals with wide tracking on every
// surface that shows it as the product name (cycle 1 turned it into plain
// sentence case on the login screen; that was a regression).
test('every brand lockup uses the wordmark style', () => {
  const sites: Record<string, RegExp> = {
    'components/ConnectPage.tsx': /className="dg-login-brand">DeltaGlider</,
    'components/FullScreenHeader.tsx': /className="[^"]*dg-wordmark[^"]*"[^>]*>\s*DeltaGlider\s*</,
    'components/Sidebar.tsx': /className="dg-wordmark"[^>]*>DeltaGlider</,
  };
  const missing = Object.entries(sites)
    .filter(([file, re]) => !re.test(readFileSync(join(SRC, file), 'utf8')))
    .map(([file]) => file);
  expect(missing).toEqual([]);
  const css = readFileSync(join(SRC, 'theme.css'), 'utf8');
  for (const sel of ['.dg-wordmark', '.dg-login-brand', '.dg-login-product']) {
    const rule = css.slice(css.indexOf(`${sel} {`));
    expect(rule.slice(0, rule.indexOf('}')), sel).toMatch(/text-transform:\s*uppercase/);
  }
});
