/**
 * BucketCard — one bucket's status row for {@link BucketsPanel}.
 *
 * Redesigned per the cognitive-load study (docs/plan/storage-ui-cognitive-load.md):
 *
 *   * COLLAPSED (the default): a single status line — bucket name + chips
 *     showing the EFFECTIVE state (backend incl. the default, encryption,
 *     public access, quota, compression override). A read task costs zero
 *     form controls.
 *   * EXPANDED (click): three groups — Public access (the dangerous one,
 *     amber), Placement (backend + migrate), and Advanced (compression /
 *     delta cutoff / real-name alias / quota behind a disclosure).
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
import { useState } from 'react';
import { Button, Collapse, Input, InputNumber, Modal, Progress, Radio, Select, Typography } from 'antd';
import { DownOutlined, RightOutlined, SyncOutlined } from '@ant-design/icons';
import type { BackendInfo } from '../adminApi';
import { resolveBackendFor, describeEncryption } from '../encryptionUi';
import { useColors } from '../ThemeContext';
import SimpleAutoComplete from './SimpleAutoComplete';
import { formRow } from './ruleEditorHelpers';
import type { BucketPolicyRow, PrefixEntry } from './bucketPolicyPayload';
import { DEFAULT_ROW_FIELDS, freshId, isAllDefaultRow } from './bucketPolicyPayload';
import PrefixListEditor from './PrefixListEditor';
import MigrateBucketModal from './MigrateBucketModal';
import type { JobRow } from '../jobsView';
import { progressLabel } from '../jobsView';
import { runJobAction } from '../adminApi';

const { Text } = Typography;

interface CardProps {
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
  /** Global compression default — for resolved labels. */
  globalCompressionOn: boolean;
  globalRatio: number;
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
}

