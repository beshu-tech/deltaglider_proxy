import { useState, type ReactNode } from 'react';
import { Input, type InputProps } from 'antd';
import { useColors } from '../ThemeContext';
import { parseTypedInteger } from './jobs/ruleFieldErrors';

/**
 * Shared React primitives for the storage sub-panels (Lifecycle / Replication /
 * Buckets). Pure helpers (lineList / lines / formRow) live in the
 * sibling `ruleEditorHelpers.ts`.
 *
 * The old bespoke `Field` labelled-wrapper was retired in favour of the
 * canonical `FormField` (label + YAML-path chip + help text + override bar) so
 * storage rule forms match every other admin form and always carry summonable
 * help.
 */

/**
 * Collapsible HTML5 `<details>` block. The summary is the title in sentence
 * case (13px, 600), with an optional `hint` as a muted second line that says
 * what is inside, so the reader knows before opening it. `hasError`: a
 * field inside has a validation error, so the block opens and says so (an
 * error inside a closed block is an error nobody sees).
 */
export function AdvancedDisclosure({
  title,
  hint,
  hasError = false,
  children,
}: {
  title: string;
  hint?: string;
  hasError?: boolean;
  children: ReactNode;
}) {
  const { BORDER, TEXT_PRIMARY, TEXT_MUTED, ACCENT_RED } = useColors();
  return (
    <details
      open={hasError || undefined}
      style={{
        marginTop: 16,
        borderTop: `1px solid ${BORDER}`,
        paddingTop: 12,
      }}
    >
      <summary
        style={{
          cursor: 'pointer',
          color: TEXT_PRIMARY,
          fontSize: 13,
          fontWeight: 600,
          userSelect: 'none',
        }}
      >
        <span>{title}</span>
        {hasError && <span style={{ marginLeft: 8, fontWeight: 400, color: ACCENT_RED }}>· a field needs a fix</span>}
        {hint && (
          <span style={{ display: 'block', marginLeft: 16, fontSize: 12, fontWeight: 400, color: TEXT_MUTED }}>
            {hint}
          </span>
        )}
      </summary>
      <div style={{ marginTop: 12 }}>
        {children}
      </div>
    </details>
  );
}

/**
 * A whole-number field of a rule: a text input that stores what was typed
 * (see `parseTypedInteger`), so "-3" stays "-3" with an error under it
 * instead of turning into 1. The text stays as typed until the value
 * changes from outside (Discard, a clone).
 */
export function RuleNumberInput({
  value,
  onChange,
  ...rest
}: Omit<InputProps, 'value' | 'onChange'> & {
  value: number | null | undefined;
  onChange: (value: number | undefined) => void;
}) {
  const [draft, setDraft] = useState<{ text: string; base: number | undefined } | null>(null);
  const stored = value ?? undefined;
  const text =
    draft && Object.is(draft.base, stored) ? draft.text : stored === undefined || Number.isNaN(stored) ? '' : String(stored);
  return (
    <Input
      {...rest}
      inputMode="numeric"
      value={text}
      onChange={(e) => {
        const next = parseTypedInteger(e.target.value);
        setDraft({ text: e.target.value, base: next });
        onChange(next);
      }}
    />
  );
}
