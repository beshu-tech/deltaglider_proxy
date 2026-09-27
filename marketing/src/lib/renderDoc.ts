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
import { highlightCode } from './highlight';
export { extractTitle, extractSummary } from './docText';

/**
 * GitHub-compatible heading slug (the github-slugger rule that rehype-slug
 * applies in the product viewer): lower-case, drop punctuation, and turn EACH
 * space into a dash. Runs of spaces are NOT collapsed: "Server / Advanced"
 * is "server--advanced", which is the anchor the markdown links use.
 */
export function slugifyHeading(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^\w\s-]/g, '')
    .trim()
    .replace(/\s/g, '-');
}

/** Collect the visible text of a hast element (for heading ids). */
function textOf(node: any): string {
  if (node.type === 'text') return node.value;
  if (node.children) return node.children.map(textOf).join('');
  return '';
}

/**
 * A changelog version heading: `v<X.Y.Z[suffix]>` then an optional
 * ` — <YYYY-MM-DD>` date then an optional ` — <title>`. Capture groups:
 * [1] version, [2] date, [3] title. Matches all three real shapes
 * (version only / version+date / version+date+title) and skips ordinary
 * headings — even "Fixed — CI" (no `v` prefix). Verified against the full
 * CHANGELOG.md corpus.
 */
export const VERSION_HEADING_RE = /^(v\d+\.\d+\.\d+\S*)(?:\s*[—-]\s*(\d{4}-\d{2}-\d{2}))?(?:\s*[—-]\s*(.+))?$/;

/**
 * rehype plugin: rewrite links/images/headings/mermaid for one doc.
 * `fromPath` is the docs/product path of the doc being rendered, needed to
 * resolve relative `../` inter-doc links correctly.
 */
/** Read the `language-xxx` class off a hast <code> node, or '' if none. */
function fenceLang(codeNode: any): string {
  const cls = codeNode?.properties?.className;
  const classes = Array.isArray(cls) ? cls : cls ? [cls] : [];
  const m = classes.find((c: string) => typeof c === 'string' && c.startsWith('language-'));
  return m ? m.slice('language-'.length) : '';
}

function rehypeDocRewrites(fromPath: string) {
  // Async transformer: the link/image/heading rewrites are sync, but code-fence
  // syntax highlighting (shiki) is async, so we collect fence nodes during the
  // sync visit and resolve them afterwards, replacing each <pre> with shiki's
  // pre-highlighted HTML (carried as a `raw` node that rehype-stringify emits
  // verbatim).
  return async (tree: any) => {
    const usedHeadingIds = new Set<string>();
    const fences: { node: any; code: string; lang: string }[] = [];

    visit(tree, 'element', (node: any) => {
      // Changelog version headings ("v1.4.3 — 2026-06-18"): tuck the
      // release date into a span we can reveal on hover via CSS, and tag
      // the h2 so it lays out version + date on one baseline. Runs BEFORE
      // the heading-id logic below, which still slugifies the full text
      // (textOf) so anchors stay stable. Non-version headings untouched.
      if (node.tagName === 'h2') {
        const m = textOf(node).trim().match(VERSION_HEADING_RE);
        if (m) {
          const [, ver, date, title] = m;
          node.properties = { ...node.properties, className: ['cl-version'] };
          node.children = [
            { type: 'element', tagName: 'span', properties: { className: ['cl-ver'] }, children: [{ type: 'text', value: ver }] },
            ...(title ? [{ type: 'element', tagName: 'span', properties: { className: ['cl-title'] }, children: [{ type: 'text', value: title }] }] : []),
            ...(date ? [{ type: 'element', tagName: 'span', properties: { className: ['cl-date'] }, children: [{ type: 'text', value: date }] }] : []),
          ];
        }
      }
      // "Last updated: …" paragraph → a top-right badge.
      if (node.tagName === 'p' && /^last updated:/i.test(textOf(node).trim())) {
        node.properties = { ...node.properties, className: ['cl-updated'] };
      }
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
          // Append a REAL clickable anchor so the hover "#" actually navigates
          // (sets the URL hash) and is copyable / keyboard-focusable — the
          // previous "#" was a CSS ::after decoration with nothing to click.
          // h2/h3/h4 only (h1 is the page title; h5/h6 are rare and unlinked).
          if (/^h[234]$/.test(node.tagName)) {
            node.children = [
              ...(node.children ?? []),
              {
                type: 'element',
                tagName: 'a',
                properties: {
                  className: ['docs-heading-anchor'],
                  href: `#${unique}`,
                  'aria-label': `Link to this section: ${textOf(node)}`,
                },
                children: [{ type: 'text', value: '#' }],
              },
            ];
          }
        }
      }
      // Code fences (<pre><code class="language-…">).
      if (node.tagName === 'pre' && node.children?.length === 1) {
        const code = node.children[0];
        if (code?.tagName !== 'code') return;
        const lang = fenceLang(code);
        // ```mermaid → <pre class="mermaid"> for client-side rendering.
        if (lang === 'mermaid') {
          node.tagName = 'pre';
          node.properties = { className: ['mermaid'] };
          node.children = [{ type: 'text', value: textOf(code) }];
          return;
        }
        // Everything else: queue for shiki highlighting.
        fences.push({ node, code: textOf(code), lang });
      }
    });

    if (fences.length) {
      await Promise.all(
        fences.map(async (f) => {
          const shikiHtml = await highlightCode(f.code, f.lang);
          // Replace the <pre><code> node in-place with shiki's output. We hang a
          // `raw` child off a now-empty wrapper and let rehype-stringify
          // (allowDangerousHtml) emit it verbatim. Wrapping in a fragment-like
          // <div> would add a box; instead we morph the node into a raw passthrough.
          f.node.tagName = 'div';
          f.node.properties = { className: ['shiki-wrap'], 'data-lang': f.lang || 'text' };
          f.node.children = [{ type: 'raw', value: shikiHtml }];
        }),
      );
    }
  };
}

