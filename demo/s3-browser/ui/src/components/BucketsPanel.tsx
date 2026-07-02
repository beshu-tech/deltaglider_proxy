/**
 * BucketsPanel — per-bucket settings, redesigned per the cognitive-load
 * study (docs/plan/storage-ui-cognitive-load.md).
 *
 * The page lists EVERY real bucket (from the S3 list) as a one-line status
 * row showing its EFFECTIVE state — backend (incl. the default), encryption,
 * public access, quota, compression override — and expands to edit. The
 * "policy" is invisible vocabulary: a bucket has settings; a policy exists
 * server-side iff something is overridden. Editing a no-override bucket
 * materialises a row; resetting a bucket to defaults deletes its policy.
 *
 * Policy entries whose bucket doesn't exist (pre-provisioned or stale) are
 * still shown, flagged "bucket not found", with an editable name.
 *
 * ## Merge-patch correctness
 *
 * The storage section PUT deep-merges (RFC 7396): absent keys PRESERVE
 * server values. `buildBucketPayload` therefore receives the BASELINE
 * bucket names (captured at fetch time in `pick`) and emits `name: null`
 * for removed/reset policies, plus explicit per-field nulls — without
 * which un-publicking, un-routing, and policy deletion silently no-op.
 *
 * Uses the section-level storage Apply flow so dirty-state indicators,
 * beforeunload, and Cmd/Ctrl+S behave like the rest of Configuration.
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { Alert, Button, Space, Typography, message } from 'antd';
import { PlusOutlined } from '@ant-design/icons';
import type { AdminConfig } from '../adminApi';
import { useAdminConfig } from '../queries/config';
import { useBackends, useBucketNames } from '../queries/backends';
import { useCardStyles } from './shared-styles';
import ApplyDialog from './ApplyDialog';
import BucketCard from './BucketCard';
import CreateBucketModal from './CreateBucketModal';
import ReencryptProposalModal from './ReencryptProposalModal';
import { useApplyHandler } from '../useDirtySection';
import { useSectionEditor } from '../useSectionEditor';
import { useJobs } from '../queries/jobs';
import { busyJobForBucket } from '../jobsView';
import type { BucketPolicyRow, BucketPolicyPatch, PrefixEntry } from './bucketPolicyPayload';
import { DEFAULT_ROW_FIELDS, buildBucketPayload, freshId, isAllDefaultRow, policyToRow } from './bucketPolicyPayload';

interface Props {
  onSessionExpired?: () => void;
}

type PolicyGet = NonNullable<AdminConfig['bucket_policies']>[string];
/** GET returns full policies; PUT sends the merge-patch (values may be null). */
type BucketWire = { buckets: Record<string, PolicyGet | BucketPolicyPatch | null> };

const { Text } = Typography;

