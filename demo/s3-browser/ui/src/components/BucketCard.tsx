/**
 * BucketCard — one bucket's status row for {@link BucketsPanel}.
 *
 * Redesigned per the cognitive-load study (docs/plan/storage-ui-cognitive-load.md):
 *
 *   * COLLAPSED (the default): a single status line — bucket name + chips
 *     showing the EFFECTIVE state (backend incl. the default, encryption,
 *     public access, quota, compression override). A read task costs zero
 *     form controls.
 *   * EXPANDED (click): four groups — Public access (the dangerous one,
 *     amber), Encryption (the server's `effective_encryption` and the
 *     bucket's own block, BucketEncryptionGroup), Placement (backend +
 *     migrate), and Advanced (compression / delta cutoff / real-name alias /
 *     quota behind a disclosure).
 *
 * Every inherited value renders RESOLVED with provenance ("Default —
 * hetzner", "0.75 — global default") so the user never computes the
 * inheritance chain in their head.
 *
 * The row may represent a REAL bucket without any policy (all chips show
 * inherited state; editing creates the policy via the parent's onPatch) or
 * a policy row whose bucket doesn't exist ("not found" chip; name editable
 * for pre-provisioning drafts).
 */
import { bucketNameError } from '../bucketName';
import { Children, isValidElement, useEffect, useState, useId } from 'react';
import type { ReactNode } from 'react';
import { useIsNarrow } from '../useIsNarrow';
import { CAPABILITY_DOC_URL, docsUrlToInAppHref } from '../linkifyDocUrl';
import { Button, Collapse, Input, Modal, Progress, Radio, Select, Typography } from 'antd';
import { DownOutlined, RightOutlined, SyncOutlined } from '@ant-design/icons';
import type { BackendInfo, EffectiveEncryption } from '../adminApi';
import { describeBucketEncryption } from '../bucketEncryption';
import BucketEncryptionGroup from './BucketEncryptionGroup';
import { useColors } from '../ThemeContext';
import SimpleAutoComplete from './SimpleAutoComplete';
import { formRow } from './ruleEditorHelpers';
import type { BucketPolicyRow, PrefixEntry } from './bucketPolicyPayload';
import { DEFAULT_ROW_FIELDS, freshId, hasOwnEncryption, isAllDefaultRow, normalizeExtension, parseDeltaRatio, parseQuotaGib, resetRowFields } from './bucketPolicyPayload';
import PrefixListEditor from './PrefixListEditor';
import MigrateBucketModal from './MigrateBucketModal';
import type { JobRow } from '../jobsView';
import { progressLabel } from '../jobsView';
import { runJobAction } from '../adminApi';
import { useBucketMaintenance } from '../queries/maintenance';
import { bucketWritesBlocked } from '../maintenanceStatus';
import { cancelConfirmText } from '../jobsView';
import { formatBytes } from '../utils';
import { gibFromBytes } from '../savings';
import { activateOnKey } from '../keyboard';
import { confirmDialog } from '../confirmDialog';
import { useNavigation } from '../NavigationContext';
import { buildViewUrl } from '../urlState';
import type { BucketRule } from '../runsView';
import { runsUrl } from './jobs/runsLink';
import { WITNESS_CARD_ID } from './EncryptionWitnessCard';
import TruncatedName from './TruncatedName';

const { Text } = Typography;

/** A radio beside a two-line (or taller) option: the dot sits on the
 *  title line, not centred on the whole block (it read as the radio of
 *  the prefix input below the title). */
const RADIO_TOP = { icon: { alignSelf: 'flex-start', marginTop: 4 } } as const;

interface CardProps {
  /** The replication and lifecycle rules that read or write this bucket. */
  rules?: BucketRule[];
  /** Bucket name (the display identity of the row). */
  name: string;
  /** The policy row, or null when this real bucket has no overrides. */
  row: BucketPolicyRow | null;
  /** Whether the bucket actually exists on storage (vs a policy draft). */
  real: boolean;
  expanded: boolean;
  onToggle: () => void;
  backends: BackendInfo[];
  defaultBackend: string | null;
  /** The backend that holds the bucket now (origins `backend_name`). It can
   *  differ from the default without an explicit route: a bucket that lives
   *  only on another backend. */
  originBackend?: string | null;
  /** The server's answer to how the bucket's new writes are encrypted
   *  (GET /api/admin/buckets `effective_encryption`). Undefined for a
   *  policy whose bucket does not exist. */
  effectiveEncryption?: EffectiveEncryption | null;
  /** Why the Encryption group cannot apply now (unsaved staged edits). */
  encryptionBlockedReason?: string | null;
  /** After the Encryption group applied a change. */
  onEncryptionApplied?: (before: EffectiveEncryption) => void;
  /** Global compression default — for resolved labels. */
  globalCompressionOn: boolean;
  globalRatio: number;
  /** The built-in delta-eligible file types (`delta_extensions_default`). */
  defaultDeltaExtensions?: readonly string[];
  /** Stage a change. Parent materialises a policy row on first edit. */
  onPatch: (patch: Partial<BucketPolicyRow>) => void;
  /** Functional prefix-list transform routed through the parent's setRows. */
  onPrefixesChange: (fn: (prev: PrefixEntry[]) => PrefixEntry[]) => void;
  /** Drafts only: rename / remove the draft row. */
  onDraftNameChange?: (name: string) => void;
  onRemoveDraft?: () => void;
  /** Draft-name autocomplete options. */
  availableBuckets?: string[];
  inputRadius: { borderRadius: number };
  /** Active maintenance (re-encryption) job for this bucket, if any. */
  maintenanceJob?: JobRow | null;
  /** Start the one-off re-encrypt job for this bucket (the [Later] path). */
  onReencrypt?: () => void;
  /** The encryption witness (several instances) refuses writes to this
   *  bucket on this instance: its encryption differs from the cluster's. */
  witnessRefused?: boolean;
  /** A field of the card has a value that cannot be staged (the parent
   *  counts it, and the dirty bar refuses to apply). Null = fixed. */
  onFieldError?: (error: string | null) => void;
}

