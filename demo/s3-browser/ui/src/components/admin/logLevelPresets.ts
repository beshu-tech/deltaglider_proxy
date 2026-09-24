/**
 * Log-level presets for the System → Logging card, and the one rule for
 * which radio button shows. Pure (no React) so it is unit-tested.
 */
export const LOG_LEVEL_PRESETS = [
  { label: 'Error', value: 'deltaglider_proxy=error,tower_http=error' },
  { label: 'Warn', value: 'deltaglider_proxy=warn,tower_http=warn' },
  { label: 'Info', value: 'deltaglider_proxy=info,tower_http=info' },
  { label: 'Debug', value: 'deltaglider_proxy=debug,tower_http=debug' },
  { label: 'Trace', value: 'deltaglider_proxy=trace,tower_http=trace' },
] as const;

function normaliseFilter(filter: string): string {
  return filter
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)
    .sort()
    .join(',');
}

export function findMatchingPreset(logLevel: string): string | null {
  const canon = normaliseFilter(logLevel);
  for (const p of LOG_LEVEL_PRESETS) {
    if (normaliseFilter(p.value) === canon) return p.value;
  }
  return null;
}

/** Radio value of the "Custom" button. */
export const CUSTOM_LOG_LEVEL = '__custom__';

/**
 * Which radio shows, derived from the value on every render (never a sticky
 * flag). `customPicked` is the operator's click on "Custom" while the value
 * still matches a preset; the caller clears it on a preset click and on
 * Discard, so a discarded custom edit shows its preset again.
 */
export function logLevelRadio(
  logLevel: string | undefined,
  customPicked: boolean
): { value: string | null; custom: boolean } {
  const preset = logLevel ? findMatchingPreset(logLevel) : null;
  const custom = customPicked || (!!logLevel && preset === null);
  return { value: custom ? CUSTOM_LOG_LEVEL : preset, custom };
}
