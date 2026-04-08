import { useState, useRef, useEffect } from 'react';
import { useColors } from '../ThemeContext';

/**
 * Self-contained autocomplete input. Type to filter, click to select,
 * or type a custom value. No Ant Design popups.
 */

interface Props {
  value: string;
  onChange: (value: string) => void;
  options: string[];
  placeholder?: string;
  style?: React.CSSProperties;
}

export default function SimpleAutoComplete({ value, onChange, options, placeholder, style }: Props) {
  const colors = useColors();
  const [open, setOpen] = useState(false);
  const [focused, setFocused] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  const dropRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const [pos, setPos] = useState({ top: 0, left: 0, width: 0 });

  const filtered = options.filter(o => o.toLowerCase().includes(value.toLowerCase()));
  const showDrop = open && focused && filtered.length > 0;

  useEffect(() => {
    if (showDrop && wrapRef.current) {
      const r = wrapRef.current.getBoundingClientRect();
      setPos({ top: r.bottom + 2, left: r.left, width: r.width });
    }
  }, [showDrop, value]);

  useEffect(() => {
    if (!showDrop) return;
    const handler = (e: MouseEvent) => {
      if (wrapRef.current?.contains(e.target as Node)) return;
      if (dropRef.current?.contains(e.target as Node)) return;
      setOpen(false);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [showDrop]);

  return (
    <>
      <div ref={wrapRef} style={{ display: 'inline-flex', ...style }}>
        <input
          ref={inputRef}
          value={value}
          onChange={e => { onChange(e.target.value); setOpen(true); }}
          onFocus={() => { setFocused(true); setOpen(true); }}
          onBlur={() => setTimeout(() => setFocused(false), 150)}
          placeholder={placeholder ?? 'Type to search...'}
          style={{
            width: '100%', height: 34, padding: '0 10px',
            border: `1px solid ${focused ? colors.ACCENT_BLUE : colors.BORDER}`,
            borderRadius: 6, background: colors.BG_ELEVATED,
            color: colors.TEXT_PRIMARY, outline: 'none',
            fontSize: 13, fontFamily: 'var(--font-mono)',
            transition: 'border-color 0.15s',
            boxSizing: 'border-box',
          }}
        />
      </div>

      {showDrop && (
        <div
          ref={dropRef}
          style={{
            position: 'fixed',
            top: pos.top,
            left: pos.left,
            width: Math.max(pos.width, 180),
            maxHeight: 200,
            overflowY: 'auto',
            background: colors.BG_ELEVATED,
            border: `1px solid ${colors.BORDER}`,
            borderRadius: 8,
            boxShadow: '0 8px 24px rgba(0,0,0,0.3)',
            zIndex: 99999,
            padding: 4,
          }}
        >
          {filtered.map(o => (
            <div
              key={o}
              onMouseDown={(e) => { e.preventDefault(); onChange(o); setOpen(false); }}
              style={{
                padding: '6px 8px', cursor: 'pointer', borderRadius: 4,
                fontSize: 13, fontFamily: 'var(--font-mono)',
                background: o === value ? `${colors.ACCENT_BLUE}18` : 'transparent',
                color: o === value ? colors.ACCENT_BLUE : colors.TEXT_PRIMARY,
                transition: 'background 0.1s',
              }}
              onMouseEnter={e => { if (o !== value) (e.target as HTMLElement).style.background = `${colors.ACCENT_BLUE}0c`; }}
              onMouseLeave={e => { if (o !== value) (e.target as HTMLElement).style.background = 'transparent'; }}
            >
              {o}
            </div>
          ))}
        </div>
      )}
    </>
  );
}
