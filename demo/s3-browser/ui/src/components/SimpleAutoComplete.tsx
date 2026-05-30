import { useEffect, useId, useMemo, useRef, useState } from 'react';
import { useColors, useTheme } from '../ThemeContext';

function sectionPanelStyle(
  gi: number,
  isDark: boolean,
): Pick<React.CSSProperties, 'background' | 'borderColor'> {
  if (isDark) {
    return gi % 2 === 0
      ? { background: '#152238', borderColor: 'rgba(255,255,255,0.07)' }
      : { background: '#101a2c', borderColor: 'rgba(255,255,255,0.06)' };
  }
  return gi % 2 === 0
    ? { background: '#f8fafc', borderColor: 'rgba(15,23,42,0.08)' }
    : { background: '#f1f5f9', borderColor: 'rgba(15,23,42,0.07)' };
}
import { useOnClickOutside } from '../useDocumentEvent';
import { useFixedOverlayPosition } from '../useFixedOverlayPosition';

/**
 * Self-contained autocomplete input. Type to filter, click to select,
 * or type a custom value. No Ant Design popups.
 */

export type AutoCompleteEntry =
  | { value: string; source: 'listed' }
  | { value: string; source: 'template'; realPrefix: string };

export type AutoCompleteGroup = {
  label: string;
  subtitle?: string;
  entries: AutoCompleteEntry[];
};

interface Props {
  value: string;
  onChange: (value: string) => void;
  onBlur?: () => void;
  /** Flat options (single implicit group, no section headers). */
  options?: string[];
  /** Grouped options with optional section labels. Overrides `options` when non-empty. */
  optionGroups?: AutoCompleteGroup[];
  filterText?: string;
  onOptionSelect?: (value: string) => void;
  placeholder?: string;
  inputTitle?: string;
  style?: React.CSSProperties;
  /** Override browser autofill name (reduces “ghost” inline predictions on technical fields). */
  autoComplete?: string;
}

function normalizeGroups(options: string[] | undefined, optionGroups: AutoCompleteGroup[] | undefined): AutoCompleteGroup[] {
  if (optionGroups?.length) return optionGroups;
  return [
    {
      label: '',
      entries: (options ?? []).map((v) => ({ value: v, source: 'listed' as const })),
    },
  ];
}

function renderEntryLabel(entry: AutoCompleteEntry, colors: ReturnType<typeof useColors>): React.ReactNode {
  if (entry.source === 'listed') {
    return entry.value;
  }
  const rp = entry.realPrefix;
  if (rp.length > 0 && entry.value.startsWith(rp)) {
    const tail = entry.value.slice(rp.length);
    return (
      <>
        <span style={{ color: colors.TEXT_PRIMARY }}>{rp}</span>
        <span style={{ color: colors.TEXT_MUTED }}>{tail}</span>
      </>
    );
  }
  return <span style={{ color: colors.TEXT_MUTED }}>{entry.value}</span>;
}

