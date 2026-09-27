// llms.ts — the docs in the shapes an LLM (or an agent) reads best:
//   /llms.txt          the llmstxt.org index: title, summary, one link per doc
//   /llms-full.txt     every doc's markdown, concatenated in reading order
//   /docs/<slug>.md    one doc's markdown (/docs.md for the landing)
// All three are built from docs/product + manifest.json at build time. Links
// inside the markdown become absolute .md URLs on the site, so a pasted page
// still points at pages the model can fetch.

import { docsByGroup, rewriteDocLink, type DocMeta } from './docs';
import { docContent } from './docContent';
import { SITE } from './seo';

/** The URL of a doc's raw markdown. The landing (slug '') is /docs.md. */
export function markdownUrl(doc: Pick<DocMeta, 'url'>): string {
  return `${doc.url}.md`;
}

/**
 * One doc's markdown as served on the site. Inter-doc links resolve to the
 * absolute `.md` URL of the target, and screenshot paths to the site's copy.
 * Links that are not docs (external URLs, in-page anchors) stay as written.
 */
export function docMarkdown(path: string, site: string = SITE.url): string {
  const raw = docContent(path) ?? '';
  return raw
    .replace(/\]\(([^)\s]+)\)/g, (whole, href: string) => {
      const page = rewriteDocLink(href, path);
      if (!page) return whole;
      const hashAt = page.indexOf('#');
      const base = hashAt >= 0 ? page.slice(0, hashAt) : page;
      const hash = hashAt >= 0 ? page.slice(hashAt) : '';
      return `](${site}${base}.md${hash})`;
    })
    .replace(/\(\/_\/screenshots\//g, `(${site}/screenshots/`);
}

const SUMMARY =
  'DeltaGlider Proxy is an S3-compatible proxy in front of existing storage. ' +
  'It stores similar files (versioned binaries, backups, builds) as xdelta3 deltas ' +
  'of a reference file, routes buckets across backends, and has an admin UI for IAM, ' +
  'SSO, replication, lifecycle, encryption, and events. Clients use the standard S3 API unchanged.';

/** /llms.txt — https://llmstxt.org format. Every manifest entry appears once. */
export function buildLlmsTxt(site: string = SITE.url): string {
  const out: string[] = [
    '# DeltaGlider Proxy',
    '',
    `> ${SUMMARY}`,
    '',
    'Each link below is the page as plain markdown. The same pages render as HTML ' +
      `at the URL without the \`.md\` suffix. All pages in one file: ${site}/llms-full.txt`,
    '',
  ];
  for (const { group, docs } of docsByGroup()) {
    // The release notes are long and rarely needed to answer a question, so
    // they go under the spec's "Optional" heading, which a reader may skip.
    const heading = group.id === 'Releases' ? 'Optional' : group.id;
    out.push(`## ${heading}`, '', group.tagline, '');
    for (const d of docs) {
      const note = d.summary ? `: ${d.summary}` : '';
      out.push(`- [${d.title}](${site}${markdownUrl(d)})${note}`);
    }
    out.push('');
  }
  return out.join('\n');
}

/** /llms-full.txt — every doc, in the sidebar reading order, each under a
 *  header that names its title and canonical URL. */
export function buildLlmsFull(site: string = SITE.url): string {
  const parts: string[] = [`# DeltaGlider Proxy documentation\n\n> ${SUMMARY}\n`];
  for (const { docs } of docsByGroup()) {
    for (const d of docs) {
      parts.push(
        [
          '---',
          '',
          `<!-- ${d.title} · ${site}${d.url} · markdown: ${site}${markdownUrl(d)} -->`,
          '',
          docMarkdown(d.path, site).trim(),
          '',
        ].join('\n'),
      );
    }
  }
  return parts.join('\n');
}
