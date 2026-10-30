// anchors.test.ts — every .md link inside docs/product names a doc, and every #anchor link resolves on the
// website: an in-page "#x" to an id on the same rendered page, and a
// "other.md#x" to an id on the other page. The ids come from the real
// renderer (renderDoc), so a slug rule that drifts from the GitHub one the
// markdown links assume fails here. The product viewer has the same test in
// demo/s3-browser/ui/src/__tests__/docAnchors.test.ts.
import { describe, it, expect } from 'vitest';
import { unified } from 'unified';
import remarkParse from 'remark-parse';
import remarkGfm from 'remark-gfm';
import { visit } from 'unist-util-visit';
import { DOCS, rewriteDocLink } from './docs';
import { docContent } from './docContent';
import { renderDoc } from './renderDoc';

function linksOf(markdown: string): string[] {
  const tree = unified().use(remarkParse).use(remarkGfm).parse(markdown);
  const out: string[] = [];
  visit(tree, 'link', (node: { url: string }) => { out.push(node.url); });
  return out;
}

describe('intra-docs #anchor links (website)', () => {
  it('resolve to a heading id on the target page', async () => {
    const idsByUrl = new Map<string, Set<string>>();
    for (const d of DOCS) {
      const html = await renderDoc(docContent(d.path) ?? '', d.path);
      idsByUrl.set(d.url, new Set([...html.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1])));
    }
    const broken: string[] = [];
    for (const d of DOCS) {
      for (const href of linksOf(docContent(d.path) ?? '')) {
        let url: string;
        let anchor: string;
        if (href.startsWith('#')) {
          url = d.url;
          anchor = href.slice(1);
        } else {
          if (/^[a-z]+:/i.test(href) || href.startsWith('/')) continue;
          const hashAt = href.indexOf('#');
          if (!(hashAt < 0 ? href : href.slice(0, hashAt)).endsWith('.md')) continue;
          const page = rewriteDocLink(href, d.path);
          // A link to a missing doc is broken with or without an anchor.
          if (!page) { broken.push(`${d.path}: ${href} (no such doc)`); continue; }
          if (hashAt < 0) continue;
          url = page.slice(0, page.indexOf('#'));
          anchor = page.slice(page.indexOf('#') + 1);
        }
        if (!anchor) continue;
        if (!idsByUrl.get(url)?.has(decodeURIComponent(anchor))) broken.push(`${d.path}: ${href}`);
      }
    }
    expect(broken).toEqual([]);
  }, 60_000);
});
