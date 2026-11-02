import { useEffect, useId, useMemo, useRef, useState } from 'react';
import { useColors } from '../ThemeContext';

/** Zebra panels over the elevated popup: a faint tint of the text colour,
 *  so one rule serves both themes. */
function sectionPanelStyle(gi: number): Pick<React.CSSProperties, 'background' | 'borderColor'> {
  return {
    background: tint(cssVar('TEXT_PRIMARY'), gi % 2 === 0 ? 4 : 2),
    borderColor: tint(cssVar('TEXT_PRIMARY'), 7),
  };
}
import { useOverlayDropdown } from '../useOverlayDropdown';
import { BORDER_RADIUS, getOverlayBaseStyles } from './overlayStyles';
import { cssVar, tint } from '../themeVars';

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
  /** Forwarded to the input so a label can name it. */
  id?: string;
  'aria-label'?: string;
  'aria-labelledby'?: string;
  /** The value is rejected: red border and `aria-invalid`. */
  invalid?: boolean;
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
  id,
  'aria-label': ariaLabel,
  'aria-labelledby': ariaLabelledBy,
  invalid = false,
}: Props) {
  const listId = useId();
  const colors = useColors();
  const [open, setOpen] = useState(false);
  const [focused, setFocused] = useState(false);
  const [highlightIndex, setHighlightIndex] = useState(0);
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
  const { triggerRef: wrapRef, setOverlay, pos } = useOverlayDropdown({
    visible: showDrop,
    onClose: () => setOpen(false),
  });
  const useSectionPanels =
    displayGroups.length > 1 || displayGroups.some((g) => Boolean(g.label?.trim()));

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
          id={id}
          aria-label={ariaLabel}
          aria-labelledby={ariaLabelledBy}
          aria-invalid={invalid || undefined}
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
            padding: '0 8px',
            border: `1px solid ${invalid ? colors.ACCENT_RED : focused ? colors.ACCENT_BLUE : colors.BORDER}`,
            borderRadius: BORDER_RADIUS.sm,
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
          ref={setOverlay}
          id={listId}
          role="listbox"
          style={{
            ...getOverlayBaseStyles(colors, pos, { minWidth: 220, maxHeight: 280, flexLayout: true }),
            padding: useSectionPanels ? '8px 8px 12px' : '4px 0',
            gap: useSectionPanels ? 12 : 0,
          }}
        >
          {displayGroups.map((group, gi) => {
            const headerPad = { paddingLeft: 2, paddingRight: 2 };
            const stripe = sectionPanelStyle(gi);
            const panelWrap = useSectionPanels
              ? {
                  borderRadius: 8,
                  border: `1px solid ${stripe.borderColor}`,
                  background: stripe.background,
                  padding: '12px 8px 8px',
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
                      marginBottom: group.subtitle ? 4 : 8,
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
                      marginBottom: 12,
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
                        padding: '8px 8px',
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
                          ? tint(colors.ACCENT_BLUE, 16)
                          : entry.value === value
                            ? tint(colors.ACCENT_BLUE, 11)
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