export default function BucketsPanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();

  // Bucket names that had a policy on the server at fetch time — the
  // baseline `buildBucketPayload` diffs against to emit `name: null`
  // deletions. Updated inside `pick`, which runs on mount and after every
  // successful apply (the editor refreshes from server truth).
  const baselineNamesRef = useRef<string[]>([]);

  const {
    value: rows,
    setValue: setRows,
    isDirty: dirty,
    discard,
    applyOpen,
    applyResponse,
    applying,
    runApply: editorRunApply,
    cancelApply,
    confirmApply,
    loading,
    error,
  } = useSectionEditor<BucketWire, BucketPolicyRow[]>({
    section: 'storage',
    dirtyKey: 'storage/buckets',
    initial: [],
    onSessionExpired,
    noun: 'bucket policies',
    pick: (body) => {
      const policies = body.buckets || {};
      baselineNamesRef.current = Object.keys(policies);
      const nextRows = Object.entries(policies)
        .filter((e): e is [string, PolicyGet] => e[1] != null)
        .map(([name, p]) => policyToRow(name, p));
      nextRows.sort((a, b) => a.name.localeCompare(b.name));
      return nextRows;
    },
    // The guarded `runApply` below blocks the apply on validation failure,
    // so this only runs for a valid set of rows; the `{}` fallback is
    // unreachable in practice but keeps the type non-null.
    toPayload: (v) => {
      const res = buildBucketPayload(v, baselineNamesRef.current);
      return res.ok ? res.body : { buckets: {} };
    },
  });

  // Config (backends + default backend + global compression) from the cache.
  const { data: cfg } = useAdminConfig();
  // Backends + real bucket names from the SHARED query cache — not a per-mount
  // Promise.all. `useBucketNames` derives names from the cached origins map, so
  // this panel no longer re-fetches /buckets that other consumers already hold.
  const fallbackBackends = useBackends().data ?? [];
  const realBuckets = useBucketNames();
  const [createOpen, setCreateOpen] = useState(false);
  /** Expanded row: `real:<name>` for buckets, the row `_id` for drafts. */
  const [expandedKey, setExpandedKey] = useState<string | null>(null);
  /** Manual "[Later]" re-encrypt action target. */
  const [reencryptBucket, setReencryptBucket] = useState<string | null>(null);
  // Active jobs drive the busy chip + progress bar; the query self-polls
  // every 2s while any job is active and goes quiet otherwise.
  const maintenanceJobs = useJobs().data?.jobs ?? [];

  useEffect(() => {
    if (cfg === null) onSessionExpired?.();
  }, [cfg, onSessionExpired]);

  // Prefer the /api/admin/config `backends` array — it synthesises a
  // "default" entry on the singleton path so encryption badges resolve
  // uniformly. Fall back to /api/admin/backends for legacy shapes.
  const backends =
    cfg?.backends && cfg.backends.length > 0 ? cfg.backends : fallbackBackends;
  const defaultBackend = cfg?.default_backend ?? null;
  const globalRatio = cfg?.max_delta_ratio ?? 0.75;
  const globalCompressionOn = globalRatio > 0;

  // Guarded apply: client-side validation first, then the editor's
  // validate → ApplyDialog → PUT flow (which re-derives the same body via
  // `toPayload`, keeping the validate/PUT body byte-identical).
  const runApply = useCallback(async () => {
    const res = buildBucketPayload(rows, baselineNamesRef.current);
    if (!res.ok) {
      message.error(res.error);
      return;
    }
    await editorRunApply();
  }, [rows, editorRunApply]);

  useApplyHandler('storage/buckets', runApply, dirty);

  // ── Row mutation: name-keyed for real buckets (materialises the row on
  //    first edit), id-keyed for drafts. ──
  //
  // `pruneNoops` drops rows that are back to ALL-DEFAULT for a REAL bucket
  // with no server-side policy: such a row serialises to nothing, so keeping
  // it would only flip the dirty indicator for a guaranteed no-op apply
  // ("Unsaved changes" that apply to nothing erodes trust in the dot).
  // Drafts (un-named / not-real) and baseline buckets (reset-to-defaults =
  // a real deletion) are kept.
  const pruneNoops = (cur: BucketPolicyRow[]): BucketPolicyRow[] =>
    cur.filter(
      (r) =>
        !isAllDefaultRow(r) ||
        baselineNamesRef.current.includes(r.name) ||
        !r.name ||
        !realBuckets.includes(r.name)
    );
  const patchBucket = (name: string, patch: Partial<BucketPolicyRow>) => {
    setRows((cur) => {
      const existing = cur.find((r) => r.name === name);
      const next = existing
        ? cur.map((r) => (r.name === name ? { ...r, ...patch } : r))
        : [...cur, { _id: freshId(), name, ...DEFAULT_ROW_FIELDS, ...patch }];
      return pruneNoops(next);
    });
  };
  const prefixChangeFor = (name: string) => (fn: (prev: PrefixEntry[]) => PrefixEntry[]) => {
    setRows((cur) => {
      const existing = cur.find((r) => r.name === name);
      const next = existing
        ? cur.map((r) =>
            r.name === name ? { ...r, public_prefixes: fn(r.public_prefixes) } : r
          )
        : [...cur, { _id: freshId(), name, ...DEFAULT_ROW_FIELDS, public_prefixes: fn([]) }];
      return pruneNoops(next);
    });
  };
  const patchDraft = (id: string, patch: Partial<BucketPolicyRow>) => {
    setRows((cur) => cur.map((r) => (r._id === id ? { ...r, ...patch } : r)));
  };
  // Draft renaming, guarded: naming a draft after an EXISTING bucket folds
  // into that bucket's row (expand it, drop the draft) instead of creating a
  // shadow duplicate; naming it after another draft/policy row is rejected
  // (the duplicate would only surface as a cryptic validation error later).
  const handleDraftRename = (draft: BucketPolicyRow, name: string) => {
    if (name && realBuckets.includes(name)) {
      message.info(`"${name}" already exists — edit it in the list above.`);
      if (isAllDefaultRow(draft)) {
        setRows((cur) => cur.filter((r) => r._id !== draft._id));
        setExpandedKey(`real:${name}`);
      }
      return;
    }
    if (name && rows.some((r) => r._id !== draft._id && r.name === name)) {
      message.warning(`"${name}" already has a settings row.`);
      return;
    }
    patchDraft(draft._id, { name });
  };
  const prefixChangeForDraft = (id: string) => (fn: (prev: PrefixEntry[]) => PrefixEntry[]) => {
    setRows((cur) =>
      cur.map((r) => (r._id === id ? { ...r, public_prefixes: fn(r.public_prefixes) } : r))
    );
  };

  const addDraft = () => {
    const id = freshId();
    setRows((cur) => [...cur, { _id: id, name: '', ...DEFAULT_ROW_FIELDS }]);
    setExpandedKey(id);
  };

  // ── Display list: every real bucket (with its row when one exists), then
  //    policy rows / drafts whose bucket doesn't exist. ──
  const rowByName = new Map(rows.filter((r) => r.name).map((r) => [r.name, r]));
  const sortedReal = [...realBuckets].sort((a, b) => a.localeCompare(b));
  const orphanRows = rows.filter((r) => !r.name || !realBuckets.includes(r.name));
  const overrideCount = rows.filter((r) => r.name && realBuckets.includes(r.name)).length;

  if (error) {
    return <Alert type="error" showIcon message="Failed to load" description={error} />;
  }

  return (
    <div
      style={{
        maxWidth: 820,
        margin: '0 auto',
        padding: 'clamp(16px, 3vw, 24px)',
        display: 'flex',
        flexDirection: 'column',
        gap: 16,
      }}
    >
      {dirty && (
        <Alert
          type="warning"
          showIcon
          message="Unsaved changes"
          description="Review the diff before applying — nothing is live yet."
          action={
            <Space>
              <Button size="small" onClick={discard} disabled={applying}>
                Discard
              </Button>
              <Button type="primary" size="small" onClick={runApply} loading={applying}>
                Review &amp; apply
              </Button>
            </Space>
          }
        />
      )}

      <div style={cardStyle}>
        {/* The page TabHeader already carries the "Buckets" title + description,
            so this is just the dynamic count line (no duplicate heading). */}
        <Text type="secondary" style={{ fontSize: 13 }}>
          {loading
            ? 'Loading…'
            : `${sortedReal.length} bucket${sortedReal.length === 1 ? '' : 's'}` +
              (overrideCount > 0
                ? ` · ${overrideCount} with custom settings`
                : ' — all on defaults') +
              '. Click a bucket to edit its settings.'}
        </Text>

        <div style={{ marginTop: 16, display: 'flex', flexDirection: 'column', gap: 8 }}>
          {sortedReal.map((name) => {
            const row = rowByName.get(name) ?? null;
            const key = `real:${name}`;
            return (
              <BucketCard
                key={key}
                name={name}
                row={row}
                real
                expanded={expandedKey === key}
                onToggle={() => setExpandedKey((k) => (k === key ? null : key))}
                backends={backends}
                defaultBackend={defaultBackend}
                globalCompressionOn={globalCompressionOn}
                globalRatio={globalRatio}
                onPatch={(patch) => patchBucket(name, patch)}
                onPrefixesChange={prefixChangeFor(name)}
                inputRadius={inputRadius}
                maintenanceJob={busyJobForBucket(maintenanceJobs, name)}
                onReencrypt={() => setReencryptBucket(name)}
              />
            );
          })}

          {orphanRows.map((row) => (
            <BucketCard
              key={row._id}
              name={row.name}
              row={row}
              real={false}
              expanded={expandedKey === row._id}
              onToggle={() => setExpandedKey((k) => (k === row._id ? null : row._id))}
              backends={backends}
              defaultBackend={defaultBackend}
              globalCompressionOn={globalCompressionOn}
              globalRatio={globalRatio}
              onPatch={(patch) => patchDraft(row._id, patch)}
              onPrefixesChange={prefixChangeForDraft(row._id)}
              onDraftNameChange={(name) => handleDraftRename(row, name)}
              onRemoveDraft={() =>
                setRows((cur) => cur.filter((r) => r._id !== row._id))
              }
              availableBuckets={sortedReal.filter((b) => !rowByName.has(b))}
              inputRadius={inputRadius}
            />
          ))}

          <div style={{ display: 'flex', alignItems: 'center', gap: 12, rowGap: 6, marginTop: 4, flexWrap: 'wrap' }}>
            <Button
              data-testid="buckets-create"
              aria-label="Create bucket"
              icon={<PlusOutlined />}
              onClick={() => setCreateOpen(true)}
              style={{ borderRadius: 8, fontFamily: 'var(--font-ui)', flexShrink: 0 }}
              type="dashed"
            >
              Create bucket
            </Button>
            <Button
              type="link"
              size="small"
              style={{ fontSize: 11, padding: 0, whiteSpace: 'normal', textAlign: 'left', height: 'auto' }}
              onClick={addDraft}
            >
              Pre-provision settings for a bucket that doesn&rsquo;t exist yet
            </Button>
          </div>
        </div>

        {dirty && (
          <Button
            type="primary"
            onClick={runApply}
            loading={applying}
            style={{ marginTop: 16, borderRadius: 8, fontWeight: 600 }}
            block
          >
            Review &amp; apply changes
          </Button>
        )}
      </div>

      <ReencryptProposalModal
        open={reencryptBucket !== null}
        transition="encrypt"
        backendName={
          reencryptBucket
            ? (rowByName.get(reencryptBucket)?.backend || defaultBackend || 'default')
            : ''
        }
        buckets={reencryptBucket ? [reencryptBucket] : []}
        onClose={() => setReencryptBucket(null)}
      />
      <CreateBucketModal
        open={createOpen}
        canAdmin
        onClose={() => setCreateOpen(false)}
        onCreated={(name) => {
          void loadSideData();
          setExpandedKey(`real:${name}`);
        }}
      />

      <ApplyDialog
        open={applyOpen}
        section="storage"
        response={applyResponse}
        onApply={confirmApply}
        onCancel={cancelApply}
        loading={applying}
      />
    </div>
  );
}
