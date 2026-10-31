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
 *  │     Help text below the input — one sentence. Try: example, example
 *  └──
 *         ↑
 *    Amber left bar when `overrideActive` is true.
 * ```
 *
 * ## Intentional constraints
 *
 * - **Does not render the input itself.** Callers pass whatever input
 *   they need (`<Input>`, `<InputNumber>`, `<Switch>`, `<Select>`,
 *   `<Radio.Group>`) as children. FormField owns the chrome; the caller
 *   owns control behaviour. Keeps FormField free of input-type branches.
 * - **Placeholder ≠ value.** The `defaultPlaceholder` prop is a label
 *   shown when the field is empty; it does NOT pre-fill the field. The
 *   invariant "omitted = default" in the YAML is preserved this way.
 * - **Examples are suggestions, not assignments.** They render as links
 *   ("Try: 256, 1024"), never as boxes that look like (disabled) inputs
 *   (UX harvest D65). Clicking one calls `onExampleClick(value)`; the
 *   caller decides whether to write it into the form state. Without
 *   `onExampleClick` they are plain text ("For example: …"). That avoids FormField needing to know about
 *   the form controller (react-hook-form vs. uncontrolled input).
 */
import { Children, Fragment, cloneElement, isValidElement, useId } from 'react';
import type { CSSProperties, ReactElement, ReactNode } from 'react';
import { Tag } from 'antd';
import { useColors } from '../ThemeContext';
import { SPACE } from './shared-styles';
import { useEnvOverride } from '../queries/config';
import { envOverrideHelp, envOverrideSource, envOverrideText } from '../envOverrides';

interface FormFieldProps {
  /**
   * Plain-English field name. E.g. "Reference-cache size (MiB)".
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
  /**
   * For a setting that exists ONLY as an environment variable (no YAML key):
   * its name. The field then always renders read-only with an env badge.
   */
  envVar?: string;
  /** Override indicator bar colour; defaults to amber. */
  overrideColour?: string;
  /** The input element itself. */
  children: ReactNode;
  /**
   * The id of the control when it is not the single direct child (e.g. a
   * Switch next to a status text): the label then points at it.
   */
  controlId?: string;
  /** Optional override for the outer container style. */
  style?: CSSProperties;
  /**
   * Validation error of this field (one sentence). Shown under the input in
   * the error colour and announced; the control gets `aria-invalid`,
   * `aria-describedby` and AntD's `status="error"` (red border).
   */
  error?: string;
}

interface LabelledProps {
  id?: string;
  'aria-label'?: string;
  'aria-labelledby'?: string;
  'aria-describedby'?: string;
  'aria-invalid'?: boolean;
  status?: 'error';
}

/**
 * Give the single control child the label: `id` (for `<label htmlFor>`)
 * when it has none, and `aria-labelledby` for controls a `<label>` cannot
 * name (a radio group's div), unless the child carries its own
 * `aria-label`. AntD Input/InputNumber/Select/Switch forward
 * both to their focusable element. Anything else renders untouched.
 */
function labelChild(
  children: ReactNode,
  controlId: string,
  labelId: string,
  describedBy: string | undefined,
  invalid: boolean,
): ReactNode {
  if (Children.count(children) !== 1 || !isValidElement(children) || children.type === Fragment) {
    return children;
  }
  const child = children as ReactElement<LabelledProps>;
  const ownDescribedBy = child.props['aria-describedby'];
  const allDescribedBy = [describedBy, ownDescribedBy].filter(Boolean).join(' ') || undefined;
  return cloneElement(child, {
    id: child.props.id ?? controlId,
    // A caller's own aria-label wins (aria-labelledby would override it).
    'aria-labelledby': child.props['aria-label'] ? undefined : (child.props['aria-labelledby'] ?? labelId),
    'aria-describedby': allDescribedBy,
    // Only an invalid field gets the two error props, so a valid field's
    // control (a Switch, a Radio.Group) never receives a prop it ignores.
    ...(invalid ? { 'aria-invalid': true, status: 'error' as const } : {}),
  });
}

/**
 * A value that the GUI shows but cannot edit (set by an environment
 * variable, or env-only): mono text in a dashed box, ONE look for "set
 * outside the GUI". FormField uses it for env-controlled fields; panels use
 * it as the child of a FormField for env-only settings.
 */
export function ReadOnlyValue({ children, muted = false, testId }: { children: ReactNode; muted?: boolean; testId?: string }) {
  const { TEXT_PRIMARY, TEXT_MUTED, BG_CARD, BORDER } = useColors();
  return (
    <div
      data-testid={testId}
      style={{
        fontFamily: 'var(--font-mono)',
        fontSize: 13,
        color: muted ? TEXT_MUTED : TEXT_PRIMARY,
        background: BG_CARD,
        border: `1px dashed ${BORDER}`,
        borderRadius: 6,
        padding: `${SPACE.xxs}px ${SPACE.sm}px`,
        minHeight: 32,
        boxSizing: 'border-box',
        wordBreak: 'break-all',
        // As wide as the value (a short value is not a full-width bar),
        // never wider than the column.
        display: 'inline-block',
        minWidth: 180,
        maxWidth: '100%',
      }}
    >
      {children}
    </div>
  );
}

/** The id a FormField gives its control: `useId`, unless the child brings one. */
function controlIdOf(children: ReactNode, fallback: string): string {
  return isValidElement<LabelledProps>(children) && children.props.id ? children.props.id : fallback;
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
  envVar,
  overrideColour,
  children,
  style,
  controlId: explicitControlId,
  error,
}: FormFieldProps) {
  const { TEXT_PRIMARY: TEXT, TEXT_MUTED, TEXT_FAINT, BG_CARD, BORDER, ACCENT_AMBER, ACCENT_RED, ACCENT_BLUE } = useColors();
  // THE env rule: a field that a `DGP_*` variable controls shows the
  // effective value read-only with a "from env" badge, never an empty or
  // editable-looking input (an edit would be overridden again at restart).
  const envOverride = useEnvOverride(yamlPath, envVar);
  const envBadge = envOverride
    ? `from env ${envOverrideSource(envOverride)}`
    : envVar
      ? `set with env ${envVar}`
      : undefined;
  const baseId = useId();
  const labelId = `${baseId}-label`;
  const helpId = `${baseId}-help`;
  const errorId = `${baseId}-error`;
  // The error first: a screen reader reads what is wrong before the hint.
  const describedBy = [error ? errorId : '', helpText ? helpId : ''].filter(Boolean).join(' ') || undefined;
  const controlId = explicitControlId ?? controlIdOf(children, `${baseId}-control`);
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
          gap: SPACE.xs,
          marginBottom: SPACE.xxs,
          flexWrap: 'wrap',
        }}
      >
        <label
          id={labelId}
          htmlFor={envOverride ? undefined : controlId}
          style={{ color: TEXT, fontSize: 13.5, fontWeight: 600, letterSpacing: '-0.005em' }}
        >
          {label}
        </label>
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
              padding: `1px ${SPACE.xxs}px`,
              lineHeight: '18px',
            }}
            title="YAML path for this field"
          >
            {yamlPath}
          </code>
        )}
        {(envBadge ?? ownerBadge) && (
          <Tag
            style={{
              fontSize: 10,
              fontFamily: 'var(--font-mono)',
              margin: 0,
              padding: `0 ${SPACE.xxs}px`,
              lineHeight: '16px',
              borderRadius: 4,
              background: BG_CARD,
            }}
          >
            {envBadge ?? ownerBadge}
          </Tag>
        )}
      </div>
      {/* Input — hugs the label above it. An env-controlled field shows its
          effective value instead of the editor. */}
      {envOverride ? (
        <ReadOnlyValue testId="env-override-value" muted={envOverride.secret}>
          {envOverrideText(envOverride)}
        </ReadOnlyValue>
      ) : (
        <div>{explicitControlId ? children : labelChild(children, controlId, labelId, describedBy, Boolean(error))}</div>
      )}
      {/* Field error — directly under the input, so it is next to the field
          the user looks at (a banner at the top of the form can be scrolled
          away). role="alert" announces it when it appears. */}
      {error && !envOverride && (
        <div
          id={errorId}
          role="alert"
          data-testid="field-error"
          style={{ marginTop: SPACE.xxs, fontSize: 12.5, color: ACCENT_RED, lineHeight: 1.4 }}
        >
          {error}
        </div>
      )}
      {/* Help text — clearly subordinate: smaller, fainter, tight line-height.
          Example chips share this row. */}
      {envOverride && (
        <div style={{ marginTop: SPACE.xxs, fontSize: 12.5, color: TEXT_MUTED, lineHeight: 1.4 }}>
          {envOverrideHelp(envOverride)}
        </div>
      )}
      {(helpText || (!envOverride && (defaultPlaceholder || (examples && examples.length > 0)))) && (
        <div
          style={{
            marginTop: SPACE.xxs,
            fontSize: 12.5,
            color: TEXT_MUTED,
            lineHeight: 1.4,
            display: 'flex',
            flexWrap: 'wrap',
            gap: SPACE.xs,
            alignItems: 'center',
          }}
        >
          {helpText && <span id={helpId}>{helpText}</span>}
          {!envOverride && defaultPlaceholder && (
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
          {!envOverride && examples && examples.length > 0 && (
            <span data-testid="field-examples" style={{ display: 'inline-flex', columnGap: SPACE.xxs, flexWrap: 'wrap', alignItems: 'baseline' }}>
              {/* D65: links, not dashed boxes (those read as disabled inputs). */}
              <span>{onExampleClick ? 'Try:' : 'For example:'}</span>
              {examples.map((ex, i) => (
                <span key={`ex-${ex}`}>
                  {onExampleClick ? (
                    <button
                      type="button"
                      className="dg-example-link"
                      onClick={() => onExampleClick(ex)}
                      style={{
                        background: 'none',
                        border: 'none',
                        padding: 0,
                        cursor: 'pointer',
                        color: ACCENT_BLUE,
                        fontFamily: 'var(--font-mono)',
                        fontSize: 12,
                        textDecoration: 'underline',
                        textDecorationStyle: 'dotted',
                        textUnderlineOffset: 3,
                      }}
                      title={`Use "${ex}" as the value`}
                    >
                      {ex}
                    </button>
                  ) : (
                    <code style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: TEXT_MUTED }}>{ex}</code>
                  )}
                  {i < examples.length - 1 && <span aria-hidden>,</span>}
                </span>
              ))}
            </span>
          )}
        </div>
      )}
    </div>
  );
}
