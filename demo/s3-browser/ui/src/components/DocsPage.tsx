import { useState, useEffect, useRef, useMemo, useCallback, type ReactNode } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import mermaid from 'mermaid';
import { Button, Drawer, Spin } from 'antd';
import { CheckOutlined, CopyOutlined, MenuOutlined } from '@ant-design/icons';
import { findDocByFilename, type DocEntry } from '../docsBundle';
import { DOC_REHYPE_PLUGINS, DOC_REMARK_PLUGINS, classifyDocHref, splitMermaid } from '../docsMarkdown';
import { useCopyToClipboard } from '../useCopyToClipboard';
import { useIsNarrow } from '../useIsNarrow';
import { useDocs } from '../queries/docs';
import { useColors, useTheme } from '../ThemeContext';
import FullScreenHeader from './FullScreenHeader';
import DocSearch from './DocSearch';
import Lightbox from './Lightbox';
import { useNavigation } from '../NavigationContext';
import DocsLanding from './DocsLanding';
import '../docs.css';

mermaid.initialize({
  startOnLoad: false,
  securityLevel: 'strict',
  flowchart: { useMaxWidth: false },
  sequence: { useMaxWidth: false },
});

/** Self-contained Mermaid diagram React component.
 * After render, measures the actual content bbox and rewrites the viewBox
 * to fit tightly — Mermaid's default viewBox is often 2-3x larger than the content. */
function Mermaid({ chart, caption }: { chart: string; caption?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [svg, setSvg] = useState('');
  const { isDark } = useTheme();
  const { ACCENT_BLUE, TEXT_FAINT } = useColors();

  useEffect(() => {
    let cancelled = false;
    const id = `mermaid-${Math.random().toString(36).slice(2, 8)}`;
    // Re-apply theme-aware variables before each render so diagrams track
    // the active light/dark theme instead of a frozen palette.
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: 'strict',
      theme: isDark ? 'dark' : 'default',
      themeVariables: { primaryColor: ACCENT_BLUE, lineColor: TEXT_FAINT },
      flowchart: { useMaxWidth: false },
      sequence: { useMaxWidth: false },
    });
    mermaid.render(id, chart).then(({ svg: rendered }) => {
      if (!cancelled) setSvg(rendered);
    }).catch(console.warn);
    return () => { cancelled = true; };
  }, [chart, isDark, ACCENT_BLUE, TEXT_FAINT]);

  // After SVG is in the DOM, try to tighten the viewBox to fit the
  // drawn content — Mermaid's default is often 2-3× too tall, which
  // wastes vertical space.
  //
  // CAVEAT (fixed 2026-04-22): `getBBox()` on a Mermaid subgraph
  // root element returns a bounding box tens-of-thousands of pixels
  // wide — an internal Mermaid quirk where each subgraph's `<g>`
  // has invisible label-layout elements far outside its visible
  // extent. Rewriting the SVG's `viewBox` + `width` with that number
  // produced ~17,000-px wide SVGs that rendered as two tiny squares
  // at opposite edges of the container.
  //
  // The safe path: trust Mermaid's initial values. They're derived
  // from the actual layout and set `style="max-width: 100%"`, which
  // already handles responsive sizing. We only rewrite when the
  // measured bbox is strictly smaller than the initial viewBox
  // (the tightening case we wanted) AND stays within a sane ratio.
  useEffect(() => {
    if (!svg || !ref.current) return;
    const svgEl = ref.current.querySelector('svg');
    if (!svgEl) return;

    // Preserve the initial viewBox as the authoritative size from
    // Mermaid. We compare against this to detect getBBox-blowout.
    const initialViewBox = svgEl.getAttribute('viewBox');
    if (!initialViewBox) return;
    const parts = initialViewBox.split(/\s+/).map(Number);
    if (parts.length !== 4 || parts.some((n) => !Number.isFinite(n))) return;
    const [, , initW, initH] = parts;

    try {
      const bb = svgEl.getBBox();
      // Guard against the subgraph-bbox blowout. If either dimension
      // is >1.5× the initial viewBox, the bbox is bogus — leave the
      // initial values in place.
      if (
        !Number.isFinite(bb.width) ||
        !Number.isFinite(bb.height) ||
        bb.width <= 0 ||
        bb.height <= 0 ||
        bb.width > initW * 1.5 ||
        bb.height > initH * 1.5
      ) {
        return;
      }
      const pad = 16;
      const w = Math.ceil(bb.width + pad * 2);
      const h = Math.ceil(bb.height + pad * 2);
      svgEl.setAttribute('viewBox', `${bb.x - pad} ${bb.y - pad} ${w} ${h}`);
      svgEl.setAttribute('width', String(w));
      svgEl.setAttribute('height', String(h));
      svgEl.style.maxWidth = '100%';
      svgEl.style.height = 'auto';
    } catch {
      // getBBox can fail if SVG is not visible — fine, keep Mermaid's defaults.
    }
  }, [svg]);
  return (
    <Lightbox caption={caption}>
      <div ref={ref} className="mermaid-diagram" dangerouslySetInnerHTML={{ __html: svg }} />
    </Lightbox>
  );
}


