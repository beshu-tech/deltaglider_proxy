import type { CSSProperties } from 'react';

/**
 * Pure editor helpers shared by the storage sub-panels (Lifecycle / Replication
 * / Buckets). These were copy-pasted byte-for-byte across panels; collapsing
 * them onto one module keeps the panels in lock-step. The React components that
 * accompany them (Field / AdvancedDisclosure) live in `ruleEditorFields.tsx`
 * so react-refresh's only-export-components rule stays happy.
 */

/**
 * Split a textarea value into a trimmed, blank-filtered list of lines.
 *
 * Blank entries are dropped on purpose: the backend rejects empty-string globs,
 * so "preserve blanks" is never the right behaviour here.
 */
export function lineList(value: string): string[] {
  return value
    .split('\n')
    .map((s) => s.trim())
    .filter(Boolean);
}

/** Inverse of {@link lineList}: join a list back into a textarea value. */
export function lines(value: string[]): string {
  return value.join('\n');
}

/**
 * Build the repeated `{ display: 'flex', alignItems: 'center', gap }` row style.
 * Pass `extra` to override (e.g. `flexWrap`, `flexDirection`, margins).
 */
export function formRow(gap: number, extra?: CSSProperties): CSSProperties {
  return { display: 'flex', alignItems: 'center', gap, ...extra };
}
