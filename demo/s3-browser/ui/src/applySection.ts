/**
 * applySection — the headless half of the section apply protocol.
 *
 * One section PUT with `If-Match`, then the bookkeeping every caller needs:
 * sibling editors of the section in this tab follow the version this PUT
 * moved (sectionVersionBus), and the cached full config is invalidated.
 * useSectionEditor's confirmApply and the one-shot storage PUTs in
 * BackendsPanel (encryption change, clear legacy key) both go through it,
 * so the 409/bus/cache rules live once.
 *
 * A 409 throws `ConfigConflictError` (see `SECTION_CONFLICT_TITLE` for the
 * wording); other transport errors throw as usual.
 */
import type { QueryClient } from '@tanstack/react-query';
import type { SectionApplyResponse, SectionName } from './adminApi';
import { ConfigConflictError, putSection } from './adminApi';
import { normalizeUiError } from './errorHandling';
import { qk } from './queries/keys';
import { sectionVersionAdvanced } from './sectionVersionBus';

/** What an operator reads when a section PUT gets a 409. */
export const SECTION_CONFLICT_TITLE = 'This section changed in another tab or by another admin';

export async function applySection<Wire>(
  queryClient: QueryClient,
  section: SectionName,
  body: Wire,
  version: string | null,
): Promise<SectionApplyResponse & { version?: string | null }> {
  const resp = await putSection<Wire>(section, body, version);
  if (!resp.ok) return resp;
  if (version && resp.version) sectionVersionAdvanced(section, version, resp.version);
  // Other panels read the full config through the cached `qk.config()`
  // query; a section PUT changed server truth.
  void queryClient.invalidateQueries({ queryKey: qk.config() });
  return resp;
}

/**
 * The error text for a failed one-shot section apply (a PUT built from a
 * fresh GET): a 409 says what happened and that a retry is safe, not the
 * caller's generic label.
 */
export function sectionApplyErrorText(e: unknown, fallback: string): string {
  if (e instanceof ConfigConflictError) {
    return `${SECTION_CONFLICT_TITLE}. Nothing is applied. Try again: the next try starts from the new version.`;
  }
  return normalizeUiError(e, fallback);
}
