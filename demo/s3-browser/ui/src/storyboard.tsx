// Dev-only storyboard for the replication parity Verify verdict.
// Renders ParityResult against fake ParityOutcome fixtures — no backend.
// Served by Vite at /storyboard.html (npm run dev). Not in the prod bundle.
import React from 'react';
import ReactDOM from 'react-dom/client';
import { useState } from 'react';
import ThemeProvider from './ThemeProvider';
import { useColors } from './ThemeContext';
import { ParityResult } from './components/jobs/VerifyTab';
import OutcomeMeter from './components/jobs/OutcomeMeter';
import type {
  ActionableSummary,
  ParityOutcome,
  ParityFinding,
  ReasonCode,
  Remediation,
} from './adminApi';
import './theme.css';

// The causes the fixtures below exercise — keeps the storyboard honest about
// which remediation reasons get a visual once-over (typed by ReasonCode).
const DEMOED_REASONS: ReasonCode[] = [
  'never_copied',
  'copy_failing',
  'source_modified_after_copy',
  'dest_modified_after_copy',
  'diverged_unknown_age',
  'rule_owned_orphan_source_deleted',
  'foreign_orphan',
];

const now = Math.floor(Date.now() / 1000);
const ago = (mins: number) => now - mins * 60;

const f = (
  key: string,
  kind: ParityFinding['kind'],
  detail: string,
  remediation?: Remediation,
  verifier?: ParityFinding['verifier'],
  unverifiable = false
): ParityFinding => ({ key, kind, detail, verifier, unverifiable, remediation });

const noActionable: ActionableSummary = {
  rerun_fixes: 0,
  rerun_conditional: 0,
  needs_manual: 0,
  copy_failing: 0,
  foreign_orphans: 0,
};

// ── Remediation fixtures (one per interesting verdict) ──────────────────────
const remNeverCopied: Remediation = {
  reason: 'never_copied',
  rerun_helps: { verdict: 'yes' },
  fix: { action: 'run_now' },
  reason_detail: 'present on source, never copied to destination',
  fix_detail: 're-run the rule — a missing key is always copied',
};
const remCopyFailing: Remediation = {
  reason: 'copy_failing',
  rerun_helps: { verdict: 'no', why: 'copy_keeps_failing' },
  fix: { action: 'resolve_copy_failure' },
  reason_detail: 'copy has failed 4 time(s); last error: AccessDenied writing to b2',
  fix_detail: 'fix the underlying copy error (permissions, backend reachability), then re-run',
};
const remForeignOrphan: Remediation = {
  reason: 'foreign_orphan',
  rerun_helps: { verdict: 'no', why: 'foreign_not_ours' },
  fix: { action: 'delete_from_dest', foreign: true },
  reason_detail: 'destination object was not written by this rule',
  fix_detail: "we never touch foreign objects — delete it manually if it shouldn't be there",
};
const remOrphanNeedsDelete: Remediation = {
  reason: 'rule_owned_orphan_source_deleted',
  rerun_helps: { verdict: 'no', why: 'orphan_needs_delete' },
  fix: { action: 'enable_replicate_deletes' },
  reason_detail: 'we copied this key and the source is gone, but mirror-delete is disabled',
  fix_detail: 'enable replicate_deletes on the rule, then re-run to remove it',
};
const remSrcNewer: Remediation = {
  reason: 'source_modified_after_copy',
  rerun_helps: { verdict: 'yes' },
  fix: { action: 'run_now' },
  reason_detail: 'source is newer than the destination copy',
  fix_detail: 're-run the rule — newer-wins copies the newer source',
};
// THE LIE: skip-if-dest-exists + mismatch — re-run provably skips.
const remSkipLie: Remediation = {
  reason: 'dest_modified_after_copy',
  rerun_helps: { verdict: 'no', why: 'policy_skips_existing_dest' },
  fix: { action: 'copy_overwrite' },
  reason_detail: 'source and destination differ, but the rule skips existing destination keys',
  fix_detail: 're-run will NOT fix this — overwrite manually, or change the policy to source-wins',
};
const remChangePolicy: Remediation = {
  reason: 'dest_modified_after_copy',
  rerun_helps: { verdict: 'no', why: 'policy_skips_existing_dest' },
  fix: { action: 'change_conflict_policy', to: 'source-wins' },
  reason_detail: 'source and destination differ, but the rule skips existing destination keys',
  fix_detail: 'change the conflict policy to source-wins so re-runs overwrite the destination',
};
const remConditional: Remediation = {
  reason: 'diverged_unknown_age',
  rerun_helps: { verdict: 'conditional', why: 'newer_wins_depends_on_timestamps' },
  fix: { action: 'run_now' },
  reason_detail: 'content differs and an object age is unknown — re-run copies only if source is newer',
  fix_detail: 're-run the rule — it copies only when the source proves newer',
};

