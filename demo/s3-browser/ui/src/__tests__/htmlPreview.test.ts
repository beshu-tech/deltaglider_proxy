import { describe, expect, test } from 'vitest';
import { buildPreviewDocument, isHtmlKey, previewCsp, sandboxFlags } from '../components/htmlPreview';

const ALL = [
  { scripts: false, remote: false },
  { scripts: true, remote: false },
  { scripts: false, remote: true },
  { scripts: true, remote: true },
];

describe('sandbox', () => {
  test.each(ALL)('never same-origin, forms, popups or top navigation (%o)', (opts) => {
    const flags = sandboxFlags(opts);
    for (const f of ['allow-same-origin', 'allow-forms', 'allow-popups', 'allow-top-navigation', 'allow-modals']) {
      expect(flags).not.toContain(f);
    }
  });
  test('scripts run only when the user allows them', () => {
    expect(sandboxFlags({ scripts: false, remote: true })).toBe('');
    expect(sandboxFlags({ scripts: true, remote: false })).toBe('allow-scripts');
  });
});

describe('csp', () => {
  test.each(ALL)('the page can never send a request or submit a form (%o)', (opts) => {
    const csp = previewCsp(opts);
    expect(csp).toContain("default-src 'none'");
    expect(csp).toContain("connect-src 'none'");
    expect(csp).toContain("form-action 'none'");
    expect(csp).toContain("base-uri 'none'");
  });
  test('by default nothing loads from the network and no script runs', () => {
    const csp = previewCsp({ scripts: false, remote: false });
    expect(csp).not.toContain('https:');
    expect(csp).toContain("script-src 'none'");
  });
  test('remote content allows https loads only', () => {
    const csp = previewCsp({ scripts: false, remote: true });
    expect(csp).toContain('img-src data: blob: https:');
    expect(csp).not.toContain('http:');
    expect(csp).toContain("script-src 'none'");
  });
  test('external scripts need both switches', () => {
    expect(previewCsp({ scripts: true, remote: false })).toContain("script-src 'unsafe-inline';");
    expect(previewCsp({ scripts: true, remote: true })).toContain("script-src 'unsafe-inline' https:;");
  });
});

describe('document', () => {
  const opts = { scripts: false, remote: false };
  test('the CSP comes first, before any page markup', () => {
    const doc = buildPreviewDocument('<html><head><script>x()</script></head></html>', opts);
    expect(doc.indexOf('Content-Security-Policy')).toBeLessThan(doc.indexOf('<html>'));
    expect(doc.startsWith('<meta http-equiv="Content-Security-Policy"')).toBe(true);
  });
  test('the doctype stays first, so the page keeps standards mode', () => {
    const doc = buildPreviewDocument('  <!DOCTYPE html>\n<html><body>hi</body></html>', opts);
    expect(doc).toMatch(/^\s*<!DOCTYPE html><meta http-equiv="Content-Security-Policy"/);
    expect(doc).toContain('<html><body>hi</body></html>');
  });
  test('a fragment gets the CSP too', () => {
    expect(buildPreviewDocument('<p>hi</p>', opts)).toMatch(/^<meta http-equiv="Content-Security-Policy".*<p>hi<\/p>$/);
  });
  test('the policy is attribute-escaped', () => {
    const doc = buildPreviewDocument('', opts);
    const content = /content="([^"]*)"/.exec(doc)?.[1] ?? '';
    expect(content).toBe(previewCsp(opts).replace(/&/g, '&amp;'));
  });
});

test.each([
  ['site/index.html', true],
  ['a/B.HTM', true],
  ['.html', false],
  ['dir.html/page', false],
  ['x.xhtml', true],
  ['readme.md', false],
  ['html', false],
  ['page.html.gz', false],
])('isHtmlKey(%s) = %s', (key, want) => {
  expect(isHtmlKey(key)).toBe(want);
});
