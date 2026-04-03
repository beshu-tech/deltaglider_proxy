import { useState, useEffect, useRef, useMemo, useCallback } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import rehypeSlug from 'rehype-slug';
import { DOCS, DOC_GROUPS, findDocByFilename, type DocEntry } from '../docs-imports';
import { useColors } from '../ThemeContext';
import FullScreenHeader from './FullScreenHeader';
import '../docs.css';

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
  initialDoc?: string;
  onBack?: () => void;
}

export default function DocsPage({ initialDoc, onBack }: Props) {
  const colors = useColors();
  const [selectedId, setSelectedId] = useState(initialDoc || DOCS[0]?.id || '');
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
  }, []);

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
      {/* Left sidebar: doc navigation */}
      <nav style={{
        width: 200,
        flexShrink: 0,
        borderRight: `1px solid ${colors.BORDER}`,
        overflowY: 'auto',
        padding: '16px 0',
        background: colors.BG_SIDEBAR,
      }}>
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
          {/* Markdown content */}
          {selectedDoc && (
            <article className="docs-content" style={{ flex: 1, minWidth: 0 }}>
              <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                rehypePlugins={[rehypeHighlight, rehypeSlug]}
                components={{
                  a: ({ href, children, ...props }) => {
                    if (href && href.endsWith('.md')) {
                      return (
                        <a
                          {...props}
                          href="#"
                          onClick={(e) => {
                            e.preventDefault();
                            handleLinkClick(href);
                          }}
                        >
                          {children}
                        </a>
                      );
                    }
                    if (href && (href.startsWith('http://') || href.startsWith('https://'))) {
                      return <a {...props} href={href} target="_blank" rel="noopener noreferrer">{children}</a>;
                    }
                    return <a {...props} href={href}>{children}</a>;
                  },
                }}
              >
                {selectedDoc.content}
              </ReactMarkdown>
            </article>
          )}

          {/* ToC — sticky inside the scroll container */}
          {headings.length > 2 && (
            <nav className="docs-toc" style={{
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
