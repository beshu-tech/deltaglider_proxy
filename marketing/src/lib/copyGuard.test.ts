// copyGuard — the site's copy rules as a test (docs/plan/site-rewrite-2026-11.md §3).
// The site must read as written by the engineers who run the product: no
// hype words, no vague calls to action, no exclamation marks. A new page
// that breaks a rule fails here, with the file and the line.
import { describe, expect, it } from 'vitest';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';

const SRC = join(__dirname, '..');
const ROOTS = ['pages', 'components', 'layouts'].map((d) => join(SRC, d));

const BANNED_WORDS = [
  'seamless', 'seamlessly', 'effortless', 'effortlessly', 'powerful', 'robust',
  'blazing', 'unlock', 'supercharge', 'empower', 'leverage', 'revolutionary',
  'next-generation', 'next-gen', 'enterprise-grade', 'world-class',
  'cutting-edge', 'game-changing', 'best-in-class', 'trusted by', 'at scale',
];

// A link or button names what happens. These labels do not.
const BANNED_LABELS = ['Get started', 'Learn more', 'Book a demo', 'Request a demo', 'Sign up now'];

function files(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) return files(p);
    return /\.(astro|tsx)$/.test(name) ? [p] : [];
  });
}

/** The text a visitor reads: code fences, scripts, styles and comments removed. */
export function visibleText(source: string): string {
  return source
    .replace(/^---[\s\S]*?^---/m, (m) => m.replace(/\/\/.*$/gm, '').replace(/\/\*[\s\S]*?\*\//g, ''))
    .replace(/<style[\s\S]*?<\/style>/g, '')
    .replace(/<script[\s\S]*?<\/script>/g, '')
    .replace(/\{\/\*[\s\S]*?\*\/\}/g, '')
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/^\s*\/\/.*$/gm, '');
}

function findings(test: (line: string) => string | null): string[] {
  const out: string[] = [];
  for (const root of ROOTS) {
    for (const f of files(root)) {
      visibleText(readFileSync(f, 'utf8'))
        .split('\n')
        .forEach((line) => {
          const hit = test(line);
          if (hit) out.push(`${relative(SRC, f)}: "${hit}" in: ${line.trim().slice(0, 100)}`);
        });
    }
  }
  return out;
}

describe('site copy rules', () => {
  it('uses no hype words', () => {
    const re = new RegExp(`\\b(${BANNED_WORDS.join('|')})\\b`, 'i');
    expect(findings((l) => l.match(re)?.[0] ?? null)).toEqual([]);
  });

  it('names every call to action for what happens', () => {
    const re = new RegExp(`>\\s*(${BANNED_LABELS.join('|')})\\s*<`, 'i');
    expect(findings((l) => l.match(re)?.[1] ?? null)).toEqual([]);
  });

  it('has no exclamation mark in a sentence', () => {
    // `!` after a letter, then a space, a tag or the end of the line.
    const re = /[A-Za-z]!(\s|<|$)/;
    expect(findings((l) => (re.test(l) && !/[!=]==?|!important/.test(l) ? '!' : null))).toEqual([]);
  });

  it('the guard itself reads visible text only', () => {
    const src = '---\n// seamless comment\nconst x = 1;\n---\n<p>plain</p>\n<style>.a{b:c}</style>{/* robust */}';
    expect(visibleText(src)).not.toMatch(/seamless|robust/);
    expect(visibleText('<p>seamless</p>')).toMatch(/seamless/);
  });
});