/** One "On this page" entry, read from the rendered heading. */
interface TocItem {
  id: string;
  level: 2 | 3;
  /** Heading text in runs; `code` runs render mono, as in the heading. */
  parts: { text: string; code: boolean }[];
}

/**
 * A changelog version heading: `v<X.Y.Z[suffix]>` then an optional
 * ` — <YYYY-MM-DD>` date then an optional ` — <title>`. Capture groups:
 * [1] version, [2] date, [3] title. Matches all three real shapes
 * (version only / version+date / version+date+title) and skips ordinary
 * headings — even "Fixed — CI" (no `v` prefix). Mirrors VERSION_HEADING_RE
 * in marketing/src/lib/renderDoc.ts; verified against the full CHANGELOG corpus.
 */
const VERSION_HEADING_RE = /^(v\d+\.\d+\.\d+\S*)(?:\s*[—-]\s*(\d{4}-\d{2}-\d{2}))?(?:\s*[—-]\s*(.+))?$/;

/**
 * Split a changelog version heading into version / hover-date / title.
 * Returns null for any non-version heading so the caller renders it
 * untouched. Tolerates ReactMarkdown passing children as a string or array.
 */
function splitVersionHeading(children: ReactNode): { version: string; date?: string; title?: string } | null {
  const m = nodeToText(children).trim().match(VERSION_HEADING_RE);
  if (!m) return null;
  return { version: m[1], date: m[2], title: m[3] };
}

/** Flatten ReactMarkdown heading children to plain text. */
function nodeToText(node: ReactNode): string {
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(nodeToText).join('');
  return '';
}

/**
 * The TOC comes from the rendered h2/h3 elements, so every entry carries the
 * id that rehype-slug really gave the heading (the old markdown-regex TOC
 * guessed ids and broke on `_`, `[`, `'` and "—"). A changelog version
 * heading shows its version and title, never the hover-only date.
 */
function readHeadings(root: HTMLElement): TocItem[] {
  const items: TocItem[] = [];
  root.querySelectorAll<HTMLHeadingElement>('article h2[id], article h3[id]').forEach((h) => {
    const level = h.tagName === 'H2' ? 2 : 3;
    const ver = h.querySelector('.cl-ver');
    if (ver) {
      const title = h.querySelector('.cl-title')?.textContent;
      items.push({ id: h.id, level, parts: [{ text: [ver.textContent, title].filter(Boolean).join(' — '), code: false }] });
      return;
    }
    const parts: TocItem['parts'] = [];
    h.childNodes.forEach((n) => {
      const text = n.textContent ?? '';
      if (text) parts.push({ text, code: n.nodeName === 'CODE' });
    });
    if (parts.length) items.push({ id: h.id, level, parts });
  });
  return items;
}

function TocLabel({ parts }: { parts: TocItem['parts'] }) {
  return <>{parts.map((p, i) => (p.code ? <code key={i}>{p.text}</code> : <span key={i}>{p.text}</span>))}</>;
}

/** A fenced code block with a copy button (the website has the same). */
function CodeBlock({ children, ...props }: React.HTMLAttributes<HTMLPreElement>) {
  const ref = useRef<HTMLPreElement>(null);
  const { copy, copied } = useCopyToClipboard();
  return (
    <div className="docs-pre-wrap">
      <pre ref={ref} {...props}>{children}</pre>
      <button
        type="button"
        className={`docs-copy-btn${copied ? ' is-copied' : ''}`}
        aria-label="Copy the code to the clipboard"
        title="Copy the code"
        onClick={() => { void copy(ref.current?.innerText ?? '', { successMessage: 'Code copied' }); }}
      >
        {copied ? <CheckOutlined aria-hidden="true" /> : <CopyOutlined aria-hidden="true" />}
        <span>{copied ? 'Copied' : 'Copy'}</span>
      </button>
    </div>
  );
}

