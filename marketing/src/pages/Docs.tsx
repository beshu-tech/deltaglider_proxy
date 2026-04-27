import { useMemo, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import rehypeSlug from 'rehype-slug';
import remarkGfm from 'remark-gfm';
import { Link } from 'react-router-dom';
import {
  DOCS,
  DOC_GROUPS,
  GROUP_TAGLINE,
  findDocByFilename,
  type DocEntry,
} from '../docs-imports';
import { SEO } from '../components/SEO';
import type { PageMeta } from '../seo/pages';
import { SITE_URL, organization, website } from '../seo/schema';

interface DocsProps {
  initialDocId?: string;
}

function groupedDocs(): Map<string, DocEntry[]> {
  const map = new Map<string, DocEntry[]>();
  for (const group of DOC_GROUPS) map.set(group, []);
  for (const doc of DOCS) map.get(doc.group)?.push(doc);
  for (const [, docs] of map) docs.sort((a, b) => a.order - b.order);
  return map;
}

function docsPath(doc: DocEntry): string {
  return `/docs/${doc.id}/`;
}

function normalizeHref(href: string): string {
  if (href.startsWith('http://') || href.startsWith('https://') || href.startsWith('#')) {
    return href;
  }
  const doc = findDocByFilename(href);
  return doc ? docsPath(doc) : href;
}

function normalizeImageSrc(src: string): string {
  return src.startsWith('/_/screenshots/') ? src.replace('/_/', '/') : src;
}

export function Docs({ initialDocId }: DocsProps): JSX.Element {
  const grouped = useMemo(groupedDocs, []);
  const initialDoc = DOCS.find((doc) => doc.id === initialDocId) ?? DOCS[0];
  const [selectedId, setSelectedId] = useState(initialDoc?.id ?? '');
  const selectedDoc = DOCS.find((doc) => doc.id === selectedId) ?? initialDoc;

  if (!selectedDoc) {
    return (
      <section className="mx-auto max-w-6xl px-6 py-20">
        <h1 className="text-4xl font-black text-ink-900 dark:text-ink-50">Documentation unavailable</h1>
      </section>
    );
  }

  const meta: PageMeta = {
    path: docsPath(selectedDoc),
    title: `${selectedDoc.title} — DeltaGlider Proxy Docs`,
    description:
      'Product documentation for DeltaGlider Proxy: setup, IAM, encryption, replication, metrics, operations, and reference material.',
    ogImage: `${SITE_URL}/screenshots/filebrowser.jpg`,
    jsonLd: [organization(), website()],
  };

  return (
    <>
      <SEO meta={meta} />
      <section className="mx-auto max-w-6xl px-6 py-12 sm:py-16">
        <nav
          aria-label="Breadcrumb"
          className="mb-6 flex flex-wrap items-center gap-2 text-sm font-extrabold text-ink-500 dark:text-ink-400"
        >
          <Link to="/" className="hover:text-brand-700 dark:hover:text-brand-300">
            DeltaGlider
          </Link>
          <span aria-hidden>/</span>
          <Link to="/docs/" className="hover:text-brand-700 dark:hover:text-brand-300">
            Docs
          </Link>
          <span aria-hidden>/</span>
          <span className="text-brand-700 dark:text-brand-300">{selectedDoc.group}</span>
          <span aria-hidden>/</span>
          <span className="text-ink-900 dark:text-ink-100">{selectedDoc.title}</span>
        </nav>

        <div className="grid gap-8 md:grid-cols-4">
          <aside className="md:sticky md:top-24 md:col-span-1 md:self-start">
            <div className="rounded-3xl border border-ink-200 bg-white p-4 shadow-xl shadow-brand-950/5 dark:border-ink-700 dark:bg-ink-900/80">
              {Array.from(grouped.entries()).map(([group, docs]) => (
                <div key={group} className="mb-5 last:mb-0">
                  <div className="text-[11px] font-extrabold uppercase tracking-widest text-brand-700 dark:text-brand-300">
                    {group}
                  </div>
                  <p className="mt-1 text-xs leading-5 text-ink-500 dark:text-ink-400">
                    {GROUP_TAGLINE[group as keyof typeof GROUP_TAGLINE]}
                  </p>
                  <div className="mt-2 space-y-1">
                    {docs.map((doc) => (
                      <Link
                        key={doc.id}
                        to={docsPath(doc)}
                        onClick={() => setSelectedId(doc.id)}
                        className={`block rounded-xl px-3 py-2 text-sm font-bold transition ${
                          doc.id === selectedDoc.id
                            ? 'bg-brand-100 text-brand-900 dark:bg-brand-900/70 dark:text-brand-100'
                            : 'text-ink-700 hover:bg-ink-100 dark:text-ink-200 dark:hover:bg-ink-800'
                        }`}
                      >
                        {doc.title}
                      </Link>
                    ))}
                  </div>
                </div>
              ))}
            </div>
          </aside>

          <article className="docs-markdown min-w-0 rounded-[2rem] border border-ink-200 bg-white p-6 shadow-xl shadow-brand-950/5 dark:border-ink-700 dark:bg-ink-950/80 sm:p-9 md:col-span-3">
            <ReactMarkdown
              remarkPlugins={[remarkGfm]}
              rehypePlugins={[rehypeSlug, rehypeHighlight]}
              components={{
                a({ href = '', children, ...props }) {
                  const next = normalizeHref(href);
                  const external = next.startsWith('http://') || next.startsWith('https://');
                  if (external) {
                    return (
                      <a href={next} target="_blank" rel="noopener noreferrer" {...props}>
                        {children}
                      </a>
                    );
                  }
                  return (
                    <Link to={next} {...props}>
                      {children}
                    </Link>
                  );
                },
                img({ src = '', alt = '', ...props }) {
                  return (
                    <img
                      src={normalizeImageSrc(src)}
                      alt={alt}
                      className="rounded-2xl border border-ink-200 shadow-lg shadow-brand-950/10 dark:border-ink-700"
                      {...props}
                    />
                  );
                },
              }}
            >
              {selectedDoc.content}
            </ReactMarkdown>
          </article>
        </div>
      </section>
    </>
  );
}
