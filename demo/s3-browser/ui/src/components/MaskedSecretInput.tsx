/**
 * MaskedSecretInput — the single shared masked-secret field, unifying the two
 * incompatible idioms that had been copy-pasted across the admin UI.
 *
 * Both render an AntD `Input.Password` (a real secret, hidden by default) but
 * they answer "what does an empty input mean?" differently — and getting that
 * wrong silently clears or fails to rotate a credential. So the meaning is a
 * required prop, never inferred:
 *
 * - **`mode="sentinel"`** — the server masks the stored value to the
 *   {@link REDACTED_SENTINEL} (`"__redacted__"`) on GET. While the field still
 *   carries the sentinel it is shown EMPTY with a "unchanged — type to replace"
 *   placeholder; the consumer keeps the sentinel in its form state and passes it
 *   through untouched on save (the server restores the real value). Typing emits
 *   the typed string AND flips the consumer's `masked` flag off (now a real,
 *   operator-entered value). Used for: webhook header values, Slack bot token.
 *
 *   In sentinel mode the component takes `masked` + the raw `value`. It shows
 *   `''` while masked (never the sentinel, never a real token) and the live
 *   typed value once unmasked. `onChange` always emits the operator's literal
 *   input; the consumer decides how to fold that back into form state.
 *
 * - **`mode="blank-keeps"`** — there is NO sentinel; a blank input simply means
 *   "keep the existing secret", a non-blank input rotates it. The field shows
 *   `value` verbatim with a "(leave blank to keep existing)" placeholder.
 *
 * - **`mode="new"`** — a create form: there is no existing secret to keep, so
 *   a blank input never means "keep". The consumer decides what blank means
 *   (rejected, or auto-generated) and says so with its own placeholder.
 *
 * `autoComplete` defaults to `"new-password"` in every mode, so a browser
 * password manager does not fill a saved login password into a credential
 * field (and then save or rotate it). Callers can override it.
 *
 * The show/hide control is a real `<button>` named "Show secret" / "Hide
 * secret" (aria-pressed), with the shared focus ring (UX harvest D3): AntD's
 * Input.Password toggle is a focusable `span` with a ring of its own.
 *
 * The font is the form's font, like every other input (a secret is not code);
 * a caller may pass its own `style`. The ARIA and `status` props that a
 * FormField gives its control (label, error link, invalid state) pass through.
 */
import { useState, type CSSProperties } from 'react';
import { Input } from 'antd';
import { EyeInvisibleOutlined, EyeOutlined } from '@ant-design/icons';

// NOTE: the secret-mask sentinel itself (`__redacted__`, mirrored in
// `src/config.rs` + `webhookDeliveryPayload.ts`) never appears here — this
// component takes a `masked` boolean instead of the raw sentinel string, so the
// sentinel pass-through stays entirely in the consumers' form/payload state.

interface BaseProps {
  /** The operator's literal input is always emitted here. */
  onChange: (value: string) => void;
  /** Placeholder override. Defaults are mode-appropriate. */
  placeholder?: string;
  size?: 'small' | 'middle' | 'large';
  style?: CSSProperties;
  autoComplete?: string;
  /**
   * Render a plain (always-visible) `Input` instead of `Input.Password`. Used
   * by the webhook header VALUE field, which historically showed the typed
   * value in the clear (it's only masked-empty while still carrying the
   * sentinel). The round-trip semantics are identical either way — `reveal`
   * only controls whether the dots-toggle eye is present.
   */
  reveal?: boolean;
  /** Forwarded to the input, so a FormField label names it. */
  id?: string;
  disabled?: boolean;
  autoFocus?: boolean;
  onPressEnter?: () => void;
  'aria-label'?: string;
  'aria-labelledby'?: string;
  /** Set by FormField: the help text and the error under the field. */
  'aria-describedby'?: string;
  /** Set by FormField when the field has an error. */
  'aria-invalid'?: boolean;
  /** Set by FormField when the field has an error (red border). */
  status?: 'error';
}

interface SentinelProps extends BaseProps {
  mode: 'sentinel';
  /** The raw form value (may equal the sentinel while masked). */
  value: string;
  /** True while `value` is still the server sentinel (untouched secret). */
  masked: boolean;
}

interface BlankKeepsProps extends BaseProps {
  mode: 'blank-keeps';
  /** Shown verbatim; blank means "keep existing". */
  value: string;
  masked?: never;
}

interface NewSecretProps extends BaseProps {
  mode: 'new';
  /** Shown verbatim; no existing secret stands behind a blank value. */
  value: string;
  masked?: never;
}

type MaskedSecretInputProps = SentinelProps | BlankKeepsProps | NewSecretProps;

const SENTINEL_PLACEHOLDER = '•••••••• (unchanged — type to replace)';
const BLANK_KEEPS_PLACEHOLDER = '(leave blank to keep existing)';
const NEW_PLACEHOLDER = 'Secret';

export default function MaskedSecretInput(props: MaskedSecretInputProps) {
  const { onChange, placeholder, size, style, autoComplete = 'new-password', reveal } = props;

  // In sentinel mode a masked field renders EMPTY (never the sentinel, never a
  // real token); once the operator types it unmasks and shows the live value.
  const shownValue = props.mode === 'sentinel' && props.masked ? '' : props.value;

  const resolvedPlaceholder =
    placeholder ??
    (props.mode === 'sentinel'
      ? props.masked
        ? SENTINEL_PLACEHOLDER
        : 'Bearer …'
      : props.mode === 'new'
        ? NEW_PLACEHOLDER
        : BLANK_KEEPS_PLACEHOLDER);

  const [shown, setShown] = useState(false);
  const toggle = reveal ? undefined : (
    <button
      type="button"
      className="btn-reset dg-secret-toggle"
      aria-label={shown ? 'Hide secret' : 'Show secret'}
      aria-pressed={shown}
      title={shown ? 'Hide secret' : 'Show secret'}
      disabled={props.disabled}
      onClick={() => setShown((v) => !v)}
      style={{ padding: 2, borderRadius: 4, color: 'inherit', lineHeight: 1 }}
    >
      {shown ? <EyeInvisibleOutlined aria-hidden="true" /> : <EyeOutlined aria-hidden="true" />}
    </button>
  );

  return (
    <Input
      type={reveal || shown ? 'text' : 'password'}
      suffix={toggle}
      spellCheck={false}
      disabled={props.disabled}
      autoFocus={props.autoFocus}
      onPressEnter={props.onPressEnter}
      value={shownValue}
      onChange={(e) => onChange(e.target.value)}
      placeholder={resolvedPlaceholder}
      size={size}
      autoComplete={autoComplete}
      id={props.id}
      aria-label={props['aria-label']}
      aria-labelledby={props['aria-labelledby']}
      aria-describedby={props['aria-describedby']}
      aria-invalid={props['aria-invalid']}
      status={props.status}
      style={style}
    />
  );
}