const base: ParityOutcome = {
  rule_name: 'copyHzToB2',
  source_bucket: 'beshu',
  dest_bucket: 'beshu-b2',
  source_objects: 1284,
  dest_objects: 1284,
  matched: 1284,
  missing_on_dest: 0,
  orphan_on_dest: 0,
  checksum_mismatch: 0,
  unverifiable: 0,
  truncated: false,
  in_sync: true,
  scanned_at: ago(4),
  conflict_policy: 'newer-wins',
  replicate_deletes: false,
  actionable: noActionable,
  missing_samples: [],
  orphan_samples: [],
  mismatch_samples: [],
};

const inSync: ParityOutcome = { ...base };

const truncated: ParityOutcome = {
  ...base,
  source_objects: 100000,
  dest_objects: 100000,
  matched: 100000,
  truncated: true,
  in_sync: false, // truncated → not provably in sync
  scanned_at: ago(11),
};

const amber: ParityOutcome = {
  ...base,
  matched: 1279,
  missing_on_dest: 3,
  orphan_on_dest: 2,
  in_sync: false,
  scanned_at: ago(1),
  // 2 fixable (never-copied) · 1 failing · 2 orphans needing manual action.
  actionable: { rerun_fixes: 2, rerun_conditional: 0, needs_manual: 2, copy_failing: 1, foreign_orphans: 1 },
  missing_samples: [
    f('ror/builds/1.28.1/readonlyrest-1.28.1_es6.2.2.zip', 'missing_on_dest', 'absent on destination', remNeverCopied),
    f('ror/builds/1.28.0/readonlyrest-1.28.0_es7.9.2.zip', 'missing_on_dest', 'absent on destination', remNeverCopied),
    f('ror/builds/1.28.0/readonlyrest-1.28.0_es7.5.0.zip.sha1', 'missing_on_dest', 'absent on destination', remCopyFailing),
  ],
  orphan_samples: [
    f('ror/builds/legacy/old-artifact-2019.zip', 'orphan_on_dest', 'on destination, not in source', remForeignOrphan),
    f('ror/scratch/tmp-upload.bin', 'orphan_on_dest', 'on destination, not in source', remOrphanNeedsDelete),
  ],
};

// newer-wins: mismatches resolve by timestamp (some re-run, some conditional).
const red: ParityOutcome = {
  ...base,
  matched: 1280,
  checksum_mismatch: 3,
  unverifiable: 4,
  in_sync: false,
  scanned_at: ago(0),
  actionable: { rerun_fixes: 1, rerun_conditional: 1, needs_manual: 0, copy_failing: 1, foreign_orphans: 0 },
  mismatch_samples: [
    f('ror/builds/1.28.0/readonlyrest-1.28.0_es8.0.0.zip', 'checksum_mismatch', 'sha256 differ', remSrcNewer, 'sha256'),
    f('ror/builds/1.28.0/manifest.json', 'checksum_mismatch', 'size 4096 vs 4101', remConditional),
    f('ror/builds/1.28.0/checksums.txt', 'checksum_mismatch', 'sha256 differ', remCopyFailing, 'sha256'),
  ],
};

// THE LIE made visible: a skip-if-dest-exists rule with content mismatches.
// Re-run provably skips — the guidance must say so, not "just re-run".
const skipLie: ParityOutcome = {
  ...base,
  matched: 1281,
  checksum_mismatch: 3,
  in_sync: false,
  scanned_at: ago(2),
  conflict_policy: 'skip-if-dest-exists',
  actionable: { rerun_fixes: 0, rerun_conditional: 0, needs_manual: 3, copy_failing: 0, foreign_orphans: 0 },
  mismatch_samples: [
    f('ror/builds/1.28.0/readonlyrest-1.28.0_es8.0.0.zip', 'checksum_mismatch', 'sha256 differ', remSkipLie, 'sha256'),
    f('ror/builds/1.28.0/manifest.json', 'checksum_mismatch', 'size 4096 vs 4101', remChangePolicy),
    f('ror/builds/1.28.0/notes.txt', 'checksum_mismatch', 'sha256 differ', remSkipLie, 'sha256'),
  ],
};

const FIXTURES: { label: string; outcome: ParityOutcome }[] = [
  { label: 'In sync (the deliverable)', outcome: inSync },
  { label: 'In sync — scan truncated', outcome: truncated },
  { label: 'Differences — missing & extra (amber)', outcome: amber },
  { label: 'Differences — checksum mismatch (red)', outcome: red },
  { label: 'skip-if-dest-exists (the lie)', outcome: skipLie },
];

