/**
 * useSectionEditor — shared editor lifecycle for a config section.
 *
 * Three admin panels (AdmissionPanel, CredentialsModePanel, and every
 * Advanced sub-panel) were each carrying a ~150-LOC copy of the same
 * machinery:
 *
 *   * fetch `getSection(section)` on mount → `resetWith()`
 *   * `useDirtySection` for snapshot/dirty/apply/discard
 *   * Apply flow: `validateSection` → open `ApplyDialog` → on confirm
 *     `putSection` → `markApplied` → refresh
 *   * Snapshot the body at validate time (§F5 fix) so the diff and
 *     the subsequent PUT refer to the same payload even if the user
 *     keeps editing under the dialog
 *   * Unwrap `session-expired` 401s via `onSessionExpired`
 *   * `useApplyHandler` for ⌘S → same action as clicking Apply
 *
 * Keeping three copies in sync was the failure mode — the §F5 fix
 * already had to land three times. This hook is the single place
 * the apply protocol lives.
 *
 * ## Subset vs full body
 *
 * Advanced sub-panels each own a slice of `AdvancedSectionBody`
 * (Caches owns `cache_size_mb`, Logging owns `log_level`, etc.). On
 * GET, they need to filter the server's full body down to their
 * fields; on PUT, the server applies RFC 7396 merge-patch so sibling
 * panels' fields are preserved.
 *
 * AdmissionPanel and CredentialsModePanel own their WHOLE section
 * body — no filtering needed.
 *
 * `pick` is the optional filter. Provide it for subset editing;
 * leave it off for whole-section editing.
 */
import { useCallback, useEffect, useState } from 'react';
import { message } from 'antd';
import type { SectionApplyResponse, SectionName } from './adminApi';
import { getSection, putSection, validateSection } from './adminApi';
import { useApplyHandler, useDirtySection } from './useDirtySection';

interface UseSectionEditorOptions<Wire, Local = Wire> {
  section: SectionName;
  initial: Local;
  onSessionExpired?: () => void;
  /**
   * When set, the fetch-path calls `pick(serverBody)` to produce the
   * local value. Use this when:
   *   (a) the panel owns only a subset of the section's fields; OR
   *   (b) the local shape differs from the wire shape (e.g. the
   *       AdmissionPanel treats the section as a flat array, but the
   *       wire is `{ blocks: [...] }`).
   *
   * Leave off for full-body editing where local === wire.
   */
  pick?: (body: Wire) => Local;
  /**
   * When set, the apply-path calls `toPayload(localValue)` to produce
   * the wire body for validate/PUT. Default: `value as unknown as Wire`
   * (full-body editing).
   */
  toPayload?: (value: Local) => Wire;
  /**
   * Error noun for message.error: "Failed to load $noun section: ...".
   * Default: the section name.
   */
  noun?: string;
}

export interface UseSectionEditorResult<Local> {
  /** Current editable value. */
  value: Local;
  /** Replace the value (bypasses snapshot — callers drive equality). */
  setValue: (next: Local) => void;
  /** Revert to the last-applied snapshot. */
  discard: () => void;
  /** True when `value` differs from the snapshot. */
  isDirty: boolean;
  /** Loading = first GET hasn't resolved yet. */
  loading: boolean;
  /** Error string if the first GET failed (non-401). */
  error: string | null;
  /** ApplyDialog state — pass directly to <ApplyDialog /> props. */
  applyOpen: boolean;
  applyResponse: SectionApplyResponse | null;
  applying: boolean;
  /** Opens the validate → dialog flow. */
  runApply: () => Promise<void>;
  /** Close the dialog without persisting. */
  cancelApply: () => void;
  /** PUT the snapshot the dialog was showing. */
  confirmApply: () => Promise<void>;
  /** Manually re-fetch the section (rare — useful when external state changes). */
  refresh: () => Promise<void>;
}

export function useSectionEditor<Wire, Local = Wire>(
  opts: UseSectionEditorOptions<Wire, Local>
): UseSectionEditorResult<Local> {
  const {
    section,
    initial,
    onSessionExpired,
    pick,
    toPayload,
    noun = section,
  } = opts;

  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const { value, isDirty, setValue, discard, markApplied, resetWith } =
    useDirtySection<Local>(section, initial);

  // Apply-dialog state. `pendingBody` captures the exact body that
  // went to /validate so confirmApply PUTs what the user saw (§F5).
  const [applyOpen, setApplyOpen] = useState(false);
  const [applyResponse, setApplyResponse] = useState<SectionApplyResponse | null>(null);
  const [pendingBody, setPendingBody] = useState<Wire | null>(null);
  const [applying, setApplying] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setLoading(true);
      const body = await getSection<Wire>(section);
      if (pick) {
        // Caller converts wire → local outright (subset OR shape-change).
        resetWith(pick(body));
      } else {
        // Full-body edit path: local === wire. Merge incoming into
        // the initial so absent fields keep their form defaults
        // (e.g. a bare `{}` on a fresh install).
        resetWith({
          ...(initial as unknown as object),
          ...(body as unknown as object),
        } as Local);
      }
      setError(null);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(`Failed to load ${noun} section: ${e instanceof Error ? e.message : 'unknown'}`);
    } finally {
      setLoading(false);
    }
    // `initial` / `resetWith` / `pick` are expected stable across renders.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [section, onSessionExpired, noun]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const buildPayload = useCallback(
    (v: Local): Wire => (toPayload ? toPayload(v) : (v as unknown as Wire)),
    [toPayload]
  );

  const runApply = useCallback(async () => {
    const snapshot = buildPayload(value);
    try {
      const resp = await validateSection<Wire>(section, snapshot);
      setApplyResponse(resp);
      setPendingBody(snapshot);
      setApplyOpen(true);
    } catch (e) {
      message.error(`Validate failed: ${e instanceof Error ? e.message : 'unknown'}`);
    }
  }, [section, buildPayload, value]);

  const cancelApply = useCallback(() => {
    setApplyOpen(false);
    setPendingBody(null);
  }, []);

  const confirmApply = useCallback(async () => {
    if (!pendingBody) return;
    setApplying(true);
    try {
      const resp = await putSection<Wire>(section, pendingBody);
      if (!resp.ok) {
        message.error(resp.error || 'Apply failed');
        return;
      }
      message.success(
        resp.persisted_path ? `Applied + persisted to ${resp.persisted_path}` : 'Applied'
      );
      markApplied();
      setApplyOpen(false);
      setPendingBody(null);
      void refresh();
    } catch (e) {
      message.error(`Apply failed: ${e instanceof Error ? e.message : 'unknown'}`);
      setApplyOpen(false);
      setPendingBody(null);
      void refresh();
    } finally {
      setApplying(false);
    }
  }, [section, pendingBody, markApplied, refresh]);

  // ⌘S wiring: when dirty, ⌘S opens the validate → ApplyDialog sequence.
  useApplyHandler(section, runApply, isDirty);

  return {
    value,
    setValue,
    discard,
    isDirty,
    loading,
    error,
    applyOpen,
    applyResponse,
    applying,
    runApply,
    cancelApply,
    confirmApply,
    refresh,
  };
}