export interface TocEntry {
  depth: 2 | 3;
  id: string;
  text: string;
  /** The label as HTML: plain text with the heading's `<code>` spans kept, so
   *  a TOC entry like `--init` renders in the mono face, not as a dash. */
  html: string;
}

/**
 * Build a table-of-contents from rendered doc HTML. Reads h2/h3 (they already
 * carry stable ids from rehypeDocRewrites) in document order. Server-side only
 * — runs once per page at build time. h1 (the doc title) and h4+ (too granular
 * for a right rail) are skipped.
 */
export function extractToc(html: string): TocEntry[] {
  const out: TocEntry[] = [];
  const re = /<h([23])\b([^>]*)>([\s\S]*?)<\/h\1>/gi;
  let m: RegExpExecArray | null;
  while ((m = re.exec(html)) !== null) {
    const depth = Number(m[1]) as 2 | 3;
    const idMatch = /\bid=["']([^"']+)["']/.exec(m[2]);
    if (!idMatch) continue;
    // Drop the appended heading-anchor (<a class="docs-heading-anchor">#</a>)
    // before reading the label, or every TOC entry would end in a stray "#".
    const inner = m[3].replace(/<a\b[^>]*class=["'][^"']*docs-heading-anchor[^"']*["'][^>]*>[\s\S]*?<\/a>/i, '');
    // Changelog version headings render version/date/title as separate spans;
    // the date span is hover-only on the page and unwanted in the ToC. Read
    // the version (+ title) spans directly — stripping all tags would mash
    // the adjacent spans into "v1.4.32026-06-18". `clVerTitle` returns null
    // for ordinary headings, so they fall through to plain stripTags.
    const cl = clVerTitle(inner);
    const text = cl ?? stripTags(inner).trim();
    const html = cl
      ? escapeHtml(cl)
      : inner.replace(/<(?!\/?code\b)[^>]+>/g, '').replace(/\s+/g, ' ').trim();
    if (text) out.push({ depth, id: idMatch[1], text, html });
  }
  return out;
}

/** ToC label for a changelog version heading: the `.cl-ver` (+ `.cl-title`)
 *  span text, never the hover-only `.cl-date`. Returns null when the heading
 *  has no `.cl-ver` span (ordinary headings), so callers fall back to plain
 *  text. Reading the spans avoids re-parsing the version out of mashed text. */
function clVerTitle(headingInner: string): string | null {
  const span = (cls: string) =>
    new RegExp(`<span[^>]*class=["'][^"']*\\b${cls}\\b[^"']*["'][^>]*>([\\s\\S]*?)</span>`, 'i').exec(headingInner);
  const ver = span('cl-ver');
  if (!ver) return null;
  const title = span('cl-title');
  return [stripTags(ver[1]).trim(), title && stripTags(title[1]).trim()].filter(Boolean).join(' — ');
}

function escapeHtml(text: string): string {
  return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

/** Strip inline tags + decode the entities that appear in heading text,
 *  including numeric (&#38;) and hex (&#x26;) forms that the markdown pipeline
 *  emits for characters like &. */
function stripTags(html: string): string {
  return html
    .replace(/<[^>]+>/g, '')
    .replace(/&#x([0-9a-fA-F]+);/g, (_, h) => String.fromCodePoint(parseInt(h, 16)))
    .replace(/&#(\d+);/g, (_, d) => String.fromCodePoint(parseInt(d, 10)))
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/\s+/g, ' ');
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