/** Copies the doc's markdown source, for pasting into an LLM chat. */
function CopyPageButton({ doc }: { doc: DocEntry }) {
  const { copy, copied } = useCopyToClipboard();
  return (
    <Button
      size="small"
      icon={copied ? <CheckOutlined /> : <CopyOutlined />}
      title="Copy this page as Markdown, to paste into an LLM"
      onClick={() => {
        void copy(doc.content, {
          successMessage: 'Page copied as Markdown',
          fallbackFilename: doc.filename.slice(doc.filename.lastIndexOf('/') + 1),
          fallbackMimeType: 'text/markdown',
        });
      }}
    >
      Copy page
    </Button>
  );
}

interface DocsNavProps {
  docs: readonly DocEntry[];
  grouped: Map<string, DocEntry[]>;
  selectedId: string;
  onSelect: (id: string) => void;
}

/** Search + the grouped doc list: the sidebar on desktop, the drawer on a phone. */
function DocsNav({ docs, grouped, selectedId, onSelect }: DocsNavProps) {
  const colors = useColors();
  return (
    <>
      <DocSearch docs={docs} onSelect={onSelect} />
      <div style={{ padding: '0 0 16px', flex: 1, overflowY: 'auto' }}>
        {Array.from(grouped.entries()).map(([group, groupDocs]) => (
          <div key={group} style={{ marginBottom: 16 }}>
            <div style={{
              padding: '4px 16px',
              fontSize: 10,
              fontWeight: 700,
              textTransform: 'uppercase',
              letterSpacing: 1.5,
              color: colors.TEXT_FAINT,
              fontFamily: 'var(--font-ui)',
            }}>
              {group}
            </div>
            {groupDocs.map((doc) => (
              <button
                key={doc.id}
                className="btn-reset"
                aria-current={doc.id === selectedId ? 'page' : undefined}
                onClick={() => onSelect(doc.id)}
                style={{
                  display: 'block',
                  width: '100%',
                  textAlign: 'left',
                  padding: '6px 16px 6px 20px',
                  fontSize: 13,
                  fontFamily: 'var(--font-ui)',
                  color: doc.id === selectedId ? colors.ACCENT_BLUE : colors.TEXT_SECONDARY,
                  background: doc.id === selectedId ? `${colors.ACCENT_BLUE}10` : 'transparent',
                  borderLeft: doc.id === selectedId ? `2px solid ${colors.ACCENT_BLUE}` : '2px solid transparent',
                  cursor: 'pointer',
                  transition: 'all 0.15s',
                }}
                onMouseEnter={(e) => {
                  if (doc.id !== selectedId) e.currentTarget.style.color = colors.TEXT_PRIMARY;
                }}
                onMouseLeave={(e) => {
                  if (doc.id !== selectedId) e.currentTarget.style.color = colors.TEXT_SECONDARY;
                }}
              >
                {doc.title}
              </button>
            ))}
          </div>
        ))}
      </div>
    </>
  );
}

interface Props {
  /** Doc ID from URL path (e.g., 'configuration' from /_/docs/configuration) */
  docId?: string;
  onBack?: () => void;
  accountMenu?: ReactNode;
  /** Open the keyboard-shortcuts help modal (header help icon). */
  onShowShortcuts?: () => void;
}

const EMPTY_DOCS: DocEntry[] = [];
const EMPTY_GROUPS: readonly string[] = [];

