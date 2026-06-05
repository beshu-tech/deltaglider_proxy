// renderDoc.ts — Markdown → HTML for the website docs, using the same
// remark-gfm pipeline the product viewer uses (react-markdown + remark-gfm),
// so rendering matches. On top of the base pipeline we apply doc-aware
// rewrites that the product does at the React layer:
//   - inter-doc `*.md` links  → friendly /docs/ URLs (resolved per source doc)
//   - `/_/screenshots/*` images → the website's /screenshots/* served copies
//   - heading ids (so in-page `#anchor` links resolve)
//   - ```mermaid fences → <pre class="mermaid"> for client-side rendering
//
// unified + remark-* + rehype-* are already in the tree (Astro deps); no new
// packages added.

import { unified } from 'unified';
import remarkParse from 'remark-parse';
import remarkGfm from 'remark-gfm';
import remarkRehype from 'remark-rehype';
import rehypeRaw from 'rehype-raw';
import rehypeStringify from 'rehype-stringify';
import { visit } from 'unist-util-visit';
import { rewriteDocLink, rewriteAssetSrc } from './docs';
export { extractTitle, extractSummary } from './docText';

/** GitHub-style heading slug: lower, strip punctuation, spaces→dashes. */
function slugifyHeading(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^\w\s-]/g, '')
    .trim()
    .replace(/\s+/g, '-');
}

/** Collect the visible text of a hast element (for heading ids). */
function textOf(node: any): string {
  if (node.type === 'text') return node.value;
  if (node.children) return node.children.map(textOf).join('');
  return '';
}

/**
 * rehype plugin: rewrite links/images/headings/mermaid for one doc.
 * `fromPath` is the docs/product path of the doc being rendered, needed to
 * resolve relative `../` inter-doc links correctly.
 */
function rehypeDocRewrites(fromPath: string) {
  return (tree: any) => {
    const usedHeadingIds = new Set<string>();
    visit(tree, 'element', (node: any) => {
      // Inter-doc links → friendly URLs.
      if (node.tagName === 'a' && typeof node.properties?.href === 'string') {
        const rewritten = rewriteDocLink(node.properties.href, fromPath);
        if (rewritten) node.properties.href = rewritten;
      }
      // Screenshot/image src → website-served path.
      if (node.tagName === 'img' && typeof node.properties?.src === 'string') {
        node.properties.src = rewriteAssetSrc(node.properties.src);
        node.properties.loading = 'lazy';
      }
      // Heading ids for #anchor links (dedupe collisions like GitHub).
      if (/^h[1-6]$/.test(node.tagName) && !node.properties?.id) {
        let id = slugifyHeading(textOf(node));
        if (id) {
          let unique = id;
          let n = 1;
          while (usedHeadingIds.has(unique)) unique = `${id}-${n++}`;
          usedHeadingIds.add(unique);
          node.properties = { ...node.properties, id: unique };
        }
      }
      // ```mermaid fences: remark-rehype emits <pre><code class="language-mermaid">.
      // Turn it into <pre class="mermaid"> so mermaid.run() picks it up.
      if (node.tagName === 'pre' && node.children?.length === 1) {
        const code = node.children[0];
        const cls = code?.properties?.className;
        const classes = Array.isArray(cls) ? cls : cls ? [cls] : [];
        if (code?.tagName === 'code' && classes.includes('language-mermaid')) {
          node.tagName = 'pre';
          node.properties = { className: ['mermaid'] };
          node.children = [{ type: 'text', value: textOf(code) }];
        }
      }
    });
  };
}

/** Render one doc's markdown to HTML, with doc-aware rewrites applied.
 *  A fresh processor per call keeps the per-doc `fromPath` rewrite isolated
 *  (SSG build — runs once per page, so the cost is irrelevant). */
export async function renderDoc(markdown: string, fromPath: string): Promise<string> {
  const file = await unified()
    .use(remarkParse)
    .use(remarkGfm)
    .use(remarkRehype, { allowDangerousHtml: true })
    .use(rehypeRaw)
    .use(rehypeDocRewrites, fromPath)
    .use(rehypeStringify, { allowDangerousHtml: true })
    .process(markdown);
  return String(file);
}