function Storyboard() {
  const c = useColors();
  const [i, setI] = useState(0);
  return (
    <div style={{ minHeight: '100vh', background: c.BG_BASE, color: c.TEXT_PRIMARY, padding: 32 }}>
      <h2 style={{ marginBottom: 4 }}>Replication parity — Verify verdict storyboard</h2>
      <p style={{ color: c.TEXT_SECONDARY, marginTop: 0 }}>
        Fake data. The same <code>ParityResult</code> the Jobs → Verify tab renders.
      </p>
      <p style={{ color: c.TEXT_MUTED, marginTop: 0, fontSize: 12 }}>
        Reasons demonstrated: {DEMOED_REASONS.join(', ')}
      </p>
      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', margin: '16px 0 24px' }}>
        {FIXTURES.map((fx, idx) => (
          <button
            key={fx.label}
            onClick={() => setI(idx)}
            style={{
              padding: '6px 12px',
              borderRadius: 6,
              cursor: 'pointer',
              border: `1px solid ${idx === i ? c.ACCENT_BLUE : c.BORDER}`,
              background: idx === i ? c.ACCENT_BLUE : c.BG_ELEVATED,
              color: idx === i ? '#fff' : c.TEXT_PRIMARY,
            }}
          >
            {fx.label}
          </button>
        ))}
      </div>
      <div style={{ maxWidth: 880, background: c.BG_BASE, padding: 16, borderRadius: 12, border: `1px solid ${c.BORDER}` }}>
        <ParityResult
          outcome={FIXTURES[i].outcome}
          onReverify={() => {}}
          onRunNow={() => {}}
          c={c}
        />
      </div>

      <h2 style={{ margin: '40px 0 4px' }}>Runs — progress bar</h2>
      <p style={{ color: c.TEXT_SECONDARY, marginTop: 0, fontSize: 13 }}>
        The Jobs → Runs table bar. Scaled to work DONE (copied+errors): green =
        copied, red = errors, blank = skipped (already in sync). Number = copied;
        full breakdown on hover.
      </p>
      <table style={{ borderCollapse: 'collapse', maxWidth: 560 }}>
        <thead>
          <tr style={{ textAlign: 'left', color: c.TEXT_MUTED, fontSize: 12 }}>
            <th style={{ padding: '6px 0' }}>Case</th>
            <th style={{ width: 220 }}>Progress</th>
          </tr>
        </thead>
        <tbody>
          {RUN_BAR_CASES.map((r) => (
            <tr key={r.label} style={{ borderTop: `1px solid ${c.BORDER}` }}>
              <td style={{ padding: '10px 16px 10px 0', fontSize: 13 }}>
                {r.label}
                <div style={{ color: c.TEXT_MUTED, fontSize: 11 }}>
                  scanned {r.scanned} · copied {r.copied} · err {r.errors} · skip {r.skipped}
                </div>
              </td>
              <td style={{ width: 220 }}>
                <OutcomeMeter
                  scanned={r.scanned}
                  copied={r.copied}
                  errors={r.errors}
                  skipped={r.skipped}
                  status={r.status}
                  percent={r.percent}
                />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** OutcomeMeter fixtures — real-shaped cases across the calm→loud spectrum so
 *  the attention-budget treatment (quiet in-sync vs loud error) is legible. */
const RUN_BAR_CASES: Array<{
  label: string;
  scanned: number;
  copied: number;
  errors: number;
  skipped: number;
  status: string;
  percent?: number | null;
}> = [
  { label: 'Incremental (all in sync)', scanned: 3400, copied: 0, errors: 0, skipped: 3400, status: 'succeeded' },
  { label: 'Copied a few', scanned: 3400, copied: 7, errors: 0, skipped: 3393, status: 'succeeded' },
  { label: 'One transient error', scanned: 4100, copied: 10, errors: 1, skipped: 4089, status: 'succeeded' },
  { label: 'Failed, nothing copied', scanned: 58, copied: 0, errors: 58, skipped: 0, status: 'failed' },
  { label: 'Balanced copy (60/40)', scanned: 200, copied: 120, errors: 80, skipped: 0, status: 'succeeded' },
  { label: 'Running (known %)', scanned: 1000, copied: 400, errors: 0, skipped: 0, status: 'running', percent: 40 },
  { label: 'Running (unknown %)', scanned: 0, copied: 120, errors: 0, skipped: 0, status: 'running', percent: null },
  { label: 'Cancelled', scanned: 500, copied: 30, errors: 0, skipped: 470, status: 'cancelled' },
];

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <ThemeProvider>
      <Storyboard />
    </ThemeProvider>
  </React.StrictMode>,
);
