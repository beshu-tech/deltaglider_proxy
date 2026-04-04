import { useState, useEffect, useRef, useMemo, useCallback } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import rehypeSlug from 'rehype-slug';
import mermaid from 'mermaid';
import { DOCS, DOC_GROUPS, findDocByFilename, type DocEntry } from '../docs-imports';
import { useColors } from '../ThemeContext';
import FullScreenHeader from './FullScreenHeader';
import DocSearch from './DocSearch';
import Lightbox from './Lightbox';
import { useNavigation } from '../NavigationContext';
import DocsLanding from './DocsLanding';
import '../docs.css';

mermaid.initialize({
  startOnLoad: false,
  theme: 'dark',
  themeVariables: { primaryColor: '#2dd4bf', lineColor: '#5e7290' },
  flowchart: { useMaxWidth: false },
  sequence: { useMaxWidth: false },
});

/** Self-contained Mermaid diagram React component.
 * After render, measures the actual content bbox and rewrites the viewBox
 * to fit tightly — Mermaid's default viewBox is often 2-3x larger than the content. */
function Mermaid({ chart, caption }: { chart: string; caption?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [svg, setSvg] = useState('');

  useEffect(() => {
    let cancelled = false;
    const id = `mermaid-${Math.random().toString(36).slice(2, 8)}`;
    mermaid.render(id, chart).then(({ svg: rendered }) => {
      if (!cancelled) setSvg(rendered);
    }).catch(console.warn);
    return () => { cancelled = true; };
  }, [chart]);

  // After SVG is in the DOM, measure actual content and fix viewBox
  useEffect(() => {
    if (!svg || !ref.current) return;
    const svgEl = ref.current.querySelector('svg');
    if (!svgEl) return;
    try {
      const bb = svgEl.getBBox();
      const pad = 16;
      svgEl.setAttribute('viewBox', `${bb.x - pad} ${bb.y - pad} ${bb.width + pad * 2} ${bb.height + pad * 2}`);
      svgEl.removeAttribute('width');
      svgEl.removeAttribute('height');
      svgEl.style.width = '100%';
      svgEl.style.height = 'auto';
      svgEl.style.maxWidth = 'none';
    } catch {
      // getBBox can fail if SVG is not visible
    }
  }, [svg]);
  return (
    <Lightbox caption={caption}>
      <div ref={ref} className="mermaid-diagram" dangerouslySetInnerHTML={{ __html: svg }} />
    </Lightbox>
  );
}


/** Split markdown into text segments and mermaid code blocks */
function splitMermaid(md: string): { type: 'text' | 'mermaid'; content: string; caption?: string }[] {
  const segments: { type: 'text' | 'mermaid'; content: string; caption?: string }[] = [];
  const regex = /```mermaid\n([\s\S]*?)```/g;
  let lastIndex = 0;
  let match;
  while ((match = regex.exec(md)) !== null) {
    const textBefore = md.slice(lastIndex, match.index);
    if (textBefore) {
      segments.push({ type: 'text', content: textBefore });
    }
    // Use the last heading before the mermaid block as caption
    const lines = textBefore.trim().split('\n');
    const lastHeading = [...lines].reverse().find(l => /^#{2,4}\s/.test(l));
    const caption = lastHeading?.replace(/^#+\s+/, '').trim();
    const mermaidContent = match[1].trim();
    if (mermaidContent) {
      segments.push({ type: 'mermaid', content: mermaidContent, caption });
    }
    lastIndex = match.index + match[0].length;
  }
  if (lastIndex < md.length) {
    segments.push({ type: 'text', content: md.slice(lastIndex) });
  }
  return segments;
}

interface TocItem {
  id: string;
  text: string;
  level: number;
}

/** Extract headings from markdown for ToC */
function extractHeadings(markdown: string): TocItem[] {
  const items: TocItem[] = [];
  for (const line of markdown.split('\n')) {
    const m = line.match(/^(#{2,3})\s+(.+)/);
    if (m) {
      const text = m[2].replace(/[`*_\[\]]/g, '');
      const id = text.toLowerCase().replace(/[^\w]+/g, '-').replace(/(^-|-$)/g, '');
      items.push({ id, text, level: m[1].length });
    }
  }
  return items;
}

interface Props {
  /** Doc ID from URL path (e.g., 'configuration' from /_/docs/configuration) */
  docId?: string;
  onBack?: () => void;
}

export default function DocsPage({ docId, onBack }: Props) {
  const colors = useColors();
  const { navigate } = useNavigation();

  // Resolve doc ID: URL-driven if provided, else default to first doc
  const resolvedId = (docId && DOCS.some(d => d.id === docId)) ? docId : DOCS[0]?.id || '';
  const [selectedId, setSelectedIdState] = useState(resolvedId);

  // Sync selectedId when URL changes (browser back/forward)
  useEffect(() => {
    if (docId && DOCS.some(d => d.id === docId)) {
      setSelectedIdState(docId);
    }
  }, [docId]);

  // Navigate + update state when user selects a doc
  const setSelectedId = useCallback((id: string) => {
    setSelectedIdState(id);
    navigate(`docs/${id}`);
  }, [navigate]);
  const [activeHeading, setActiveHeading] = useState('');
  const contentRef = useRef<HTMLDivElement>(null);

  const selectedDoc = useMemo(() => DOCS.find(d => d.id === selectedId), [selectedId]);
  const headings = useMemo(() => selectedDoc ? extractHeadings(selectedDoc.content) : [], [selectedDoc]);

  // Scroll to top when doc changes
  useEffect(() => {
    contentRef.current?.scrollTo(0, 0);
    setActiveHeading('');
  }, [selectedId]);


  // Intersection observer for active heading tracking
  useEffect(() => {
    const el = contentRef.current;
    if (!el) return;

    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) {
            setActiveHeading(entry.target.id);
          }
        }
      },
      { root: el, rootMargin: '-10% 0px -80% 0px' }
    );

    const headingEls = el.querySelectorAll('h2[id], h3[id]');
    headingEls.forEach(h => observer.observe(h));
    return () => observer.disconnect();
  }, [selectedId, selectedDoc]);

  // Inter-page link handler
  const handleLinkClick = useCallback((href: string) => {
    const doc = findDocByFilename(href);
    if (doc) {
      setSelectedId(doc.id);
      return true;
    }
    return false;
  }, [setSelectedId]);

  // Group docs by category
  const grouped = useMemo(() => {
    const map = new Map<string, DocEntry[]>();
    for (const g of DOC_GROUPS) map.set(g, []);
    for (const d of DOCS) {
      const list = map.get(d.group);
      if (list) list.push(d);
    }
    return map;
  }, []);

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', overflow: 'hidden' }}>
      {onBack && <FullScreenHeader title="Documentation" onBack={onBack} />}

      <div style={{ display: 'flex', flex: 1, overflow: 'hidden' }}>
      {/* Left sidebar: search + doc navigation */}
      <nav className="hide-mobile" style={{
        width: 220,
        flexShrink: 0,
        borderRight: `1px solid ${colors.BORDER}`,
        overflowY: 'auto',
        background: colors.BG_SIDEBAR,
        display: 'flex',
        flexDirection: 'column',
      }}>
        <DocSearch onSelect={setSelectedId} />
        <div style={{ padding: '0 0 16px', flex: 1, overflowY: 'auto' }}>
        {Array.from(grouped.entries()).map(([group, docs]) => (
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
            {docs.map(doc => (
              <button
                key={doc.id}
                className="btn-reset"
                onClick={() => setSelectedId(doc.id)}
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
                onMouseEnter={e => {
                  if (doc.id !== selectedId) e.currentTarget.style.color = colors.TEXT_PRIMARY;
                }}
                onMouseLeave={e => {
                  if (doc.id !== selectedId) e.currentTarget.style.color = colors.TEXT_SECONDARY;
                }}
              >
                {doc.title}
              </button>
            ))}
          </div>
        ))}
        </div>
      </nav>

      {/* Center: markdown content + sticky ToC */}
      <div
        ref={contentRef}
        style={{
          flex: 1,
          overflowY: 'auto',
          padding: 'clamp(20px, 4vw, 40px)',
        }}
      >
        <div style={{ display: 'flex', gap: 32, maxWidth: 1100, margin: '0 auto' }}>
          {/* Landing page for overview, markdown for everything else */}
          {selectedId === 'readme' || !selectedDoc ? (
            <div style={{ flex: 1, minWidth: 0 }}>
              <DocsLanding onSelectDoc={setSelectedId} />
            </div>
          ) : selectedDoc && (
            <article className="docs-content" style={{ flex: 1, minWidth: 0 }}>
              {splitMermaid(selectedDoc.content).map((segment, i) =>
                segment.type === 'mermaid' ? (
                  <Mermaid key={i} chart={segment.content} caption={segment.caption} />
                ) : (
                  <ReactMarkdown
                    key={i}
                    remarkPlugins={[remarkGfm]}
                    rehypePlugins={[rehypeHighlight, rehypeSlug]}
                    components={{
                      a: ({ href, children, ...props }) => {
                        if (href && href.endsWith('.md')) {
                          return (
                            <a {...props} href="#" onClick={(e) => { e.preventDefault(); handleLinkClick(href); }}>
                              {children}
                            </a>
                          );
                        }
                        if (href && (href.startsWith('http://') || href.startsWith('https://'))) {
                          return <a {...props} href={href} target="_blank" rel="noopener noreferrer">{children}</a>;
                        }
                        return <a {...props} href={href}>{children}</a>;
                      },
                      // Wrap images in Lightbox — alt text becomes caption
                      img: ({ alt, src, ...props }) => (
                        <Lightbox caption={alt}>
                          <img {...props} alt={alt} src={src} style={{ width: '100%', display: 'block' }} />
                        </Lightbox>
                      ),
                    }}
                  >
                    {segment.content}
                  </ReactMarkdown>
                )
              )}
            </article>
          )}

          {/* ToC — sticky inside the scroll container (hidden on landing page) */}
          {selectedId !== 'readme' && headings.length > 2 && (
            <nav className="docs-toc hide-mobile" style={{
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
              {headings.map(h => (
                <a
                  key={h.id}
                  href={`#${h.id}`}
                  className={`${h.level === 3 ? 'toc-h3' : ''} ${activeHeading === h.id ? 'active' : ''}`}
                  onClick={(e) => {
                    e.preventDefault();
                    const el = contentRef.current?.querySelector(`#${CSS.escape(h.id)}`);
                    el?.scrollIntoView({ behavior: 'smooth', block: 'start' });
                  }}
                >
                  {h.text}
                </a>
              ))}
            </nav>
          )}
          </div>
        </div>
      </div>
    </div>
  );
}
