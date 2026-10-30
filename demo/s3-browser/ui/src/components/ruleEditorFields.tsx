import type { ReactNode } from 'react';
import { useColors } from '../ThemeContext';

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
 * what is inside, so the reader knows before opening it.
 */
export function AdvancedDisclosure({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  const { BORDER, TEXT_PRIMARY, TEXT_MUTED } = useColors();
  return (
    <details
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