export default function DocsPage({ docId, onBack, accountMenu, onShowShortcuts }: Props) {
  const colors = useColors();
  const { navigate } = useNavigation();
  // Same breakpoint as the landing and `hide-mobile`: below it the sidebar
  // and the right-rail TOC give way to a drawer and an in-flow TOC.
  const narrow = useIsNarrow(769);
  const [navOpen, setNavOpen] = useState(false);

  // Docs arrive at runtime from the session-gated /_/api/docs (see
  // docsBundle.ts for why they are not in the bundle). Until they land,
  // DOCS is empty and the page shows a spinner instead of the landing.
  const { data: bundle, error: docsError } = useDocs();
  const DOCS = bundle?.docs ?? EMPTY_DOCS;
  const DOC_GROUPS = bundle?.groups ?? EMPTY_GROUPS;

  // Resolve doc ID: URL-driven if provided, else default to first doc
  const resolvedId = (docId && DOCS.some(d => d.id === docId)) ? docId : DOCS[0]?.id || '';
  const [selectedId, setSelectedIdState] = useState(resolvedId);

  // Sync selectedId when URL changes (browser back/forward).
  // When navigating back to the docs landing (no docId), restore the default
  // instead of keeping the last doc visible.
  useEffect(() => {
    if (docId && DOCS.some(d => d.id === docId)) {
      setSelectedIdState(docId);
    } else if (!docId) {
      setSelectedIdState(DOCS[0]?.id || '');
    }
  }, [docId, DOCS]);

  // A heading to scroll to once the next doc has rendered: from a
  // "page.md#section" link, or from the URL hash on first load.
  const pendingAnchor = useRef(typeof window !== 'undefined' ? decodeURIComponent(window.location.hash.slice(1)) : '');

  // Navigate + update state when user selects a doc
  const setSelectedId = useCallback((id: string) => {
    setSelectedIdState(id);
    setNavOpen(false);
    navigate(`docs/${id}`);
  }, [navigate]);
  const [activeHeading, setActiveHeading] = useState('');
  const [headings, setHeadings] = useState<TocItem[]>([]);
  const contentRef = useRef<HTMLDivElement>(null);

  const selectedDoc = useMemo(() => DOCS.find(d => d.id === selectedId), [selectedId, DOCS]);
  const showLanding = !!bundle && (selectedId === 'readme' || !selectedDoc);

  // Scroll only the content pane. scrollIntoView would also scroll every
  // scrollable ancestor, and pushed the page header out of view.
  const scrollToHeading = useCallback((id: string, smooth = true) => {
    const pane = contentRef.current;
    const el = id && pane ? pane.querySelector(`#${CSS.escape(id)}`) : null;
    if (!pane || !el) return false;
    const top = pane.scrollTop + el.getBoundingClientRect().top - pane.getBoundingClientRect().top - 16;
    pane.scrollTo({ top, behavior: smooth ? 'smooth' : 'auto' });
    setActiveHeading(id);
    return true;
  }, []);

  // New doc: scroll to top, read its headings, then honour a pending anchor.
  useEffect(() => {
    const root = contentRef.current;
    root?.scrollTo(0, 0);
    setActiveHeading('');
    setHeadings(root && selectedDoc && !showLanding ? readHeadings(root) : []);
    const anchor = pendingAnchor.current;
    if (anchor && selectedDoc) {
      pendingAnchor.current = '';
      // After layout, so the offset is measured on the new doc.
      const frame = requestAnimationFrame(() => { scrollToHeading(anchor, false); });
      return () => cancelAnimationFrame(frame);
    }
  }, [selectedDoc, showLanding, scrollToHeading]);

  // Active heading = the last one scrolled past the top band of the content
  // pane. A scroll listener, not an IntersectionObserver: the observer only
  // reports headings that cross the band, so a long section left the
  // highlight on whatever crossed last (on load, a heading far down the page).
  useEffect(() => {
    const el = contentRef.current;
    if (!el || headings.length === 0) return;
    let frame = 0;
    const update = () => {
      frame = 0;
      const top = el.getBoundingClientRect().top + 96;
      let current = '';
      for (const h of headings) {
        const node = el.querySelector(`#${CSS.escape(h.id)}`);
        if (node && node.getBoundingClientRect().top <= top) current = h.id;
        else if (node) break;
      }
      setActiveHeading(current);
    };
    const onScroll = () => { if (!frame) frame = requestAnimationFrame(update); };
    update();
    el.addEventListener('scroll', onScroll, { passive: true });
    return () => {
      el.removeEventListener('scroll', onScroll);
      if (frame) cancelAnimationFrame(frame);
    };
  }, [headings]);

  // A link inside a doc: another doc (optionally at a heading), a heading on
  // this page, or a normal URL.
  const handleDocLink = useCallback((file: string, anchor: string) => {
    const doc = findDocByFilename(DOCS, file, selectedDoc?.filename);
    if (!doc) return false;
    if (doc.id === selectedId) {
      scrollToHeading(anchor);
    } else {
      pendingAnchor.current = anchor;
      setSelectedId(doc.id);
    }
    return true;
  }, [DOCS, selectedDoc, selectedId, setSelectedId, scrollToHeading]);

  // Group docs by category, sort within each group by the `order`
  // field (stable across title edits).
  const grouped = useMemo(() => {
    const map = new Map<string, DocEntry[]>();
    for (const g of DOC_GROUPS) map.set(g, []);
    for (const d of DOCS) {
      const list = map.get(d.group);
      if (list) list.push(d);
    }
    for (const [, docs] of map) docs.sort((a, b) => a.order - b.order);
    return map;
  }, [DOCS, DOC_GROUPS]);

  const tocLinks = headings.map((h) => (
    <a
      key={h.id}
      href={`#${h.id}`}
      className={`${h.level === 3 ? 'toc-h3' : ''} ${activeHeading === h.id ? 'active' : ''}`}
      onClick={(e) => { e.preventDefault(); scrollToHeading(h.id); }}
    >
      <TocLabel parts={h.parts} />
    </a>
  ));

  // No active-heading highlight here: it would re-render the whole markdown
  // (this element sits inside the h1 override) on every scroll step.
  const inlineToc = useMemo(() => (narrow && headings.length > 2 ? (
    <details className="docs-toc-inline">
      <summary>On this page</summary>
      <nav className="docs-toc" aria-label="On this page">
        {headings.map((h) => (
          <a
            key={h.id}
            href={`#${h.id}`}
            className={h.level === 3 ? 'toc-h3' : undefined}
            onClick={(e) => { e.preventDefault(); scrollToHeading(h.id); }}
          >
            <TocLabel parts={h.parts} />
          </a>
        ))}
      </nav>
    </details>
  ) : null), [narrow, headings, scrollToHeading]);

  const markdownComponents = useMemo<Components>(() => ({
    a: ({ href, children, node: _node, ...props }) => {
      const target = href ? classifyDocHref(href) : null;
      if (target?.kind === 'doc') {
        return (
          <a {...props} href="#" onClick={(e) => { e.preventDefault(); handleDocLink(target.file, target.anchor); }}>
            {children}
          </a>
        );
      }
      if (target?.kind === 'anchor') {
        return (
          <a {...props} href={href} onClick={(e) => { e.preventDefault(); scrollToHeading(target.anchor); }}>
            {children}
          </a>
        );
      }
      if (href && (href.startsWith('http://') || href.startsWith('https://'))) {
        return <a {...props} href={href} target="_blank" rel="noopener noreferrer">{children}</a>;
      }
      return <a {...props} href={href}>{children}</a>;
    },
    pre: ({ node: _node, ...props }) => <CodeBlock {...props} />,
    // Tables scroll inside their own box, so a wide one never widens the page.
    table: ({ node: _node, ...props }) => (
      <div className="docs-table-wrap"><table {...props} /></div>
    ),
    // On a phone, "On this page" folds in right under the title.
    h1: ({ children, node: _node, ...props }) => (
      <>
        <h1 {...props}>{children}</h1>
        {inlineToc}
      </>
    ),
    // Wrap images in Lightbox — alt text becomes caption
    img: ({ alt, src, node: _node, ...props }) => (
      <Lightbox caption={alt}>
        <img {...props} alt={alt} src={src} style={{ width: '100%', display: 'block' }} />
      </Lightbox>
    ),
    // Changelog version headings ("vX.Y.Z — DATE"): show the
    // version, tuck the release date behind a hover reveal.
    // Non-version h2s pass through unchanged. The rehypeSlug
    // `id` is preserved so anchors + the ToC still work.
    h2: ({ children, node: _node, ...props }) => {
      const split = splitVersionHeading(children);
      if (!split) return <h2 {...props}>{children}</h2>;
      return (
        <h2 {...props} className="cl-version">
          <span className="cl-ver">{split.version}</span>
          {split.title && <span className="cl-title">{split.title}</span>}
          {split.date && <span className="cl-date">{split.date}</span>}
        </h2>
      );
    },
    // "Last updated: …" line → a small top-right badge.
    p: ({ children, node: _node, ...props }) => {
      const text = nodeToText(children).trim();
      if (/^last updated:/i.test(text)) {
        return <p {...props} className="cl-updated">{children}</p>;
      }
      return <p {...props}>{children}</p>;
    },
  }), [handleDocLink, scrollToHeading, inlineToc]);

  return (
    // .docs-page pins the page to the viewport height, so the sidebar, header
    // and TOC stay put and only the content pane scrolls (height: 100% had
    // no bounded parent, so the whole document scrolled and the TOC left).
    <div className="docs-page" style={{ display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
      {onBack && <FullScreenHeader title="Documentation" onBack={onBack} onShowShortcuts={onShowShortcuts} accountMenu={accountMenu} />}

      <div style={{ display: 'flex', flex: 1, overflow: 'hidden' }}>
      {/* Left sidebar: search + doc navigation. On a phone it moves into a drawer. */}
      {narrow ? (
        <Drawer
          title={null}
          placement="left"
          open={navOpen}
          onClose={() => setNavOpen(false)}
          closable={false}
          size={280}
          styles={{ body: { padding: 0, background: colors.BG_SIDEBAR, display: 'flex', flexDirection: 'column' }, header: { display: 'none' } }}
        >
          <nav aria-label="Documentation" style={{ display: 'flex', flexDirection: 'column', minHeight: '100%' }}>
            <DocsNav docs={DOCS} grouped={grouped} selectedId={selectedId} onSelect={setSelectedId} />
          </nav>
        </Drawer>
      ) : (
        <nav aria-label="Documentation" style={{
          width: 220,
          flexShrink: 0,
          borderRight: `1px solid ${colors.BORDER}`,
          overflowY: 'auto',
          background: colors.BG_SIDEBAR,
          display: 'flex',
          flexDirection: 'column',
        }}>
          <DocsNav docs={DOCS} grouped={grouped} selectedId={selectedId} onSelect={setSelectedId} />
        </nav>
      )}

      {/* Center: markdown content + sticky ToC */}
      <div
        ref={contentRef}
        data-docs-scroll
        style={{
          flex: 1,
          overflowY: 'auto',
          padding: narrow ? '0 16px 32px' : 'clamp(20px, 4vw, 40px)',
        }}
      >
        {narrow && bundle && (
          <div className="docs-mobile-bar" style={{ background: colors.BG_BASE, borderBottom: `1px solid ${colors.BORDER}` }}>
            <Button size="small" icon={<MenuOutlined />} onClick={() => setNavOpen(true)}>
              All docs
            </Button>
            {selectedDoc && !showLanding && <CopyPageButton doc={selectedDoc} />}
          </div>
        )}
        <div style={{ display: 'flex', gap: 40, maxWidth: 1200, margin: '0 auto', paddingTop: narrow ? 20 : 0 }}>
          {/* Landing page for overview, markdown for everything else */}
          {!bundle ? (
            <div style={{ flex: 1, minWidth: 0, display: 'flex', justifyContent: 'center', padding: 60, color: colors.TEXT_MUTED }}>
              {docsError ? <span>Could not load the documentation: {docsError.message}</span> : <Spin />}
            </div>
          ) : showLanding ? (
            <div style={{ flex: 1, minWidth: 0 }}>
              <DocsLanding bundle={bundle} onSelectDoc={setSelectedId} />
            </div>
          ) : selectedDoc && (
            <article className="docs-content" style={{ flex: 1, minWidth: 0 }}>
              {!narrow && (
                <div className="docs-page-actions">
                  <CopyPageButton doc={selectedDoc} />
                </div>
              )}
              {splitMermaid(selectedDoc.content).map((segment) =>
                segment.type === 'mermaid' ? (
                  // Key by diagram content so reordering blocks doesn't reuse stale SVG state
                  <Mermaid key={`mermaid-${segment.content}`} chart={segment.content} caption={segment.caption} />
                ) : (
                  <ReactMarkdown
                    key={`text-${segment.content}`}
                    remarkPlugins={DOC_REMARK_PLUGINS}
                    rehypePlugins={DOC_REHYPE_PLUGINS}
                    components={markdownComponents}
                  >
                    {segment.content}
                  </ReactMarkdown>
                )
              )}
            </article>
          )}

          {/* ToC — sticky inside the scroll container (hidden on landing page) */}
          {!narrow && !showLanding && headings.length > 2 && (
            <nav className="docs-toc" aria-label="On this page" style={{
              width: 180,
              flexShrink: 0,
              position: 'sticky',
              top: 0,
              alignSelf: 'flex-start',
              maxHeight: 'calc(100vh - 80px)',
              overflowY: 'auto',
              paddingTop: 8,
            }}>
              <div style={{
                fontSize: 10,
                fontWeight: 700,
                textTransform: 'uppercase',
                letterSpacing: 1.5,
                color: colors.TEXT_FAINT,
                fontFamily: 'var(--font-ui)',
                marginBottom: 8,
              }}>
                On this page
              </div>
              {tocLinks}
            </nav>
          )}
          </div>
        </div>
      </div>
    </div>
  );
}
