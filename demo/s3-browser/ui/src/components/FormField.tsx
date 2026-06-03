/**
 * FormField — Wave 2 foundation wrapper.
 *
 * A thin shell around Ant Design's Form.Item / Typography.Text that
 * standardises label, help text, default-value placeholder, and the
 * "overriding default" indicator (§2.5, §2.6 of the admin UI revamp plan).
 * Every form input across the new admin panels is expected to be
 * rendered inside a FormField so labelling + override UX stays
 * consistent.
 *
 * ## Anatomy
 *
 * ```
 *  ┌── Label — Plain English name (not YAML key)  `yaml.path` (code chip)
 *  │     [       input (passed as children)       ]
 *  │     Help text below the input — one sentence. • example chip
 *  └──
 *         ↑
 *    Amber left bar when `overrideActive` is true.
 * ```
 *
 * ## Intentional constraints
 *
 * - **Does not render the input itself.** Callers pass whatever input
 *   they need (`<Input>`, `<InputNumber>`, `<Switch>`, `<SimpleSelect>`,
 *   `<Radio.Group>`) as children. FormField owns the chrome; the caller
 *   owns control behaviour. Keeps FormField free of input-type branches.
 * - **Placeholder ≠ value.** The `defaultPlaceholder` prop is a label
 *   shown when the field is empty; it does NOT pre-fill the field. The
 *   invariant "omitted = default" in the YAML is preserved this way.
 * - **Example chips are suggestions, not assignments.** Clicking one
 *   calls `onExampleClick(value)`; the caller decides whether to write
 *   it into the form state. That avoids FormField needing to know about
 *   the form controller (react-hook-form vs. uncontrolled input).
 */
import type { CSSProperties, ReactNode } from 'react';
import { Tag } from 'antd';
import { useColors } from '../ThemeContext';

interface FormFieldProps {
  /**
   * Plain-English field name. E.g. "Reference-cache size (MB)".
   * Accepts ReactNode so callers can embed inline chips (e.g. a
   * "Restart required" badge) next to the text — the underlying
   * `<span>` renders whatever's passed.
   */
  label: ReactNode;
  /** Full YAML path. E.g. `advanced.cache_size_mb`. */
  yamlPath?: string;
  /** One-sentence help shown below the input. */
  helpText?: string;
  /** Greyed placeholder showing the runtime default (never pre-filled). */
  defaultPlaceholder?: string;
  /** Clickable example chips — each calls `onExampleClick(value)`. */
  examples?: Array<string | number>;
  /** Handler for example clicks; no-op when omitted (chips become display-only). */
  onExampleClick?: (value: string | number) => void;
  /**
   * True when the field currently differs from its compile-time default.
   * Renders an amber left-edge bar so operators can see at a glance which
   * settings they've customised.
   */
  overrideActive?: boolean;
  /**
   * Chip labelling this field's source of truth — `"YAML-managed"`,
   * `"from DGP_X_Y"`, `"read-only"`. Matches the §2.2 honesty layer.
   * Rendered inline with the label.
   */
  ownerBadge?: string;
  /** Override indicator bar colour; defaults to amber. */
  overrideColour?: string;
  /** The input element itself. */
  children: ReactNode;
  /** Optional override for the outer container style. */
  style?: CSSProperties;
}

export default function FormField({
  label,
  yamlPath,
  helpText,
  defaultPlaceholder,
  examples,
  onExampleClick,
  overrideActive = false,
  ownerBadge,
  overrideColour,
  children,
  style,
}: FormFieldProps) {
  const { TEXT_PRIMARY: TEXT, TEXT_MUTED, TEXT_FAINT, BG_CARD, BORDER, ACCENT_AMBER } = useColors();
  const barColour = overrideColour || ACCENT_AMBER; // amber — matches §2.6 "override" indicator
  // Tight groups, air between: the label→input→help unit hugs together; the
  // BIG gap lives at the bottom of the group so each field reads as one chunk.
  const containerStyle: CSSProperties = {
    position: 'relative',
    paddingLeft: overrideActive ? 12 : 0,
    marginBottom: 32,
    transition: 'padding-left 120ms ease',
    ...style,
  };
  const barStyle: CSSProperties = {
    position: 'absolute',
    left: 0,
    top: 0,
    bottom: 0,
    width: 3,
    borderRadius: 3,
    background: barColour,
    display: overrideActive ? 'block' : 'none',
  };

  return (
    <div className="dg-field" style={containerStyle}>
      <div style={barStyle} aria-hidden="true" />
      {/* Label row: plain-English name + the YAML path as a code chip on the
          side. The chip is hidden until the field is hovered/focused (see
          `.dg-field .dg-yaml-path` in theme.css) so the bold label is the only
          thing competing for the eye at rest. */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          marginBottom: 6,
          flexWrap: 'wrap',
        }}
      >
        <span
          style={{ color: TEXT, fontSize: 13.5, fontWeight: 600, letterSpacing: '-0.005em' }}
        >
          {label}
        </span>
        {yamlPath && (
          <code
            className="dg-yaml-path"
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 11,
              color: TEXT_FAINT,
              background: BG_CARD,
              border: `1px solid ${BORDER}`,
              borderRadius: 4,
              padding: '1px 6px',
              lineHeight: '18px',
            }}
            title="YAML path for this field"
          >
            {yamlPath}
          </code>
        )}
        {ownerBadge && (
          <Tag
            style={{
              fontSize: 10,
              fontFamily: 'var(--font-mono)',
              margin: 0,
              padding: '0 6px',
              lineHeight: '16px',
              borderRadius: 4,
              background: BG_CARD,
            }}
          >
            {ownerBadge}
          </Tag>
        )}
      </div>
      {/* Input — hugs the label above it */}
      <div>{children}</div>
      {/* Help text — clearly subordinate: smaller, fainter, tight line-height.
          Example chips share this row. */}
      {(helpText || defaultPlaceholder || (examples && examples.length > 0)) && (
        <div
          style={{
            marginTop: 6,
            fontSize: 12.5,
            color: TEXT_MUTED,
            lineHeight: 1.4,
            display: 'flex',
            flexWrap: 'wrap',
            gap: 8,
            alignItems: 'center',
          }}
        >
          {helpText && <span>{helpText}</span>}
          {defaultPlaceholder && (
            <span
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 11,
                color: TEXT_FAINT,
              }}
              title="Runtime default when this field is omitted"
            >
              default: {defaultPlaceholder}
            </span>
          )}
          {examples && examples.length > 0 && (
            <span style={{ display: 'inline-flex', gap: 4, flexWrap: 'wrap' }}>
              {examples.map((ex, i) => (
                <button
                  key={i}
                  type="button"
                  onClick={() => onExampleClick?.(ex)}
                  disabled={!onExampleClick}
                  style={{
                    background: 'transparent',
                    border: `1px dashed ${TEXT_MUTED}`,
                    borderRadius: 4,
                    padding: '1px 6px',
                    cursor: onExampleClick ? 'pointer' : 'default',
                    color: TEXT_MUTED,
                    fontFamily: 'var(--font-mono)',
                    fontSize: 11,
                  }}
                  title={onExampleClick ? `Use "${ex}" as the value` : `Example value "${ex}"`}
                >
                  {ex}
                </button>
              ))}
            </span>
          )}
        </div>
      )}
    </div>
  );
}
