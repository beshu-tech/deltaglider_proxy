/**
 * The in-product docs markdown pipeline, shared by DocsPage (which renders it)
 * and the anchor test (which checks every "#x" / "page.md#x" link against the
 * ids this exact pipeline produces). Pure: no React components, no fetch.
 *
 * rehype-slug uses github-slugger, the rule GitHub and the website renderer
 * (marketing/src/lib/renderDoc.ts) follow, so the anchors written in
 * docs/product resolve on all three.
 */
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import rehypeSlug from 'rehype-slug';

export const DOC_REMARK_PLUGINS = [remarkGfm];
export const DOC_REHYPE_PLUGINS = [rehypeHighlight, rehypeSlug];

export interface DocSegment {
  type: 'text' | 'mermaid';
  content: string;
  /** For a diagram: the last heading before it, used as the lightbox caption. */
  caption?: string;
}

/** Split markdown into text segments and mermaid code blocks. */
export function splitMermaid(md: string): DocSegment[] {
  const segments: DocSegment[] = [];
  const regex = /```mermaid\n([\s\S]*?)```/g;
  let lastIndex = 0;
  let match;
  while ((match = regex.exec(md)) !== null) {
    const textBefore = md.slice(lastIndex, match.index);
    if (textBefore) segments.push({ type: 'text', content: textBefore });
    const lines = textBefore.trim().split('\n');
    const lastHeading = [...lines].reverse().find((l) => /^#{2,4}\s/.test(l));
    const caption = lastHeading?.replace(/^#+\s+/, '').trim();
    const mermaidContent = match[1].trim();
    if (mermaidContent) segments.push({ type: 'mermaid', content: mermaidContent, caption });
    lastIndex = match.index + match[0].length;
  }
  if (lastIndex < md.length) segments.push({ type: 'text', content: md.slice(lastIndex) });
  return segments;
}

/** What a link in a doc points at. */
export type DocHref =
  | { kind: 'external'; href: string }
  | { kind: 'anchor'; anchor: string }
  | { kind: 'doc'; file: string; anchor: string };

/**
 * Classify a link as written in the markdown: another doc (`page.md`,
 * `../x/page.md#section`), a heading on this page (`#section`), or anything
 * else (a URL, an asset). `anchor` is decoded and '' when absent.
 */
export function classifyDocHref(href: string): DocHref {
  if (href.startsWith('#')) return { kind: 'anchor', anchor: decodeURIComponent(href.slice(1)) };
  if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith('/')) return { kind: 'external', href };
  const hashAt = href.indexOf('#');
  const file = hashAt >= 0 ? href.slice(0, hashAt) : href;
  if (!file.endsWith('.md')) return { kind: 'external', href };
  return { kind: 'doc', file, anchor: hashAt >= 0 ? decodeURIComponent(href.slice(hashAt + 1)) : '' };
}
