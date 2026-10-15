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
import { useCallback, useMemo, useRef, useState } from 'react';
import { Alert, Button, Space, Typography, message } from 'antd';
import Dropdown from './KeyboardDropdown';
import { DownOutlined, PlusOutlined } from '@ant-design/icons';
import { LoadingState } from './StatePlaceholders';
import type { AdminConfig } from '../adminApi';
import { useQueryClient } from '@tanstack/react-query';
import { useAdminConfig } from '../queries/config';
import { useEncryptionWitness } from '../queries/encryptionWitness';
import { bucketMismatch } from '../encryptionWitness';
import { useBackends, useBucketNames, useBucketOrigins } from '../queries/backends';
import { qk } from '../queries/keys';
import { useCardStyles, contentColumn, CONTENT_FORM } from './shared-styles';
import ApplyDialog from './ApplyDialog';
import StickyDirtyBar from './StickyDirtyBar';
import BucketCard from './BucketCard';
import { ObjectSizeLimitCard } from './advancedPanels';
import CreateBucketModal from './CreateBucketModal';
import ReencryptProposalModal from './ReencryptProposalModal';
import { useApplyHandler } from '../useDirtySection';
import { useSectionEditor } from '../useSectionEditor';
import { useJobs } from '../queries/jobs';
import { busyJobForBucket } from '../jobsView';
import type { BucketPolicyRow, BucketPolicyPatch, PrefixEntry } from './bucketPolicyPayload';
import {
  DEFAULT_ROW_FIELDS,
  buildBucketPayload,
  freshId,
  isAllDefaultRow,
  hasOwnEncryption,
  ownEncryptionMode,
  policyMustSurvive,
  policyToRow,
} from './bucketPolicyPayload';
import { getSectionVersioned } from '../adminApi';
import { applySection, sectionApplyErrorText } from '../applySection';
import { bucketsWhoseEncryptionChanged, dominantTransition, type ReencryptTransition } from '../bucketEncryption';
import type { EffectiveEncryption } from '../adminApi';
import { numericCompare } from '../utils';

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
  // Baseline buckets whose policy must survive every apply (the bucket's
  // own `encryption` block, or fields this panel does not model).
  const unmodeledNamesRef = useRef<string[]>([]);

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
    rebase,
  } = useSectionEditor<BucketWire, BucketPolicyRow[]>({
    section: 'storage',
    dirtyKey: 'storage/buckets',
    initial: [],
    onSessionExpired,
    noun: 'bucket policies',
    pick: (body) => {
      const policies = body.buckets || {};
      baselineNamesRef.current = Object.keys(policies);
      unmodeledNamesRef.current = Object.entries(policies)
        .filter(([, p]) => p != null && policyMustSurvive(p))
        .map(([name]) => name);
      const nextRows = Object.entries(policies)
        .filter((e): e is [string, PolicyGet] => e[1] != null)
        .map(([name, p]) => policyToRow(name, p));
      nextRows.sort((a, b) => numericCompare(a.name, b.name));
      return nextRows;
    },
    // The guarded `runApply` below blocks the apply on validation failure,
    // so this only runs for a valid set of rows; the `{}` fallback is
    // unreachable in practice but keeps the type non-null.
    toPayload: (v) => {
      const res = buildBucketPayload(v, baselineNamesRef.current, unmodeledNamesRef.current);
      return res.ok ? res.body : { buckets: {} };
    },
  });

  // Config (backends + default backend + global compression) from the cache.
  const { data: cfg } = useAdminConfig({ onSessionExpired });
  // Several instances: the buckets that this instance refuses to write.
  const witnessMismatches = useEncryptionWitness().data?.mismatches ?? [];
  const queryClient = useQueryClient();
  // Backends + real bucket names from the SHARED query cache — not a per-mount
  // Promise.all. `useBucketNames` derives names from the cached origins map, so
  // this panel no longer re-fetches /buckets that other consumers already hold.
  const fallbackBackends = useBackends().data?.backends ?? [];
  const realBuckets = useBucketNames();
  // The backend that holds each bucket now (the server resolves routes,
  // aliases and buckets that live only on a non-default backend), and the
  // server's answer to how each bucket is encrypted.
  const originsQuery = useBucketOrigins();
  const originsData = originsQuery.data;
  const originBackendByName = useMemo(
    () => new Map((originsData?.buckets ?? []).map((o) => [o.name, o.backend_name ?? null] as const)),
    [originsData]
  );
  const effectiveByName = useMemo(
    () => new Map((originsData?.buckets ?? []).map((o) => [o.name, o.effective_encryption ?? null] as const)),
    [originsData]
  );
  const [createOpen, setCreateOpen] = useState(false);
  /** Expanded row: `real:<name>` for buckets, the row `_id` for drafts. */
  const [expandedKey, setExpandedKey] = useState<string | null>(null);
  /** Re-encrypt proposal: the manual "Re-encrypt existing objects" action
   *  of a card, or the offer after an encryption change of a bucket. */
  const [reencrypt, setReencrypt] = useState<{ buckets: string[]; transition: ReencryptTransition } | null>(null);
  // Active jobs drive the busy chip + progress bar; the query self-polls
  // every 2s while any job is active and goes quiet otherwise.
  const maintenanceJobs = useJobs().data?.jobs ?? [];

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
    const res = buildBucketPayload(rows, baselineNamesRef.current, unmodeledNamesRef.current);
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

  // An encryption change of one bucket applied at once (its own section
  // PUT): re-read the section and the origins, then offer the re-encrypt
  // job when the server's answer for the bucket changed.
  const onEncryptionApplied = async (bucket: string, before: EffectiveEncryption) => {
    // Rebase, not reset: staged edits (if any appeared meanwhile) survive.
    await rebase();
    const res = await originsQuery.refetch();
    const after = res.data?.buckets.find((o) => o.name === bucket)?.effective_encryption ?? null;
    const changed = bucketsWhoseEncryptionChanged(
      [{ name: bucket, effective_encryption: before }],
      [{ name: bucket, effective_encryption: after }]
    );
    if (changed.length > 0) setReencrypt({ buckets: [bucket], transition: dominantTransition(changed) });
  };
  /** The manual action: the words follow what the job would do now. */
  const openReencrypt = (bucket: string) => {
    const eff = effectiveByName.get(bucket);
    const transition: ReencryptTransition = !eff?.encrypts ? 'decrypt' : eff.legacy_key_id ? 'rotate' : 'encrypt';
    setReencrypt({ buckets: [bucket], transition });
  };

  // Settings with the bucket's own encryption for a bucket that does not
  // exist: removed at once (the staged path would keep the block, see
  // `policyMustSurvive`). No bucket, so no object uses the key, and the
  // server lets it go without a key-removal confirmation. Removing a block
  // that encrypts is a confirmed downgrade (the row's confirm dialog).
  const removeSettingsWithKey = async (row: BucketPolicyRow) => {
    const mode = ownEncryptionMode(row);
    try {
      const { version } = await getSectionVersioned('storage');
      const resp = await applySection(
        queryClient,
        'storage',
        { buckets: { [row.name]: null } },
        version,
        mode && mode !== 'none' ? { downgrades: [row.name] } : undefined
      );
      if (!resp.ok) {
        message.error(resp.error || 'The proxy refused to remove the settings');
        return;
      }
      message.success(`Settings of ${row.name} removed`);
      await rebase();
    } catch (e) {
      message.error(sectionApplyErrorText(e, 'Failed to remove the settings'));
    }
  };

  const addDraft = () => {
    const id = freshId();
    setRows((cur) => [...cur, { _id: id, name: '', ...DEFAULT_ROW_FIELDS }]);
    setExpandedKey(id);
  };

  // ── Display list: every real bucket (with its row when one exists), then
  //    policy rows / drafts whose bucket doesn't exist. ──
  const rowByName = new Map(rows.filter((r) => r.name).map((r) => [r.name, r]));
  const sortedReal = [...realBuckets].sort((a, b) => numericCompare(a, b));
  const orphanRows = rows.filter((r) => !r.name || !realBuckets.includes(r.name));
  const overrideCount = rows.filter((r) => r.name && realBuckets.includes(r.name)).length;

  if (error) {
    return <Alert type="error" showIcon title="Failed to load" description={error} />;
  }

  return (
    <div
      style={{
        ...contentColumn(CONTENT_FORM),
        display: 'flex',
        flexDirection: 'column',
        gap: 16,
      }}
    >
      <div style={cardStyle}>
        {/* The page TabHeader carries the "Buckets" title, so the header row
            is the count line and ONE call to action. The action sits here,
            not after the list: below an expanded card it read as that card's
            save button. Pre-provisioning (settings for a bucket that does not
            exist yet) is its secondary option. */}
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12 }}>
          <Text type="secondary" style={{ fontSize: 13 }}>
            {loading
              ? 'Loading buckets…'
              : `${sortedReal.length} bucket${sortedReal.length === 1 ? '' : 's'}` +
                (overrideCount > 0
                  ? ` · ${overrideCount} with custom settings`
                  : ' — all on defaults') +
                '. Click a bucket to edit its settings.'}
          </Text>
          <Space.Compact>
            <Button
              data-testid="buckets-create"
              aria-label="Create bucket"
              onClick={() => setCreateOpen(true)}
              icon={<PlusOutlined />}
            >
              Create bucket
            </Button>
            <Dropdown
              menu={{
                items: [
                  { key: 'draft', label: 'Add settings for a bucket that does not exist yet' },
                ],
                onClick: () => addDraft(),
              }}
            >
              <Button icon={<DownOutlined />} aria-label="More ways to add a bucket" />
            </Dropdown>
          </Space.Compact>
        </div>

        <div style={{ marginTop: 16, display: 'flex', flexDirection: 'column', gap: 8 }}>
          {loading && sortedReal.length === 0 && <LoadingState />}
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
                originBackend={originBackendByName.get(name) ?? null}
                effectiveEncryption={effectiveByName.get(name) ?? null}
                encryptionBlockedReason={
                  dirty ? 'Apply or discard the unsaved bucket changes first: an encryption change applies at once.' : null
                }
                onEncryptionApplied={(before) => void onEncryptionApplied(name, before)}
                globalCompressionOn={globalCompressionOn}
                globalRatio={globalRatio}
                onPatch={(patch) => patchBucket(name, patch)}
                onPrefixesChange={prefixChangeFor(name)}
                inputRadius={inputRadius}
                maintenanceJob={busyJobForBucket(maintenanceJobs, name)}
                onReencrypt={
                  effectiveByName.get(name)?.reencrypt ? () => openReencrypt(name) : undefined
                }
                witnessRefused={
                  !!cfg &&
                  bucketMismatch(
                    witnessMismatches,
                    cfg,
                    name,
                    effectiveByName.get(name)?.backend ?? originBackendByName.get(name),
                  ) !== null
                }
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
              // A policy with its own key keeps its name: a rename would
              // move the key to another name (the server refuses it).
              onDraftNameChange={hasOwnEncryption(row) ? undefined : (name) => handleDraftRename(row, name)}
              onRemoveDraft={
                hasOwnEncryption(row)
                  ? () => void removeSettingsWithKey(row)
                  : () => setRows((cur) => cur.filter((r) => r._id !== row._id))
              }
              availableBuckets={sortedReal.filter((b) => !rowByName.has(b))}
              inputRadius={inputRadius}
            />
          ))}

        </div>
      </div>

      <ObjectSizeLimitCard onSessionExpired={onSessionExpired} />

      <StickyDirtyBar
        visible={dirty}
        applying={applying}
        onDiscard={discard}
        onApply={() => void runApply()}
        floating
      />

      <ReencryptProposalModal
        open={reencrypt !== null}
        transition={reencrypt?.transition ?? 'encrypt'}
        buckets={reencrypt?.buckets ?? []}
        onClose={() => setReencrypt(null)}
      />
      <CreateBucketModal
        open={createOpen}
        canAdmin
        onClose={() => setCreateOpen(false)}
        onCreated={(name) => {
          // Refresh the shared origins cache so the new bucket appears in the
          // list (and everywhere else that reads useBucketOrigins).
          void queryClient.invalidateQueries({ queryKey: qk.backends.origins() });
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