/** Small status chip used in the collapsed row. Sentence case, never an
 *  uppercase transform: it shouted units ("GIB") and changed the case of
 *  bucket and backend names, which are case-sensitive. */
function Chip({ tone, children, title }: { tone: string; children: React.ReactNode; title?: string }) {
  return (
    <span
      title={title}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 4,
        fontSize: 11,
        fontWeight: 600,
        padding: '2px 8px',
        borderRadius: 10,
        // A light tint (8%): the tone text keeps 4.5:1 on it in both
        // themes, also on the amber wash of a public bucket's card.
        background: `${tone}14`,
        color: tone,
        whiteSpace: 'nowrap',
        cursor: title ? 'help' : undefined,
      }}
    >
      <span style={{ width: 6, height: 6, borderRadius: '50%', background: tone }} />
      {children}
    </span>
  );
}

/** The visible text of a chip (its string children, icons dropped). */
function chipText(node: ReactNode): string {
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (!isValidElement<{ children?: ReactNode }>(node)) return '';
  return Children.toArray(node.props.children).map(chipText).join('').trim();
}

/** On a phone a row shows this many chips, then a "+N" chip. */
const PHONE_CHIPS = 2;

/** "File types: 2 added, 1 removed" — the per-bucket change to the
 *  built-in compressible types, in words (it was "+2 −1"). */
function fileTypesLabel(added: number, removed: number): string {
  const parts = [added ? `${added} added` : '', removed ? `${removed} removed` : ''].filter(Boolean);
  return `File types: ${parts.join(', ')}`;
}

/** What a rule does with the bucket (the button's title). */
const BUCKET_RULE_ROLE: Record<BucketRule['role'], string> = {
  source: 'The rule copies objects from this bucket.',
  destination: 'The rule copies objects into this bucket.',
  bucket: 'The rule expires or moves objects of this bucket.',
  archive: 'The rule moves objects into this bucket.',
};