export default function SimpleAutoComplete({
  value,
  onChange,
  onBlur,
  options = [],
  optionGroups,
  filterText,
  onOptionSelect,
  placeholder,
  inputTitle,
  style,
  autoComplete = 'off',
}: Props) {
  const listId = useId();
  const colors = useColors();
  const { isDark } = useTheme();
  const [open, setOpen] = useState(false);
  const [focused, setFocused] = useState(false);
  const [highlightIndex, setHighlightIndex] = useState(0);
  const wrapRef = useRef<HTMLDivElement>(null);
  const dropRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  // The blur handler defers `setFocused(false)` so a click on a dropdown option
  // (which blurs the input) lands before the dropdown unmounts. Track the timer
  // so we can cancel it on unmount — otherwise a component torn down within the
  // 150ms window fires setState-on-unmounted (dev warning + a stray timer).
  const blurTimerRef = useRef<number | null>(null);
  useEffect(() => () => {
    if (blurTimerRef.current !== null) window.clearTimeout(blurTimerRef.current);
  }, []);

  const query = filterText ?? value;
  const qLower = query.toLowerCase();

  const displayGroups = useMemo(() => {
    const raw = normalizeGroups(options, optionGroups);
    return raw
      .map((g) => ({
        ...g,
        entries: g.entries.filter((e) => e.value.toLowerCase().includes(qLower)),
      }))
      .filter((g) => g.entries.length > 0);
  }, [options, optionGroups, qLower]);

  const flatEntries = useMemo(() => displayGroups.flatMap((g) => g.entries), [displayGroups]);

  const showDrop = open && focused && flatEntries.length > 0;
  const pos = useFixedOverlayPosition(wrapRef, showDrop);
  const useSectionPanels =
    displayGroups.length > 1 || displayGroups.some((g) => Boolean(g.label?.trim()));

  useOnClickOutside([wrapRef, dropRef], () => setOpen(false), showDrop);

  useEffect(() => {
    setHighlightIndex(0);
  }, [query, displayGroups]);

  useEffect(() => {
    setHighlightIndex((idx) => (flatEntries.length === 0 ? 0 : Math.min(idx, flatEntries.length - 1)));
  }, [flatEntries.length]);

  const applyOption = (o: string) => {
    if (onOptionSelect) onOptionSelect(o);
    else onChange(o);
    setOpen(false);
    inputRef.current?.focus();
  };

  return (
    <>
      <div ref={wrapRef} style={{ display: 'flex', minWidth: 0, width: '100%', ...style }}>
        <input
          ref={inputRef}
          value={value}
          autoComplete={autoComplete}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          role="combobox"
          aria-expanded={showDrop}
          aria-controls={showDrop ? listId : undefined}
          aria-activedescendant={
            showDrop && flatEntries[highlightIndex] ? `${listId}-opt-${highlightIndex}` : undefined
          }
          onChange={(e) => {
            onChange(e.target.value);
            setOpen(true);
          }}
          onFocus={() => {
            setFocused(true);
            setOpen(true);
          }}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') {
              e.preventDefault();
              if (flatEntries.length === 0) return;
              setOpen(true);
              setHighlightIndex((i) => Math.min(flatEntries.length - 1, i + 1));
            } else if (e.key === 'ArrowUp') {
              e.preventDefault();
              if (flatEntries.length === 0) return;
              setOpen(true);
              setHighlightIndex((i) => Math.max(0, i - 1));
            } else if (e.key === 'Enter' && showDrop && flatEntries.length > 0) {
              e.preventDefault();
              const pick = flatEntries[Math.min(highlightIndex, flatEntries.length - 1)];
              if (pick) applyOption(pick.value);
            } else if (e.key === 'Escape') {
              e.preventDefault();
              setOpen(false);
            }
          }}
          onBlur={() => {
            onBlur?.();
            if (blurTimerRef.current !== null) window.clearTimeout(blurTimerRef.current);
            blurTimerRef.current = window.setTimeout(() => {
              blurTimerRef.current = null;
              setFocused(false);
            }, 150);
          }}
          placeholder={placeholder ?? 'Type to search...'}
          title={inputTitle}
          style={{
            width: '100%',
            height: 36,
            padding: '0 10px',
            border: `1px solid ${focused ? colors.ACCENT_BLUE : colors.BORDER}`,
            borderRadius: 6,
            background: colors.BG_ELEVATED,
            color: colors.TEXT_PRIMARY,
            outline: 'none',
            fontSize: 13,
            fontFamily: 'var(--font-mono)',
            transition: 'border-color 0.15s',
            boxSizing: 'border-box',
          }}
        />
      </div>

      {showDrop && (
        <div
          ref={dropRef}
          id={listId}
          role="listbox"
          style={{
            position: 'fixed',
            top: pos.top,
            left: pos.left,
            width: Math.max(pos.width, 220),
            maxHeight: 280,
            overflowY: 'auto',
            background: colors.BG_ELEVATED,
            border: `1px solid ${colors.BORDER}`,
            borderRadius: 8,
            boxShadow: '0 8px 24px rgba(0,0,0,0.3)',
            zIndex: 99999,
            padding: useSectionPanels ? '8px 6px 10px' : '4px 0',
            display: 'flex',
            flexDirection: 'column',
            gap: useSectionPanels ? 10 : 0,
          }}
        >
          {displayGroups.map((group, gi) => {
            const headerPad = { paddingLeft: 2, paddingRight: 2 };
            const stripe = sectionPanelStyle(gi, isDark);
            const panelWrap = useSectionPanels
              ? {
                  borderRadius: 8,
                  border: `1px solid ${stripe.borderColor}`,
                  background: stripe.background,
                  padding: '12px 10px 10px',
                }
              : { padding: '0 0' };

            return (
              <div
                key={`g-${gi}-${group.label}`}
                role="group"
                aria-label={group.label || 'Suggestions'}
                style={panelWrap}
              >
                {group.label ? (
                  <div
                    style={{
                      ...headerPad,
                      marginBottom: group.subtitle ? 6 : 8,
                      fontSize: 11,
                      fontWeight: 600,
                      letterSpacing: '0.01em',
                      lineHeight: 1.35,
                      color: colors.TEXT_SECONDARY,
                      fontFamily: 'var(--font-ui)',
                    }}
                  >
                    {group.label}
                  </div>
                ) : null}
                {group.subtitle ? (
                  <div
                    style={{
                      ...headerPad,
                      marginBottom: 10,
                      fontSize: 11,
                      lineHeight: 1.5,
                      color: colors.TEXT_MUTED,
                      fontFamily: 'var(--font-ui)',
                    }}
                  >
                    {group.subtitle}
                  </div>
                ) : null}
                {group.entries.map((entry, ei) => {
                  const idx =
                    displayGroups.slice(0, gi).reduce((sum, g) => sum + g.entries.length, 0) + ei;
                  const hi = idx === highlightIndex;
                  const lastInGroup = ei === group.entries.length - 1;
                  return (
                    <div
                      key={`${gi}-${ei}-${entry.source}-${entry.value}`}
                      id={`${listId}-opt-${idx}`}
                      role="option"
                      aria-selected={hi}
                      onMouseDown={(e) => {
                        e.preventDefault();
                        applyOption(entry.value);
                      }}
                      title={`Use ${entry.value}`}
                      style={{
                        padding: '7px 8px',
                        cursor: 'pointer',
                        borderRadius: 6,
                        margin: useSectionPanels
                          ? lastInGroup
                            ? '0'
                            : '0 0 4px 0'
                          : '0 4px 4px 4px',
                        fontSize: 13,
                        lineHeight: 1.4,
                        fontFamily: 'var(--font-mono)',
                        background: hi
                          ? `${colors.ACCENT_BLUE}28`
                          : entry.value === value
                            ? `${colors.ACCENT_BLUE}1c`
                            : 'transparent',
                        transition: 'background 0.08s',
                        color: colors.TEXT_PRIMARY,
                      }}
                      onMouseEnter={() => setHighlightIndex(idx)}
                    >
                      {renderEntryLabel(entry, colors)}
                    </div>
                  );
                })}
              </div>
            );
          })}
        </div>
      )}
    </>
  );
}
