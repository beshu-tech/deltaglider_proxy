import type { ReactNode } from 'react';
import { useColors } from '../ThemeContext';

/**
 * Shared React primitives for the storage sub-panels (Lifecycle / Replication /
 * Buckets). Pure helpers (lineList / lines / fmtUnix / formRow) live in the
 * sibling `ruleEditorHelpers.ts`.
 *
 * The old bespoke `Field` labelled-wrapper was retired in favour of the
 * canonical `FormField` (label + YAML-path chip + help text + override bar) so
 * storage rule forms match every other admin form and always carry summonable
 * help.
 */

/** Collapsible HTML5 `<details>` block with an uppercase summary header. */
export function AdvancedDisclosure({ title, children }: { title: string; children: ReactNode }) {
  const { BORDER, TEXT_SECONDARY } = useColors();
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
          color: TEXT_SECONDARY,
          fontSize: 12,
          fontWeight: 700,
          letterSpacing: 0.5,
          textTransform: 'uppercase',
          userSelect: 'none',
        }}
      >
        {title}
      </summary>
      <div style={{ marginTop: 12 }}>
        {children}
      </div>
    </details>
  );
}