export default function BucketCard({
  name,
  row,
  real,
  expanded,
  onToggle,
  backends,
  defaultBackend,
  originBackend = null,
  effectiveEncryption,
  encryptionBlockedReason = null,
  onEncryptionApplied,
  globalCompressionOn,
  globalRatio,
  defaultDeltaExtensions = [],
  onPatch,
  onPrefixesChange,
  onDraftNameChange,
  onRemoveDraft,
  availableBuckets = [],
  inputRadius,
  maintenanceJob = null,
  onReencrypt,
  witnessRefused = false,
  rules = [],
  onFieldError,
}: CardProps) {
  const uid = useId();
  const colors = useColors();
  // Phone: all the chips took three or four lines under the name.
  const phone = useIsNarrow(600);
  const { navigate } = useNavigation();
  const witnessUrl = buildViewUrl('admin', 'system', { focus: WITNESS_CARD_ID });
  const [migrateOpen, setMigrateOpen] = useState(false);
  // Whether the job blocks writes is the server's answer (its write gate),
  // read only while a job runs on this bucket.
  const gate = useBucketMaintenance(maintenanceJob ? name : null).data;
  // Until the first answer, keep the job's busy look.
  const writesBlocked = gate === undefined ? true : bucketWritesBlocked(gate);

  // Effective field values: the policy row when present, defaults otherwise.
  // Fresh fallback object (not the frozen singleton) so the shared
  // `public_prefixes: []` reference can never leak into a materialised row.
  const eff: Omit<BucketPolicyRow, '_id' | 'name'> = row ?? { ...DEFAULT_ROW_FIELDS, public_prefixes: [] };
  const isPublic = eff.publicMode !== 'none';
  const publicPrefixCount = eff.public_prefixes.filter((p) => p.value.trim()).length;
  const hasOverrides = row !== null && !isAllDefaultRow(row);

  // The Quota field keeps what the user typed while it is not a valid quota
  // (the staged value stays the last valid one, and the error says why);
  // a valid text is staged at once and the field shows the staged value.
  const [quotaDraft, setQuotaDraft] = useState<string | null>(null);
  const quotaText = quotaDraft ?? (eff.quota_bytes != null ? String(gibFromBytes(eff.quota_bytes)) : '');
  const quotaParsed = quotaDraft === null ? null : parseQuotaGib(quotaDraft);
  const quotaError = quotaParsed && !quotaParsed.ok ? quotaParsed.error : null;
  const onQuotaText = (text: string) => {
    const parsed = parseQuotaGib(text);
    setQuotaDraft(text);
    if (parsed.ok && parsed.bytes !== eff.quota_bytes) onPatch({ quota_bytes: parsed.bytes });
  };
  // The delta size cutoff: the same rule (keep the text, say what is wrong,
  // stage only a valid value; never a silent clamp, UX harvest D9).
  const [ratioDraft, setRatioDraft] = useState<string | null>(null);
  const ratioText = ratioDraft ?? (eff.max_delta_ratio != null ? String(eff.max_delta_ratio) : '');
  const ratioParsed = ratioDraft === null ? null : parseDeltaRatio(ratioDraft);
  const ratioError = ratioParsed && !ratioParsed.ok ? ratioParsed.error : null;
  const onRatioText = (text: string) => {
    const parsed = parseDeltaRatio(text);
    setRatioDraft(text);
    if (parsed.ok && parsed.ratio !== eff.max_delta_ratio) onPatch({ max_delta_ratio: parsed.ratio });
  };
  const stagedRatio = eff.max_delta_ratio;
  useEffect(() => {
    setRatioDraft((cur) => {
      if (cur === null) return cur;
      const parsed = parseDeltaRatio(cur);
      return parsed.ok && parsed.ratio !== stagedRatio ? null : cur;
    });
  }, [stagedRatio]);
  const fieldError = quotaError ?? ratioError;

  // A removed card takes its error with it.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => () => onFieldError?.(null), []);
  useEffect(() => {
    onFieldError?.(fieldError);
    // Report changes of the error only; the callback is a fresh closure.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fieldError]);
  // A staged value that changes under a valid draft (Discard, Reset to
  // defaults, a rebase) wins over the text.
  const stagedQuota = eff.quota_bytes;
  useEffect(() => {
    setQuotaDraft((cur) => {
      if (cur === null) return cur;
      const parsed = parseQuotaGib(cur);
      return parsed.ok && parsed.bytes !== stagedQuota ? null : cur;
    });
  }, [stagedQuota]);

  const cardBorder = isPublic ? `${colors.ACCENT_AMBER}66` : colors.BORDER;
  const cardBg = isPublic ? `${colors.ACCENT_AMBER}0a` : colors.BG_ELEVATED;

  // Re-routing an ALREADY-routed bucket orphans its objects (explicit route
  // wins; the old backend is never scanned) — confirm first. First-time set
  // is the create path and applies directly.
  const handleBackendChange = (next: string) => {
    const value = next ?? '';
    const prev = eff.backend;
    // The bucket's own encryption block belongs to its storage: it needs an
    // explicit backend (the server refuses a block without one).
    if (!value && hasOwnEncryption(row)) {
      Modal.info({
        title: `${name} keeps an explicit backend`,
        content:
          'This bucket has its own encryption, and that setting belongs to the backend that stores it. To follow the default backend, first set the Encryption group to Inherit.',
      });
      return;
    }
    if (!prev || value === prev) {
      onPatch({ backend: value });
      return;
    }
    const target = value || `the default (${defaultBackend ?? 'default'})`;
    Modal.confirm({
      title: `Re-route ${name || 'this bucket'} to ${target}?`,
      okText: 'Re-route anyway',
      okButtonProps: { danger: true },
      content: (
        <Text type="secondary">
          Routing only — this does <strong>not</strong> move existing objects.
          Objects already stored on <Text code>{prev}</Text> become unreachable
          through this bucket until they are migrated (use &ldquo;Migrate
          data&hellip;&rdquo; instead to move them).
        </Text>
      ),
      onOk: () => onPatch({ backend: value }),
    });
  };

  // ── Collapsed status chips (effective state, always resolved) ──
  // `Config::default_backend_name`: the singleton backend is "default".
  const defaultName = defaultBackend ?? 'default';
  // The server's answer, never a client-side resolution (a bucket's own
  // block overrides its backend).
  const encryptionInfo = describeBucketEncryption(effectiveEncryption);
  const fileTypesChanged = eff.delta_add.length > 0 || eff.delta_remove.length > 0;
  const advancedActive =
    eff.compression !== null || eff.max_delta_ratio !== null || fileTypesChanged || eff.alias !== '' || eff.quota_bytes !== null;

  const chips: React.ReactNode[] = [];
  if (maintenanceJob) {
    const pct = maintenanceJob.percent ?? null;
    chips.push(
      <Chip
        key="busy"
        tone={writesBlocked ? colors.ACCENT_AMBER : colors.TEXT_MUTED}
        title={
          writesBlocked
            ? maintenanceJob.kind === 'migrate'
              ? 'Migrating this bucket to another backend. Reads work; uploads and deletes are temporarily rejected.'
              : 'A job is rewriting this bucket. Reads work; uploads and deletes are temporarily rejected.'
            : 'A job is still finishing on this bucket. Reads, uploads and deletes work.'
        }
      >
        <SyncOutlined spin aria-hidden style={{ fontSize: 10 }} /> {writesBlocked ? 'Busy' : 'Job finishing'}
        {pct != null ? ` ${pct}%` : ''}
      </Chip>
    );
  }
  if (!real) {
    chips.push(
      <Chip key="missing" tone={colors.ACCENT_AMBER} title="No bucket with this name exists yet — the policy applies once it's created.">
        Bucket not found
      </Chip>
    );
  }
  chips.push(
    eff.backend ? (
      <Chip key="backend" tone={colors.ACCENT_BLUE} title={`Explicitly routed to ${eff.backend}`}>
        → {eff.backend}
      </Chip>
    ) : originBackend && originBackend !== defaultName ? (
      <Chip
        key="backend"
        tone={colors.ACCENT_BLUE}
        title={`No explicit route — the bucket exists on ${originBackend}${defaultBackend ? ` (the default backend is ${defaultBackend})` : ''}`}
      >
        On {originBackend}
      </Chip>
    ) : (
      // "Default backend", not "on default": that read as "on by default".
      <Chip key="backend" tone={colors.TEXT_MUTED} title={`No explicit route: the bucket uses the default backend (${defaultName}).`}>
        Default backend{defaultName !== 'default' ? ` · ${defaultName}` : ''}
      </Chip>
    )
  );
  // Plaintext inherited from a plaintext backend is the default state of
  // every bucket: a "Not encrypted" chip on every row said nothing (UX
  // harvest C42). The chip shows when the bucket is encrypted, when it
  // differs from its backend, or when the proxy does not know.
  const encryptionIsPlainDefault =
    !!effectiveEncryption &&
    !effectiveEncryption.encrypts &&
    effectiveEncryption.source === 'backend' &&
    effectiveEncryption.mode === 'none' &&
    !effectiveEncryption.legacy_key_id;
  if (real && !encryptionIsPlainDefault) {
    chips.push(
      <Chip
        key="enc"
        tone={encryptionInfo.isEncrypted ? colors.ACCENT_GREEN : colors.TEXT_MUTED}
        title={encryptionInfo.tooltip}
      >
        {encryptionInfo.label}
      </Chip>
    );
  }
  chips.push(
    eff.publicMode === 'none' ? (
      <Chip key="pub" tone={colors.TEXT_MUTED} title="Authenticated requests only">
        Private
      </Chip>
    ) : (
      <Chip
        key="pub"
        tone={colors.ACCENT_AMBER}
        title={
          eff.publicMode === 'entire'
            ? 'The whole bucket is readable without credentials'
            : 'Some prefixes are readable without credentials'
        }
      >
        {eff.publicMode === 'entire' ? '⚠ Public bucket' : `⚠ ${publicPrefixCount} public prefix${publicPrefixCount === 1 ? '' : 'es'}`}
      </Chip>
    )
  );
  if (quotaError) {
    // The field holds text that is not a quota: the chip says so, instead
    // of a size the user typed on the way (UX harvest D59).
    chips.push(
      <Chip key="quota" tone={colors.ACCENT_RED} title={quotaError}>
        Quota: fix the value
      </Chip>
    );
  } else if (eff.quota_bytes != null) {
    chips.push(
      // A quota is a normal setting, not a problem: neutral, not amber.
      <Chip key="quota" tone={colors.TEXT_MUTED} title="The bucket accepts uploads up to this total size">
        Quota {formatBytes(eff.quota_bytes)}
      </Chip>
    );
  }
  if (eff.replication_target_only) {
    chips.push(
      <Chip
        key="rto"
        tone={colors.ACCENT_BLUE}
        title="replication_target_only: client writes are rejected (403); replication is the only writer."
      >
        Replication target
      </Chip>
    );
  }
  if (eff.compression !== null || eff.max_delta_ratio !== null) {
    chips.push(
      <Chip key="comp" tone={colors.ACCENT_PURPLE} title="Compression override on this bucket">
        {eff.compression === false
          ? 'Compression off'
          : eff.max_delta_ratio !== null
            ? `Delta cutoff ${eff.max_delta_ratio}`
            : 'Compression on'}
      </Chip>
    );
  }

  if (fileTypesChanged && eff.compression !== false) {
    chips.push(
      <Chip
        key="types"
        tone={colors.ACCENT_PURPLE}
        title={[
          eff.delta_add.length ? `Also compressed: ${eff.delta_add.join(', ')}` : '',
          eff.delta_remove.length ? `Stored whole: ${eff.delta_remove.join(', ')}` : '',
        ].filter(Boolean).join('\n')}
      >
        {fileTypesLabel(eff.delta_add.length, eff.delta_remove.length)}
      </Chip>
    );
  }

  // On a phone: the first chips, then "+N" with the others in its title.
  const shownChips =
    phone && chips.length > PHONE_CHIPS + 1
      ? [
          ...chips.slice(0, PHONE_CHIPS),
          <Chip
            key="more"
            tone={colors.TEXT_MUTED}
            title={chips.slice(PHONE_CHIPS).map(chipText).join('\n')}
          >
            +{chips.length - PHONE_CHIPS}
          </Chip>,
        ]
      : chips;

  const cardActions: { key: string; label: string; confirm: { title: string; content: string; okText: string }; onSelect: () => void }[] = [
    ...(hasOverrides
      ? [{
          key: 'reset',
          label: 'Reset to defaults…',
          confirm: {
            title: `Reset ${name || 'this bucket'} to defaults?`,
            content: hasOwnEncryption(row)
              ? 'Public access, quota, and compression go back to the defaults. The bucket keeps its own encryption and its backend: a bucket key that objects use cannot be removed here. To change the encryption, use the Encryption group. Nothing changes until you review and apply.'
              : 'Public access, routing, quota, and compression go back to the defaults. Nothing changes until you review and apply.',
            okText: 'Reset',
          },
          onSelect: () => onPatch(resetRowFields(row)),
        }]
      : []),
    ...(onRemoveDraft
      ? [{
          key: 'remove',
          label: 'Remove these settings…',
          confirm: hasOwnEncryption(row)
            ? {
                title: `Remove the settings and the encryption key of ${name}?`,
                content:
                  'No bucket with this name exists, so no object uses its key. The settings and the key are removed from the configuration at once (not when you review and apply). If you create this bucket later and want this key again, keep a copy of the key.',
                okText: 'Remove settings and key',
              }
            : {
                title: 'Remove these settings?',
                content: 'The settings for this bucket name are removed when you review and apply.',
                okText: 'Remove',
              },
          onSelect: onRemoveDraft,
        }]
      : []),
  ];

  // Group titles in sentence case (UX harvest D67: one heading system).
  const groupLabel: React.CSSProperties = {
    fontSize: 13,
    fontWeight: 600,
    fontFamily: 'var(--font-ui)',
    display: 'block',
    marginBottom: 8,
  };

  return (
    <div
      data-bucket-card={name}
      style={{
        border: `1px solid ${cardBorder}`,
        borderRadius: 10,
        background: cardBg,
        transition: 'border-color 0.15s, background-color 0.15s',
      }}
    >
      {/* ── Collapsed status row: name + effective-state chips ── */}
      <div
        role="button"
        tabIndex={0}
        aria-expanded={expanded}
        // No aria-label: the name comes from the visible text (bucket name
        // + chips), so it always contains what the user sees (axe
        // label-content-name-mismatch); aria-expanded says edit/collapse.
        onClick={onToggle}
        onKeyDown={activateOnKey(onToggle)}
        style={{
          display: 'flex',
          alignItems: 'center',
          // Wrap on narrow so the chip group drops below the name instead of
          // overflowing (PRIVATE was clipped on mobile).
          flexWrap: 'wrap',
          gap: 12,
          rowGap: 8,
          padding: '12px 12px',
          cursor: 'pointer',
          minHeight: 42,
        }}
      >
        {expanded ? (
          <DownOutlined aria-hidden style={{ fontSize: 10, color: colors.TEXT_MUTED }} />
        ) : (
          <RightOutlined aria-hidden style={{ fontSize: 10, color: colors.TEXT_MUTED }} />
        )}
        {onDraftNameChange ? (
          <span onClick={(e) => e.stopPropagation()} style={{ flex: 1, minWidth: 160 }}>
            <SimpleAutoComplete
              aria-label="Bucket name"
              value={name}
              onChange={(v) => onDraftNameChange(v)}
              options={availableBuckets}
              placeholder="Bucket name"
              style={{ width: '100%' }}
            />
            {bucketNameError(name) && (
              <Text role="alert" type="danger" style={{ display: 'block', fontSize: 11, marginTop: 2 }}>
                {bucketNameError(name)}
              </Text>
            )}
          </span>
        ) : (
          // A long name is cut in the middle inside the card (it ran past
          // the card edge on a phone); the full name is in the title.
          <TruncatedName
            text={name}
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 13,
              fontWeight: 600,
              color: colors.TEXT_PRIMARY,
              flex: '0 1 auto',
              maxWidth: 'calc(100% - 24px)',
            }}
          />
        )}
        {/* Chips absorb remaining width and wrap internally; the group drops to
            the next line on a narrow row instead of overflowing. */}
        <span
          style={{
            flex: '1 1 auto',
            minWidth: 0,
            display: 'flex',
            gap: 8,
            flexWrap: 'wrap',
            justifyContent: 'flex-end',
          }}
        >
          {/* Spaces between chips: the row's accessible name reads
              "releases Default backend Private", not "releasesDefault…". */}
          {' '}
          {shownChips.flatMap((chip, i) => (i === 0 ? [chip] : [' ', chip]))}
        </span>
      </div>

      {/* ── Encryption witness: its own line, outside the clickable row ── */}
      {witnessRefused && (
        <div style={{ padding: '0 12px 12px' }}>
          <span
            data-testid="witness-refused-chip"
            title="Several instances share this cluster. This instance encrypts new writes to this bucket differently from the setting that the cluster published, so it refuses them. Reads keep working."
            style={{
              display: 'inline-flex',
              flexWrap: 'wrap',
              alignItems: 'center',
              gap: 8,
              fontSize: 11.5,
              fontWeight: 600,
              padding: '4px 12px',
              borderRadius: 10,
              background: `${colors.ACCENT_RED}1a`,
              color: colors.ACCENT_RED,
            }}
          >
            <span style={{ width: 6, height: 6, borderRadius: '50%', background: colors.ACCENT_RED }} />
            Writes refused on this instance: encryption differs from the cluster.
            <a
              href={witnessUrl}
              onClick={(e) => {
                e.preventDefault();
                navigate(witnessUrl);
              }}
              style={{ color: 'inherit', textDecoration: 'underline' }}
            >
              Open the encryption witness
            </a>
          </span>
        </div>
      )}

      {/* ── Maintenance progress: visible on every render of the row ── */}
      {maintenanceJob && (
        <div style={{ padding: '0 12px 12px', display: 'flex', alignItems: 'center', gap: 12 }}>
          <Progress
            aria-label="Maintenance job progress"
            percent={maintenanceJob.percent ?? 100}
            status="active"
            showInfo={maintenanceJob.percent != null}
            size="small"
            strokeColor={colors.ACCENT_AMBER}
            style={{ flex: 1, margin: 0 }}
          />
          <Text type="secondary" style={{ fontSize: 11, whiteSpace: 'nowrap' }}>
            {progressLabel(maintenanceJob)}
          </Text>
          {maintenanceJob.status !== 'cancelling' && (
            <Button
              size="small"
              type="text"
              danger
              style={{ fontSize: 11, padding: '0 4px' }}
              title={
                maintenanceJob.kind === 'migrate'
                  ? 'Stop the migration. The bucket stays on its current backend; nothing has switched over.'
                  : 'Stop the job. Already-rewritten objects stay rewritten (the job is idempotent — re-running later skips them).'
              }
              onClick={(e) => {
                e.stopPropagation();
                // A migration cancelled before the switch deletes the copies
                // it made: ask first, with the same words as the Jobs page.
                const text = cancelConfirmText(maintenanceJob);
                Modal.confirm({
                  title: text.title,
                  content: text.content,
                  okText: 'Cancel job',
                  cancelText: 'Keep running',
                  okButtonProps: { danger: true },
                  onOk: () =>
                    runJobAction(maintenanceJob.id, 'cancel').catch(() => {
                      /* next poll reflects the real state either way */
                    }),
                });
              }}
            >
              Cancel
            </Button>
          )}
        </div>
      )}

      {/* ── Expanded editor: Public access / Placement / Advanced ── */}
      {expanded && (
        <div style={{ padding: '0 12px 12px', borderTop: `1px solid ${colors.BORDER}` }}>
          {eff.replication_target_only && (
            <div
              style={{
                marginTop: 12,
                padding: '8px 12px',
                borderRadius: 6,
                border: `1px solid ${colors.ACCENT_BLUE}44`,
                background: `${colors.ACCENT_BLUE}0a`,
                fontSize: 12,
                color: colors.TEXT_MUTED,
              }}
            >
              This bucket is a <Text code>replication_target_only</Text> mirror: client
              writes (uploads, deletes, copies into it) are rejected with 403 so
              replication stays the single writer — which is what makes a non-CAS
              backend (e.g. Backblaze B2) safe to host it. Reads are unaffected. Set
              or clear the marker in YAML.{' '}
              <a
                href={docsUrlToInAppHref(CAPABILITY_DOC_URL) ?? CAPABILITY_DOC_URL}
                target="_blank"
                rel="noopener noreferrer"
              >
                Learn more
              </a>
            </div>
          )}
          {/* Public access — the consequential group, first and amber. */}
          <div style={{ paddingTop: 12 }}>
            <Text style={{ ...groupLabel, color: isPublic ? colors.ACCENT_AMBER : colors.TEXT_MUTED }}>
              Public access
              {isPublic && (
                <span style={{ fontSize: 12, marginLeft: 8, fontWeight: 500 }}>
                  ⚠ readable without credentials
                </span>
              )}
            </Text>
            <Radio.Group
              value={eff.publicMode}
              onChange={(e) => {
                const mode = e.target.value as BucketPolicyRow['publicMode'];
                onPatch({
                  publicMode: mode,
                  public_prefixes:
                    mode === 'prefixes'
                      ? eff.public_prefixes.length > 0
                        ? eff.public_prefixes
                        : [{ id: freshId(), value: '' }]
                      : [],
                });
              }}
              style={formRow(6, { flexDirection: 'column', alignItems: 'stretch' })}
            >
              <Radio value="none" style={{ alignItems: 'flex-start' }} styles={RADIO_TOP}>
                <div>
                  <span style={{ fontSize: 13 }}>Private (default)</span>
                  <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 1 }}>
                    Authenticated requests only.
                  </Text>
                </div>
              </Radio>
              <Radio value="entire" style={{ alignItems: 'flex-start' }} styles={RADIO_TOP}>
                <div>
                  <span style={{ fontSize: 13 }}>Entire bucket public</span>
                  <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 1 }}>
                    Anyone can read and list every object — no credentials needed.
                  </Text>
                </div>
              </Radio>
              <Radio value="prefixes" style={{ alignItems: 'flex-start' }} styles={RADIO_TOP}>
                <div style={{ width: '100%' }}>
                  <span style={{ fontSize: 13 }}>Specific prefixes public</span>
                  <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 1 }}>
                    Only keys under these prefixes are readable without credentials.
                    End folder prefixes with <code>/</code>.
                  </Text>
                  {eff.publicMode === 'prefixes' && (
                    <PrefixListEditor
                      prefixes={eff.public_prefixes}
                      onPrefixesChange={onPrefixesChange}
                      inputRadius={inputRadius}
                    />
                  )}
                </div>
              </Radio>
            </Radio.Group>
          </div>

          {/* Encryption — the server's answer and the bucket's own block. */}
          {real && onEncryptionApplied && (
            <BucketEncryptionGroup
              bucket={name}
              eff={effectiveEncryption}
              blockedReason={encryptionBlockedReason}
              onApplied={onEncryptionApplied}
            />
          )}
          {!real && hasOwnEncryption(row) && (
            <div style={{ marginTop: 12 }}>
              <Text style={{ ...groupLabel, color: colors.TEXT_MUTED }}>Encryption</Text>
              <Text type="secondary" style={{ fontSize: 12 }}>
                These settings carry the bucket&apos;s own encryption ({String(row?.encryption?.mode ?? '')}
                {row?.encryption?.key_id ? `, key id ${String(row.encryption.key_id)}` : ''}). It applies
                when the bucket is created. The name cannot change while the settings hold a key;
                to drop them, use Remove these settings below.
              </Text>
            </div>
          )}

          {/* Placement — backend routing + the honest move. */}
          {backends.length > 0 && (
            <div style={{ marginTop: 12 }}>
              <Text id={`${uid}-backend`} style={{ ...groupLabel, color: colors.TEXT_MUTED }}>Backend</Text>
              <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap' }}>
                <Select
                  aria-labelledby={`${uid}-backend`}
                  value={eff.backend}
                  onChange={handleBackendChange}
                  size="small"
                  showSearch={{ optionFilterProp: 'label' }}
                  style={{ width: 260 }}
                  options={[
                    {
                      value: '',
                      label: `Default — ${defaultBackend ?? 'default backend'}`,
                      sublabel: 'follows the default backend',
                    },
                    ...backends.map((b) => ({
                      value: b.name,
                      label: b.name,
                      sublabel: b.backend_type,
                    })),
                  ]}
                  optionRender={(opt) => (
                    <div>
                      <div>{opt.data.label}</div>
                      {opt.data.sublabel && (
                        <Text type="secondary" style={{ fontSize: 11, display: 'block' }}>{opt.data.sublabel}</Text>
                      )}
                    </div>
                  )}
                />
                {real && backends.length > 1 && (
                  <Button
                    size="small"
                    type="link"
                    style={{ fontSize: 11, padding: 0, height: 'auto' }}
                    onClick={() => setMigrateOpen(true)}
                  >
                    Migrate data…
                  </Button>
                )}
              </div>
            </div>
          )}

          {real && (
            <MigrateBucketModal
              open={migrateOpen}
              bucket={name}
              onClose={() => setMigrateOpen(false)}
            />
          )}

          {/* Advanced — rare knobs behind a disclosure; auto-open when in use. */}
          {/* The rules that read or write the bucket, and its runs. */}
          {real && (
            <div style={{ paddingTop: 12 }}>
              <Text style={{ ...groupLabel, color: colors.TEXT_MUTED }}>Rules and runs</Text>
              <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 8, marginTop: 8, fontSize: 12 }}>
                {rules.length === 0 && <Text type="secondary" style={{ fontSize: 12 }}>No replication or lifecycle rule uses this bucket.</Text>}
                {rules.map((r) => (
                  <Button
                    key={r.id}
                    size="small"
                    onClick={(e) => {
                      e.stopPropagation();
                      navigate(buildViewUrl('admin', `storage/${r.kind}`, { job: r.id }));
                    }}
                    title={BUCKET_RULE_ROLE[r.role]}
                  >
                    {r.kind === 'replication' ? 'Replication' : 'Lifecycle'}{' '}
                    <span style={{ fontFamily: 'var(--font-mono)' }}>{r.name}</span>
                    <Text type="secondary" style={{ fontSize: 11 }}>
                      ({r.role})
                    </Text>
                  </Button>
                ))}
                <Button
                  size="small"
                  type="link"
                  onClick={(e) => {
                    e.stopPropagation();
                    navigate(runsUrl({ bucket: name }));
                  }}
                >
                  Runs on this bucket →
                </Button>
              </div>
            </div>
          )}
          <Collapse
            ghost
            size="small"
            style={{ marginTop: 12, marginLeft: -8 }}
            defaultActiveKey={advancedActive ? ['adv'] : []}
            items={[
              {
                key: 'adv',
                label: (
                  <Text style={{ fontSize: 13, fontWeight: 600, color: colors.TEXT_MUTED }}>
                    Advanced{advancedActive ? ' · in use' : ''}
                  </Text>
                ),
                children: (
                  <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                    <div style={formRow(8, { flexWrap: 'wrap' })}>
                      <Text id={`${uid}-compression`} style={{ fontSize: 12, fontFamily: 'var(--font-ui)', color: colors.TEXT_MUTED, width: 150 }}>
                        Compression
                      </Text>
                      <Select
                        aria-labelledby={`${uid}-compression`}
                        size="small"
                        style={{ minWidth: 220, ...inputRadius }}
                        value={eff.compression === null ? 'inherit' : eff.compression ? 'on' : 'off'}
                        onChange={(v) => {
                          if (v === 'inherit') onPatch({ compression: null });
                          else onPatch({ compression: v === 'on' });
                        }}
                        options={[
                          {
                            value: 'inherit',
                            label: `Inherit — global default (${globalCompressionOn ? 'on' : 'off'})`,
                          },
                          { value: 'on', label: 'Always on' },
                          { value: 'off', label: 'Off' },
                        ]}
                      />
                    </div>
                    {eff.compression !== false && (
                      <div style={formRow(8, { flexWrap: 'wrap' })}>
                        <Text id={`${uid}-ratio`} style={{ fontSize: 12, color: colors.TEXT_MUTED, width: 150 }} title="A delta is kept only when delta-size / original-size is below this cutoff; otherwise the file is stored as-is.">
                          Delta size cutoff
                        </Text>
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 2, minWidth: 0 }}>
                          <Input
                            aria-labelledby={`${uid}-ratio`}
                            aria-invalid={ratioError ? true : undefined}
                            aria-describedby={ratioError ? `${uid}-ratio-error` : undefined}
                            status={ratioError ? 'error' : undefined}
                            inputMode="decimal"
                            value={ratioText}
                            onChange={(e) => onRatioText(e.target.value)}
                            placeholder={`${globalRatio} — global default`}
                            style={{ width: 170, ...inputRadius }}
                            size="small"
                          />
                          {ratioError && (
                            <Text id={`${uid}-ratio-error`} role="alert" type="danger" style={{ fontSize: 12 }}>
                              {ratioError}
                            </Text>
                          )}
                        </div>
                      </div>
                    )}
                    {eff.compression !== false && (
                      <div style={formRow(8, { flexWrap: 'wrap', alignItems: 'flex-start' })}>
                        <Text id={`${uid}-types`} style={{ fontSize: 12, color: colors.TEXT_MUTED, width: 150, paddingTop: 4 }}>
                          File types
                        </Text>
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 8, flex: 1, minWidth: 260 }}>
                          <Text aria-hidden style={{ fontSize: 12, color: colors.TEXT_SECONDARY }}>
                            Also compress
                          </Text>
                          <Select
                            mode="tags"
                            size="small"
                            aria-label="Also compress these file types"
                            placeholder="For example pkl"
                            value={eff.delta_add}
                            onChange={(v: string[]) =>
                              onPatch({ delta_add: [...new Set(v.map(normalizeExtension).filter(Boolean))] })
                            }
                            open={false}
                            suffixIcon={null}
                            style={{ width: '100%', ...inputRadius }}
                          />
                          <Text aria-hidden style={{ fontSize: 12, color: colors.TEXT_SECONDARY }}>
                            Store whole
                          </Text>
                          <Select
                            mode="multiple"
                            size="small"
                            aria-label="Store these built-in file types whole"
                            placeholder="For example sql"
                            value={eff.delta_remove}
                            onChange={(v: string[]) => onPatch({ delta_remove: v })}
                            options={defaultDeltaExtensions.map((e) => ({ value: e, label: e }))}
                            style={{ width: '100%', ...inputRadius }}
                          />
                          <Text
                            style={{ fontSize: 12, color: colors.TEXT_MUTED }}
                            title={defaultDeltaExtensions.join(', ')}
                          >
                            Built-in: {defaultDeltaExtensions.length} types, such as{' '}
                            {defaultDeltaExtensions.slice(0, 6).join(', ')}. A delta that is not small enough is still stored whole.
                          </Text>
                        </div>
                      </div>
                    )}
                    <div style={formRow(8, { flexWrap: 'wrap' })}>
                      <Text id={`${uid}-alias`} style={{ fontSize: 12, color: colors.TEXT_MUTED, width: 150 }} title="Store this bucket under a different real name on the backend.">
                        Real name on backend
                      </Text>
                      <Input
                        aria-labelledby={`${uid}-alias`}
                        value={eff.alias}
                        onChange={(e) => onPatch({ alias: e.target.value })}
                        placeholder={`same as name${name ? ` (${name})` : ''}`}
                        style={{ width: 220, ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 11 }}
                        size="small"
                      />
                    </div>
                    <div style={formRow(8, { flexWrap: 'wrap' })}>
                      <Text id={`${uid}-quota`} style={{ fontSize: 12, color: colors.TEXT_MUTED, width: 150 }}>
                        Quota
                      </Text>
                      <div style={{ display: 'flex', flexDirection: 'column', gap: 2, minWidth: 0 }}>
                        <Input
                          aria-labelledby={`${uid}-quota`}
                          aria-invalid={quotaError ? true : undefined}
                          aria-describedby={quotaError ? `${uid}-quota-error` : undefined}
                          status={quotaError ? 'error' : undefined}
                          inputMode="decimal"
                          value={quotaText}
                          onChange={(e) => onQuotaText(e.target.value)}
                          placeholder="No quota"
                          style={{ width: 170, ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 11 }}
                          size="small"
                          suffix="GiB"
                        />
                        {quotaError && (
                          <Text id={`${uid}-quota-error`} role="alert" type="danger" style={{ fontSize: 12 }}>
                            {quotaError}
                          </Text>
                        )}
                      </div>
                    </div>
                  </div>
                ),
              },
            ]}
          />

          {/* Row-level actions. Reset stages defaults (reviewed on Apply). */}
          {(hasOverrides || onRemoveDraft || (onReencrypt && !maintenanceJob)) && (
            <div style={{ marginTop: 12, display: 'flex', gap: 12, alignItems: 'center' }}>
              {onReencrypt && !maintenanceJob && (
                <Button
                  size="small"
                  type="text"
                  icon={<SyncOutlined style={{ fontSize: 11 }} />}
                  style={{ fontSize: 11, padding: '0 4px' }}
                  title="Rewrite every object so its at-rest encryption matches the backend's current setting"
                  onClick={onReencrypt}
                >
                  Re-encrypt existing objects
                </Button>
              )}
              {/* The card's own actions, as plain text buttons at its foot
                  (a "⋯" menu with one item hid them, and its popup spilled
                  over the card edge). Each one is confirmed. */}
              {cardActions.length > 0 && (
                <span style={{ marginLeft: 'auto', display: 'flex', gap: 8 }}>
                  {cardActions.map((a) => (
                    <Button
                      key={a.key}
                      size="small"
                      type="text"
                      style={{ fontSize: 11, padding: '0 4px' }}
                      onClick={async () => {
                        if (await confirmDialog({ ...a.confirm, danger: true })) a.onSelect();
                      }}
                    >
                      {a.label}
                    </Button>
                  ))}
                </span>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
