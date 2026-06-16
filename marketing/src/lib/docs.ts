// docs.ts — the website's docs model, derived from the SAME shared manifest
// the in-product docs viewer reads (docs/product/manifest.json). This is what
// keeps the website docs in lockstep with the product: one content source
// (docs/product/**/*.md), one ordering source (the manifest).
//
// The product viewer serves docs at flat ids under /_/docs/:id. The website
// uses friendly paths under /docs/ — numeric ordering prefixes (01-, 30-)
// stripped, subfolders preserved. Both render byte-identical markdown.

import manifest from '../../../docs/product/manifest.json';
import { docContent } from './docContent';
import { extractTitle, extractSummary } from './docText';

export interface DocGroup {
  id: string;
  tagline: string;
}

export interface DocMeta {
  /** Path under docs/product/, no extension. e.g. "auth/30-oauth-setup". */
  path: string;
  /** Friendly URL slug under /docs/. e.g. "auth/oauth-setup". */
  slug: string;
  /** Full site path. e.g. "/docs/auth/oauth-setup". */
  url: string;
  /** First `# heading` of the doc — the human title shown in nav/links. */
  title: string;
  /** One-line summary (first prose sentence) — for landing card sub-lines. */
  summary: string;
  group: string;
  order: number;
}

export const DOC_GROUPS: DocGroup[] = manifest.groups;

/**
 * Strip a leading numeric ordering prefix from a single path segment.
 * "01-quickstart" -> "quickstart"; "auth/30-oauth-setup" -> "auth/oauth-setup".
 * Each segment is treated independently so subfolders are preserved.
 * "README" -> "" (the landing index — see slugToUrl).
 */
export function pathToSlug(path: string): string {
  if (path === 'README') return '';
  return path
    .split('/')
    .map((seg) => seg.replace(/^\d+-/, ''))
    .join('/');
}

function slugToUrl(slug: string): string {
  return slug === '' ? '/docs' : `/docs/${slug}`;
}

/** All docs, manifest-ordered, with friendly slugs/urls/titles resolved. */
export const DOCS: DocMeta[] = manifest.docs.map((d) => {
  const slug = pathToSlug(d.path);
  const content = docContent(d.path);
  const title = content ? extractTitle(content) : d.path;
  const summary = content ? extractSummary(content, 90) : '';
  return { path: d.path, slug, url: slugToUrl(slug), title, summary, group: d.group, order: d.order };
});

/** Docs grouped + sorted exactly as the product viewer groups them. */
export function docsByGroup(): { group: DocGroup; docs: DocMeta[] }[] {
  return DOC_GROUPS.map((group) => ({
    group,
    docs: DOCS.filter((d) => d.group === group.id).sort((a, b) => a.order - b.order),
  })).filter((g) => g.docs.length > 0);
}

const PATH_BY_SLUG = new Map(DOCS.map((d) => [d.slug, d.path]));
const META_BY_PATH = new Map(DOCS.map((d) => [d.path, d]));

/** Resolve a friendly slug (from the URL) back to its docs/product path. */
export function pathForSlug(slug: string): string | undefined {
  return PATH_BY_SLUG.get(slug);
}

export function metaForPath(path: string): DocMeta | undefined {
  return META_BY_PATH.get(path);
}

/**
 * Rewrite a Markdown-relative inter-doc link target (as authored in the .md,
 * e.g. "../reference/authentication.md#iam-permissions-abac") into the
 * website's friendly URL ("/docs/reference/authentication#iam-permissions-abac").
 *
 * `fromPath` is the docs/product path of the doc CONTAINING the link, used to
 * resolve `../` correctly. Returns null if the target isn't a known doc — the
 * caller should leave the original href untouched (external link, anchor, etc.).
 */
export function rewriteDocLink(href: string, fromPath: string): string | null {
  // Leave absolute URLs, anchors, mailto, and non-.md links alone.
  if (/^[a-z]+:/i.test(href) || href.startsWith('#') || href.startsWith('/')) return null;
  const hashIdx = href.indexOf('#');
  const hash = hashIdx >= 0 ? href.slice(hashIdx) : '';
  let target = hashIdx >= 0 ? href.slice(0, hashIdx) : href;
  if (!target.endsWith('.md')) return null;
  target = target.slice(0, -3); // drop .md

  // Resolve relative to the containing doc's DIRECTORY.
  const fromDir = fromPath.includes('/') ? fromPath.slice(0, fromPath.lastIndexOf('/')) : '';
  const segments = (fromDir ? fromDir.split('/') : []).concat(target.split('/'));
  const resolved: string[] = [];
  for (const seg of segments) {
    if (seg === '.' || seg === '') continue;
    if (seg === '..') resolved.pop();
    else resolved.push(seg);
  }
  const resolvedPath = resolved.join('/');

  const meta = META_BY_PATH.get(resolvedPath);
  if (!meta) return null;
  return `${meta.url}${hash}`;
}

/** Rewrite product-server asset paths (/_/screenshots/x.jpg) to the website's
 *  served location (/screenshots/x.jpg). The same image files live in both. */
export function rewriteAssetSrc(src: string): string {
  return src.replace(/^\/_\/screenshots\//, '/screenshots/');
}

/**
 * The docs in *sidebar reading order* — group order (DOC_GROUPS) first, then
 * `order` within each group. This is the order a reader walks the docs, so it's
 * the spine for the prev/next pager. Excludes the README landing ('').
 */
export function docsInReadingOrder(): DocMeta[] {
  return docsByGroup().flatMap((g) => g.docs).filter((d) => d.slug !== '');
}

/**
 * Previous/next doc around a given slug, in reading order. Either side is null
 * at the ends of the spine (first doc has no prev, last has no next).
 */
export function adjacentDocs(slug: string): { prev: DocMeta | null; next: DocMeta | null } {
  const spine = docsInReadingOrder();
  const i = spine.findIndex((d) => d.slug === slug);
  if (i === -1) return { prev: null, next: null };
  return {
    prev: i > 0 ? spine[i - 1] : null,
    next: i < spine.length - 1 ? spine[i + 1] : null,
  };
}
