// llms.test.ts — /llms.txt and /llms-full.txt cover every doc in the manifest.
import { describe, it, expect } from 'vitest';
import manifest from '../../../docs/product/manifest.json';
import { DOCS } from './docs';
import { buildLlmsTxt, buildLlmsFull, docMarkdown, markdownUrl } from './llms';

const SITE = 'https://example.test';

describe('llms.txt', () => {
  const txt = buildLlmsTxt(SITE);

  it('lists every manifest entry exactly once, as a markdown URL', () => {
    expect(DOCS.length).toBe(manifest.docs.length);
    for (const d of DOCS) {
      const link = `](${SITE}${markdownUrl(d)})`;
      expect(txt.split(link).length - 1, `${d.path} → ${link}`).toBe(1);
    }
    const links = txt.match(/^- \[/gm) ?? [];
    expect(links.length).toBe(manifest.docs.length);
  });

  it('follows the llmstxt.org shape: H1 title, then a blockquote summary', () => {
    const lines = txt.split('\n');
    expect(lines[0]).toBe('# DeltaGlider Proxy');
    expect(lines[2].startsWith('> ')).toBe(true);
    expect(txt).toContain(`${SITE}/llms-full.txt`);
  });
});

describe('llms-full.txt', () => {
  const full = buildLlmsFull(SITE);

  it('contains every doc, in reading order', () => {
    let at = -1;
    for (const d of DOCS.filter((x) => x.slug === '')) expect(full).toContain(`${SITE}${d.url} ·`);
    for (const g of manifest.groups) {
      for (const d of DOCS.filter((x) => x.group === g.id).sort((a, b) => a.order - b.order)) {
        const i = full.indexOf(`<!-- ${d.title} · ${SITE}${d.url} ·`);
        expect(i, d.path).toBeGreaterThan(at);
        at = i;
      }
    }
  });
});

describe('docMarkdown', () => {
  it('turns inter-doc links into absolute .md URLs and keeps the anchor', () => {
    const md = docMarkdown('how-to/serve-tls', SITE);
    expect(md).toContain(`](${SITE}/docs/how-to/troubleshooting.md#`);
    expect(md).not.toMatch(/\]\((?!https?:|#|mailto:)[^)]*\.md/);
  });

  it('points screenshots at the site copy', () => {
    expect(docMarkdown('README', SITE)).not.toContain('(/_/screenshots/');
  });
});
