import { useState, useRef, useEffect } from 'react';
import { useColors } from '../ThemeContext';

/**
 * Self-contained dropdown select. No Ant Design popup layer, no rc-component/trigger,
 * no getPopupContainer nonsense. Pure React + inline styles + a portal-free absolute div.
 *
 * Renders the dropdown as a fixed-position overlay attached to the trigger button,
 * measured via getBoundingClientRect on every open. Immune to CSS transforms,
 * overflow:hidden, and z-index stacking contexts.
 */

interface SimpleSelectOption {
  value: string;
  label: string;
  sublabel?: string;
}

interface Props {
  value?: string;
  onChange: (value: string) => void;
  options: SimpleSelectOption[];
  placeholder?: string;
  allowClear?: boolean;
  style?: React.CSSProperties;
  size?: 'small' | 'middle';
}

export default function SimpleSelect({ value, onChange, options, placeholder, allowClear, style, size }: Props) {
  const colors = useColors();
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState('');
  const triggerRef = useRef<HTMLDivElement>(null);
  const dropdownRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const [pos, setPos] = useState({ top: 0, left: 0, width: 0 });

  const selected = options.find(o => o.value === value);
  const isSmall = size === 'small';
  const h = isSmall ? 28 : 34;

  // Position dropdown using fixed coordinates from getBoundingClientRect
  useEffect(() => {
    if (open && triggerRef.current) {
      const r = triggerRef.current.getBoundingClientRect();
      setPos({ top: r.bottom + 2, left: r.left, width: r.width });
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (triggerRef.current?.contains(e.target as Node)) return;
      if (dropdownRef.current?.contains(e.target as Node)) return;
      setOpen(false);
      setSearch('');
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [open]);

  // Close on Escape
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') { setOpen(false); setSearch(''); }
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [open]);

  const filtered = options.filter(o =>
    o.label.toLowerCase().includes(search.toLowerCase()) ||
    (o.sublabel ?? '').toLowerCase().includes(search.toLowerCase())
  );

  const select = (val: string) => {
    onChange(val);
    setOpen(false);
    setSearch('');
  };

  return (
    <>
      {/* Trigger button */}
      <div
        ref={triggerRef}
        onClick={() => setOpen(!open)}
        style={{
          display: 'inline-flex', alignItems: 'center', justifyContent: 'space-between',
          height: h, padding: '0 10px', cursor: 'pointer',
          border: `1px solid ${open ? colors.ACCENT_BLUE : colors.BORDER}`,
          borderRadius: 6, background: colors.BG_ELEVATED,
          fontSize: isSmall ? 12 : 13, fontFamily: 'var(--font-ui)',
          color: selected ? colors.TEXT_PRIMARY : colors.TEXT_MUTED,
          transition: 'border-color 0.15s',
          minWidth: 0,
          ...style,
        }}
      >
        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1 }}>
          {selected ? selected.label : (placeholder ?? 'Select...')}
        </span>
        <span style={{ marginLeft: 6, fontSize: 10, color: colors.TEXT_MUTED, flexShrink: 0 }}>
          {allowClear && selected ? (
            <span
              onClick={(e) => { e.stopPropagation(); onChange(''); setOpen(false); }}
              style={{ cursor: 'pointer', fontSize: 12, padding: '0 2px' }}
              title="Clear"
            >
              ×
            </span>
          ) : (
            open ? '▲' : '▼'
          )}
        </span>
      </div>

      {/* Dropdown overlay — fixed position, measured from trigger rect */}
      {open && (
        <div
          ref={dropdownRef}
          style={{
            position: 'fixed',
            top: pos.top,
            left: pos.left,
            width: Math.max(pos.width, 200),
            maxHeight: 240,
            overflowY: 'auto',
            background: colors.BG_ELEVATED,
            border: `1px solid ${colors.BORDER}`,
            borderRadius: 8,
            boxShadow: '0 8px 24px rgba(0,0,0,0.3)',
            zIndex: 99999,
            padding: 4,
          }}
        >
          {/* Search input */}
          {options.length > 5 && (
            <input
              ref={inputRef}
              value={search}
              onChange={e => setSearch(e.target.value)}
              placeholder="Search..."
              style={{
                width: '100%', border: 'none', outline: 'none',
                background: colors.BG_BASE, color: colors.TEXT_PRIMARY,
                padding: '6px 8px', borderRadius: 4, marginBottom: 4,
                fontSize: 12, fontFamily: 'var(--font-ui)',
                boxSizing: 'border-box',
              }}
            />
          )}
          {filtered.length === 0 && (
            <div style={{ padding: '8px 8px', fontSize: 12, color: colors.TEXT_MUTED, textAlign: 'center' }}>
              No matches
            </div>
          )}
          {filtered.map(o => (
            <div
              key={o.value}
              onClick={() => select(o.value)}
              style={{
                padding: '6px 8px', cursor: 'pointer', borderRadius: 4,
                fontSize: isSmall ? 12 : 13, fontFamily: 'var(--font-ui)',
                background: o.value === value ? `${colors.ACCENT_BLUE}18` : 'transparent',
                color: o.value === value ? colors.ACCENT_BLUE : colors.TEXT_PRIMARY,
                transition: 'background 0.1s',
              }}
              onMouseEnter={e => { if (o.value !== value) (e.target as HTMLElement).style.background = `${colors.ACCENT_BLUE}0c`; }}
              onMouseLeave={e => { if (o.value !== value) (e.target as HTMLElement).style.background = 'transparent'; }}
            >
              <div>{o.label}</div>
              {o.sublabel && <div style={{ fontSize: 10, color: colors.TEXT_MUTED, marginTop: 1 }}>{o.sublabel}</div>}
            </div>
          ))}
        </div>
      )}
    </>
  );
}
