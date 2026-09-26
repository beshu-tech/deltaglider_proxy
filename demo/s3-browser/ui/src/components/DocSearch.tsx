import { useState, useRef, useEffect, useMemo, useCallback } from 'react';
import { Input } from 'antd';
import { SearchOutlined, CloseCircleFilled } from '@ant-design/icons';
import MiniSearch from 'minisearch';
import type { DocEntry } from '../docsBundle';
import { useColors } from '../ThemeContext';
import { docSearchSnippet, docSearchText } from '../docSearchText';

interface SearchResult {
  docId: string;
  title: string;
  group: string;
  snippet: string;
  score: number;
}

/** Extract h2/h3 headings as searchable text */
function extractHeadingText(md: string): string {
  return md.split('\n')
    .filter(l => /^#{2,3}\s/.test(l))
    .map(l => l.replace(/^#+\s+/, ''))
    .join(' ');
}

interface Props {
  docs: readonly DocEntry[];
  onSelect: (docId: string) => void;
}

export default function DocSearch({ docs, onSelect }: Props) {
  const colors = useColors();
  const [query, setQuery] = useState('');
  const [focused, setFocused] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  // Build search index lazily (once)
  const searchIndex = useMemo(() => {
    const index = new MiniSearch<{ id: string; title: string; headings: string; body: string }>({
      fields: ['title', 'headings', 'body'],
      storeFields: ['title'],
      searchOptions: {
        boost: { title: 10, headings: 5, body: 1 },
        fuzzy: 0.2,
        prefix: true,
      },
    });

    index.addAll(docs.map(d => ({
      id: d.id,
      title: d.title,
      headings: extractHeadingText(d.content),
      body: docSearchText(d.content),
    })));
    return index;
  }, [docs]);

  // Search results
  const results: SearchResult[] = useMemo(() => {
    if (!query.trim()) return [];
    const raw = searchIndex.search(query, { combineWith: 'AND' });
    return raw.slice(0, 8).map(r => {
      const doc = docs.find(d => d.id === r.id)!;
      const bodyText = docSearchText(doc.content);
      return {
        docId: r.id,
        title: doc.title,
        group: doc.group,
        snippet: docSearchSnippet(bodyText, query),
        score: r.score,
      };
    });
  }, [query, searchIndex, docs]);

  // Cmd+K / Ctrl+K shortcut
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
        e.preventDefault();
        inputRef.current?.focus();
      }
      if (e.key === 'Escape' && focused) {
        setQuery('');
        inputRef.current?.blur();
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [focused]);

  const handleSelect = useCallback((docId: string) => {
    onSelect(docId);
    setQuery('');
  }, [onSelect]);

  const isActive = query.trim().length > 0;

  return (
    <div style={{ padding: '12px 12px 8px' }}>
      <Input
        ref={inputRef as never}
        prefix={<SearchOutlined style={{ color: colors.TEXT_FAINT, fontSize: 12 }} />}
        suffix={isActive ? (
          <button
            type="button"
            aria-label="Clear the search"
            onClick={() => setQuery('')}
            style={{ display: 'inline-flex', padding: 0, border: 'none', background: 'none', cursor: 'pointer' }}
          >
            <CloseCircleFilled aria-hidden="true" style={{ color: colors.TEXT_FAINT, fontSize: 12 }} />
          </button>
        ) : (
          <span style={{ fontSize: 10, color: colors.TEXT_FAINT, fontFamily: 'var(--font-mono)' }}>
            {navigator.platform?.includes('Mac') ? '⌘K' : 'Ctrl+K'}
          </span>
        )}
        placeholder="Search docs..."
        value={query}
        onChange={e => setQuery(e.target.value)}
        onFocus={() => setFocused(true)}
        onBlur={() => setTimeout(() => setFocused(false), 200)}
        size="small"
        style={{
          borderRadius: 6,
          fontSize: 12,
          fontFamily: 'var(--font-ui)',
          background: 'var(--input-bg)',
          borderColor: isActive ? colors.ACCENT_BLUE : colors.BORDER,
        }}
      />

      {isActive && (
        <div style={{ marginTop: 8 }}>
          {results.length === 0 ? (
            <div style={{ padding: '8px 4px', fontSize: 11, color: colors.TEXT_FAINT, fontFamily: 'var(--font-ui)' }}>
              No results for &ldquo;{query}&rdquo;
            </div>
          ) : (
            results.map(r => (
              <button
                key={r.docId}
                className="btn-reset"
                onClick={() => handleSelect(r.docId)}
                style={{
                  display: 'block',
                  width: '100%',
                  textAlign: 'left',
                  padding: '8px 8px',
                  borderRadius: 6,
                  marginBottom: 2,
                  cursor: 'pointer',
                  transition: 'background 0.1s',
                  background: 'transparent',
                }}
                onMouseEnter={e => e.currentTarget.style.background = `${colors.ACCENT_BLUE}10`}
                onMouseLeave={e => e.currentTarget.style.background = 'transparent'}
              >
                <div style={{ fontSize: 13, fontWeight: 600, color: colors.TEXT_PRIMARY, fontFamily: 'var(--font-ui)' }}>
                  {r.title}
                </div>
                <div style={{ fontSize: 10, color: colors.ACCENT_BLUE, fontFamily: 'var(--font-mono)', marginTop: 2 }}>
                  {r.group}
                </div>
                <div style={{
                  fontSize: 11,
                  color: colors.TEXT_MUTED,
                  fontFamily: 'var(--font-ui)',
                  marginTop: 3,
                  lineHeight: 1.4,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  display: '-webkit-box',
                  WebkitLineClamp: 2,
                  WebkitBoxOrient: 'vertical',
                }}>
                  {r.snippet}
                </div>
              </button>
            ))
          )}
        </div>
      )}
    </div>
  );
}