/** Small status chip used in the collapsed row. */
function Chip({ tone, children, title }: { tone: string; children: React.ReactNode; title?: string }) {
  return (
    <span
      title={title}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 4,
        fontSize: 10,
        fontWeight: 600,
        letterSpacing: 0.4,
        textTransform: 'uppercase',
        padding: '2px 8px',
        borderRadius: 10,
        background: `${tone}22`,
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

export default function BucketCard({
  name,
  row,
  real,
  expanded,
  onToggle,
  backends,
  defaultBackend,
  globalCompressionOn,
  globalRatio,
  onPatch,
  onPrefixesChange,
  onDraftNameChange,
  onRemoveDraft,
  availableBuckets = [],
  inputRadius,
  maintenanceJob = null,
  onReencrypt,
}: CardProps) {
  const colors = useColors();
  const [migrateOpen, setMigrateOpen] = useState(false);

  // Effective field values: the policy row when present, defaults otherwise.
  // Fresh fallback object (not the frozen singleton) so the shared
  // `public_prefixes: []` reference can never leak into a materialised row.
  const eff: Omit<BucketPolicyRow, '_id' | 'name'> = row ?? { ...DEFAULT_ROW_FIELDS, public_prefixes: [] };
  const isPublic = eff.publicMode !== 'none';
  const publicPrefixCount = eff.public_prefixes.filter((p) => p.value.trim()).length;
  const hasOverrides = row !== null && !isAllDefaultRow(row);

  const cardBorder = isPublic ? `${colors.ACCENT_AMBER}66` : colors.BORDER;
  const cardBg = isPublic ? `${colors.ACCENT_AMBER}0a` : colors.BG_ELEVATED;

  // Re-routing an ALREADY-routed bucket orphans its objects (explicit route
  // wins; the old backend is never scanned) — confirm first. First-time set
  // is the create path and applies directly.
  const handleBackendChange = (next: string) => {
    const value = next ?? '';
    const prev = eff.backend;
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
  const encryptionInfo = (() => {
    const info = resolveBackendFor(eff.backend, backends, defaultBackend);
    return describeEncryption(info?.encryption);
  })();
  const advancedActive =
    eff.compression !== null || eff.max_delta_ratio !== null || eff.alias !== '' || eff.quota_bytes !== null;

  const chips: React.ReactNode[] = [];
  if (maintenanceJob) {
    const pct = maintenanceJob.percent ?? null;
    chips.push(
      <Chip
        key="busy"
        tone={colors.ACCENT_AMBER}
        title={
          maintenanceJob.kind === 'migrate'
            ? 'Migrating this bucket to another backend. Reads work; uploads and deletes are temporarily rejected.'
            : 'A re-encryption job is rewriting this bucket. Reads work; uploads and deletes are temporarily rejected.'
        }
      >
        <SyncOutlined spin style={{ fontSize: 10 }} /> busy{pct != null ? ` ${pct}%` : ''}
      </Chip>
    );
  }
  if (!real) {
    chips.push(
      <Chip key="missing" tone={colors.ACCENT_AMBER} title="No bucket with this name exists yet — the policy applies once it's created.">
        bucket not found
      </Chip>
    );
  }
  chips.push(
    eff.backend ? (
      <Chip key="backend" tone={colors.ACCENT_BLUE} title={`Explicitly routed to ${eff.backend}`}>
        → {eff.backend}
      </Chip>
    ) : (
      <Chip key="backend" tone={colors.TEXT_MUTED} title="No explicit route — uses the default backend">
        default{defaultBackend ? ` — ${defaultBackend}` : ''}
      </Chip>
    )
  );
  chips.push(
    <Chip
      key="enc"
      tone={encryptionInfo.isEncrypted ? colors.ACCENT_GREEN : colors.TEXT_MUTED}
      title={encryptionInfo.tooltip}
    >
      {encryptionInfo.label}
    </Chip>
  );
  chips.push(
    eff.publicMode === 'none' ? (
      <Chip key="pub" tone={colors.TEXT_MUTED} title="Authenticated requests only">
        private
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
        {eff.publicMode === 'entire' ? '⚠ public bucket' : `⚠ ${publicPrefixCount} public prefix${publicPrefixCount === 1 ? '' : 'es'}`}
      </Chip>
    )
  );
  if (eff.quota_bytes != null) {
    chips.push(
      <Chip key="quota" tone={colors.ACCENT_AMBER} title="Storage quota">
        ≤ {Math.round(eff.quota_bytes / (1024 * 1024 * 1024))} GB
      </Chip>
    );
  }
  if (eff.compression !== null || eff.max_delta_ratio !== null) {
    chips.push(
      <Chip key="comp" tone={colors.ACCENT_PURPLE} title="Compression override on this bucket">
        {eff.compression === false
          ? 'compression off'
          : eff.max_delta_ratio !== null
            ? `delta ≤ ${eff.max_delta_ratio}`
            : 'compression on'}
      </Chip>
    );
  }

  const groupLabel: React.CSSProperties = {
    fontSize: 10,
    fontWeight: 700,
    letterSpacing: 0.5,
    textTransform: 'uppercase',
    fontFamily: 'var(--font-ui)',
    display: 'block',
    marginBottom: 8,
  };

  return (
    <div
      style={{
        border: `1px solid ${cardBorder}`,
        borderRadius: 10,
        background: cardBg,
        transition: 'all 0.15s',
      }}
    >
      {/* ── Collapsed status row: name + effective-state chips ── */}
      <div
        role="button"
        tabIndex={0}
        aria-expanded={expanded}
        aria-label={`${name || 'new bucket policy'} — click to ${expanded ? 'collapse' : 'edit'}`}
        onClick={onToggle}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            onToggle();
          }
        }}
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 10,
          padding: '10px 14px',
          cursor: 'pointer',
          minHeight: 42,
        }}
      >
        {expanded ? (
          <DownOutlined style={{ fontSize: 10, color: colors.TEXT_MUTED }} />
        ) : (
          <RightOutlined style={{ fontSize: 10, color: colors.TEXT_MUTED }} />
        )}
        {onDraftNameChange ? (
          <span onClick={(e) => e.stopPropagation()} style={{ flex: 1, minWidth: 160 }}>
            <SimpleAutoComplete
              value={name}
              onChange={(v) => onDraftNameChange(v.toLowerCase().replace(/[^a-z0-9.-]/g, ''))}
              options={availableBuckets}
              placeholder="Bucket name"
              style={{ width: '100%' }}
            />
          </span>
        ) : (
          <Text
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 13,
              fontWeight: 600,
              color: colors.TEXT_PRIMARY,
              flexShrink: 0,
            }}
          >
            {name}
          </Text>
        )}
        <span style={{ flex: 1 }} />
        <span style={{ display: 'flex', gap: 6, flexWrap: 'wrap', justifyContent: 'flex-end' }}>{chips}</span>
      </div>

      {/* ── Maintenance progress: visible on every render of the row ── */}
      {maintenanceJob && (
        <div style={{ padding: '0 14px 10px', display: 'flex', alignItems: 'center', gap: 12 }}>
          <Progress
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
                void runJobAction(maintenanceJob.id, 'cancel').catch(() => {
                  /* next poll reflects the real state either way */
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
        <div style={{ padding: '0 14px 14px', borderTop: `1px solid ${colors.BORDER}` }}>
          {/* Public access — the consequential group, first and amber. */}
          <div style={{ paddingTop: 12 }}>
            <Text style={{ ...groupLabel, color: isPublic ? colors.ACCENT_AMBER : colors.TEXT_MUTED }}>
              Public access
              {isPublic && (
                <span style={{ fontSize: 10, marginLeft: 8, fontWeight: 500, letterSpacing: 0, textTransform: 'none' }}>
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
              <Radio value="none" style={{ alignItems: 'flex-start' }}>
                <div>
                  <span style={{ fontSize: 13 }}>Private (default)</span>
                  <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 1 }}>
                    Authenticated requests only.
                  </Text>
                </div>
              </Radio>
              <Radio value="entire" style={{ alignItems: 'flex-start' }}>
                <div>
                  <span style={{ fontSize: 13 }}>Entire bucket public</span>
                  <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 1 }}>
                    Anyone can read and list every object — no credentials needed.
                  </Text>
                </div>
              </Radio>
              <Radio value="prefixes" style={{ alignItems: 'flex-start' }}>
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

          {/* Placement — backend routing + the honest move. */}
          {backends.length > 0 && (
            <div style={{ marginTop: 14 }}>
              <Text style={{ ...groupLabel, color: colors.TEXT_MUTED }}>Backend</Text>
              <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
                <Select
                  value={eff.backend}
                  onChange={handleBackendChange}
                  size="small"
                  showSearch
                  optionFilterProp="label"
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
                        <div style={{ fontSize: 11, opacity: 0.65 }}>{opt.data.sublabel}</div>
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
          <Collapse
            ghost
            size="small"
            style={{ marginTop: 10, marginLeft: -8 }}
            defaultActiveKey={advancedActive ? ['adv'] : []}
            items={[
              {
                key: 'adv',
                label: (
                  <Text style={{ fontSize: 11, fontWeight: 600, color: colors.TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 0.5 }}>
                    Advanced{advancedActive ? ' · in use' : ''}
                  </Text>
                ),
                children: (
                  <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
                    <div style={formRow(8, { flexWrap: 'wrap' })}>
                      <Text style={{ fontSize: 12, fontFamily: 'var(--font-ui)', color: colors.TEXT_MUTED, width: 150 }}>
                        Compression
                      </Text>
                      <Select
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
                        <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, width: 150 }} title="A delta is kept only when delta-size / original-size is below this cutoff; otherwise the file is stored as-is.">
                          Delta size cutoff
                        </Text>
                        <InputNumber
                          value={eff.max_delta_ratio ?? undefined}
                          onChange={(v) => onPatch({ max_delta_ratio: v ?? null })}
                          min={0}
                          max={1}
                          step={0.05}
                          placeholder={`${globalRatio} — global default`}
                          style={{ width: 170, ...inputRadius }}
                          size="small"
                        />
                      </div>
                    )}
                    <div style={formRow(8, { flexWrap: 'wrap' })}>
                      <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, width: 150 }} title="Store this bucket under a different real name on the backend.">
                        Real name on backend
                      </Text>
                      <Input
                        value={eff.alias}
                        onChange={(e) => onPatch({ alias: e.target.value })}
                        placeholder={`same as name${name ? ` (${name})` : ''}`}
                        style={{ width: 220, ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 11 }}
                        size="small"
                      />
                    </div>
                    <div style={formRow(8, { flexWrap: 'wrap' })}>
                      <Text style={{ fontSize: 12, color: eff.quota_bytes != null ? colors.ACCENT_AMBER : colors.TEXT_MUTED, width: 150 }}>
                        Quota
                      </Text>
                      <InputNumber
                        value={eff.quota_bytes != null ? Math.round(eff.quota_bytes / (1024 * 1024 * 1024)) : undefined}
                        onChange={(v) => onPatch({ quota_bytes: v != null ? v * 1024 * 1024 * 1024 : null })}
                        min={0}
                        placeholder="Unlimited"
                        style={{ width: 170, ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 11 }}
                        size="small"
                        addonAfter="GB"
                      />
                    </div>
                  </div>
                ),
              },
            ]}
          />

          {/* Row-level actions. Reset stages defaults (reviewed on Apply). */}
          {(hasOverrides || onRemoveDraft || (onReencrypt && !maintenanceJob)) && (
            <div style={{ marginTop: 10, display: 'flex', gap: 12, alignItems: 'center' }}>
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
              {hasOverrides && (
                <Button
                  size="small"
                  type="text"
                  danger
                  style={{ fontSize: 11, padding: '0 4px' }}
                  onClick={() => onPatch({ ...DEFAULT_ROW_FIELDS })}
                >
                  Reset to defaults
                </Button>
              )}
              {onRemoveDraft && (
                <Button
                  size="small"
                  type="text"
                  danger
                  style={{ fontSize: 11, padding: '0 4px' }}
                  onClick={onRemoveDraft}
                >
                  Remove draft
                </Button>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
