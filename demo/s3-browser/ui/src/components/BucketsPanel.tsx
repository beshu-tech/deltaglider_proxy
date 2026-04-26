/**
 * BucketsPanel — Wave 6 of the admin UI revamp (§7.5).
 *
 * Dedicated editor for per-bucket policies:
 *
 *   * **Compression override** — toggle + optional ratio threshold.
 *   * **Backend routing** — pick a named backend to route this
 *     bucket to.
 *   * **Alias** — virtual → real bucket name map.
 *   * **Quota** — soft storage quota in GiB.
 *   * **Anonymous read access (tri-state)** — the §7.5 UX fix:
 *     None / Entire bucket / Specific prefixes.
 *
 * The tri-state replaces the old "Public Prefixes list" that left
 * operators guessing whether an empty-string sentinel meant "no
 * public access" or "entire bucket public" (it's the latter, but
 * nothing in the UI said so). The radio group makes the intent
 * explicit; the backend's `public: true` shorthand handles the
 * entire-bucket case losslessly.
 *
 * Uses the section-level storage Apply flow so dirty-state indicators,
 * beforeunload, and Cmd/Ctrl+S behave like the rest of Configuration.
 */
import { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Input,
  InputNumber,
  Modal,
  Radio,
  Space,
  Switch,
  Typography,
  message,
} from 'antd';
import {
  CloudOutlined,
  DeleteOutlined,
  ExclamationCircleOutlined,
  PlusOutlined,
} from '@ant-design/icons';
import type { AdminConfig, BackendInfo, SectionApplyResponse } from '../adminApi';
import { getAdminConfig, getBackends, putSection, validateSection } from '../adminApi';
import { resolveBackendFor, describeEncryption } from '../encryptionUi';
import { listBuckets } from '../s3client';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import SimpleSelect from './SimpleSelect';
import SimpleAutoComplete from './SimpleAutoComplete';
import ApplyDialog from './ApplyDialog';
import { useApplyHandler, useDirtySection } from '../useDirtySection';

const { Text } = Typography;

/** Local working shape — mirrors the backend `BucketPolicyConfig`
 *  but normalises nulls to undefined for the form controllers. */
interface BucketPolicyRow {
  name: string;
  compression: boolean;
  max_delta_ratio: number | null;
  backend: string;
  alias: string;
  /** Tri-state source of truth for the anonymous-read radio group. */
  publicMode: 'none' | 'entire' | 'prefixes';
  /** Specific prefixes — only surfaced when `publicMode === 'prefixes'`. */
  public_prefixes: string[];
  quota_bytes: number | null;
}

interface Props {
  onSessionExpired?: () => void;
}

function policyToRow(
  name: string,
  p: NonNullable<AdminConfig['bucket_policies']>[string]
): BucketPolicyRow {
  // Determine the tri-state from the persisted shape:
  //   * `public: true` (shorthand)          -> entire
  //   * `public_prefixes: [""]` (expanded)   -> entire
  //   * `public_prefixes: ["builds/", ...]`  -> prefixes
  //   * anything else                        -> none
  let publicMode: 'none' | 'entire' | 'prefixes' = 'none';
  let prefixes: string[] = [];
  if (p.public === true) {
    publicMode = 'entire';
  } else if (p.public_prefixes && p.public_prefixes.length > 0) {
    if (p.public_prefixes.length === 1 && p.public_prefixes[0] === '') {
      publicMode = 'entire';
    } else {
      publicMode = 'prefixes';
      prefixes = p.public_prefixes.slice();
    }
  }
  return {
    name,
    compression: p.compression ?? true,
    max_delta_ratio: p.max_delta_ratio ?? null,
    backend: p.backend ?? '',
    alias: p.alias ?? '',
    publicMode,
    public_prefixes: prefixes,
    quota_bytes: p.quota_bytes ?? null,
  };
}

function rowToPolicy(row: BucketPolicyRow): {
  compression?: boolean;
  max_delta_ratio?: number;
  backend?: string;
  alias?: string;
  public_prefixes?: string[];
  quota_bytes?: number;
} {
  // Serialise the tri-state back to the wire shape the backend
  // accepts. `entire` uses the empty-string sentinel `[""]` — the
  // backend's `BucketPolicyConfig::normalize` collapses it to
  // `public: true` on re-serialisation, lossless round-trip.
  const out: ReturnType<typeof rowToPolicy> = {
    compression: row.compression,
  };
  if (row.max_delta_ratio != null) out.max_delta_ratio = row.max_delta_ratio;
  if (row.backend) out.backend = row.backend;
  if (row.alias) out.alias = row.alias;
  if (row.quota_bytes != null) out.quota_bytes = row.quota_bytes;
  if (row.publicMode === 'entire') {
    out.public_prefixes = [''];
  } else if (row.publicMode === 'prefixes') {
    const cleaned = row.public_prefixes.map((p) => p.trim()).filter((p) => p.length > 0);
    if (cleaned.length > 0) out.public_prefixes = cleaned;
  }
  return out;
}

export default function BucketsPanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();

  const {
    value: rows,
    setValue: setRows,
    isDirty: dirty,
    discard,
    markApplied,
    resetWith,
  } = useDirtySection<BucketPolicyRow[]>('storage', []);
  const [applyOpen, setApplyOpen] = useState(false);
  const [applyResponse, setApplyResponse] = useState<SectionApplyResponse | null>(null);
  const [pendingBody, setPendingBody] = useState<{ buckets: AdminConfig['bucket_policies'] } | null>(null);
  const [applying, setApplying] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [defaultBackend, setDefaultBackend] = useState<string | null>(null);
  const [availableBuckets, setAvailableBuckets] = useState<string[]>([]);

  const refresh = useCallback(async () => {
    try {
      setLoading(true);
      const [cfg, bs, realBuckets] = await Promise.all([
        getAdminConfig(),
        getBackends().then((r) => r.backends).catch(() => [] as BackendInfo[]),
        listBuckets().catch(() => [] as Array<{ name: string }>),
      ]);
      if (!cfg) {
        onSessionExpired?.();
        return;
      }
      const nextRows = Object.entries(cfg.bucket_policies || {}).map(([name, p]) =>
        policyToRow(name, p)
      );
      nextRows.sort((a, b) => a.name.localeCompare(b.name));
      resetWith(nextRows);
      // Prefer the /api/admin/config response's `backends` array —
      // it synthesises a "default" entry on the singleton-backend
      // path, so the per-bucket encryption badge works uniformly
      // regardless of YAML shape. Fall back to /api/admin/backends
      // when the primary endpoint doesn't carry backends (legacy
      // response shapes).
      setBackends(cfg.backends && cfg.backends.length > 0 ? cfg.backends : bs);
      setDefaultBackend(cfg.default_backend ?? null);
      setAvailableBuckets(realBuckets.map((b) => b.name));
      setError(null);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(
        `Failed to load bucket policies: ${e instanceof Error ? e.message : 'unknown'}`
      );
    } finally {
      setLoading(false);
    }
  }, [onSessionExpired, resetWith]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const updateRow = (idx: number, patch: Partial<BucketPolicyRow>) => {
    const next = rows.slice();
    next[idx] = { ...next[idx], ...patch };
    setRows(next);
  };

  const addRow = () => {
    setRows([
      ...rows,
      {
        name: '',
        compression: true,
        max_delta_ratio: null,
        backend: '',
        alias: '',
        publicMode: 'none',
        public_prefixes: [],
        quota_bytes: null,
      },
    ]);
  };

  const deleteRow = (idx: number) => {
    const name = rows[idx].name;
    Modal.confirm({
      title: `Remove bucket policy for "${name || '(unnamed)'}"?`,
      icon: <ExclamationCircleOutlined />,
      content: (
        <Text type="secondary" style={{ fontSize: 13 }}>
          The bucket itself is not deleted. Only the per-bucket policy
          overrides go away — compression / quota / public-read
          settings revert to the defaults.
        </Text>
      ),
      okText: 'Remove',
      okButtonProps: { danger: true },
      cancelText: 'Cancel',
      onOk: () => {
        setRows(rows.filter((_, i) => i !== idx));
      },
    });
  };

  const buildPayload = useCallback((): { buckets: AdminConfig['bucket_policies'] } | null => {
    // Validate — bucket names must be non-empty + lowercase +
    // [a-z0-9.\-] (what the backend accepts). Empty names are
    // genuinely-unfilled rows (just dropped); duplicates surface
    // as an error.
    const cleaned = rows.filter((r) => r.name.trim());
    const names = cleaned.map((r) => r.name);
    const dupes = names.filter((n, i) => names.indexOf(n) !== i);
    if (dupes.length > 0) {
      message.error(`Duplicate bucket name: ${dupes[0]}`);
      return null;
    }
    const bp: AdminConfig['bucket_policies'] = {};
    for (const row of cleaned) {
      bp[row.name] = rowToPolicy(row);
    }
    return { buckets: bp };
  }, [rows]);

  const runApply = useCallback(async () => {
    const body = buildPayload();
    if (!body) return;
    try {
      const resp = await validateSection('storage', body);
      setApplyResponse(resp);
      setPendingBody(body);
      setApplyOpen(true);
    } catch (e) {
      message.error(`Validate failed: ${e instanceof Error ? e.message : 'unknown'}`);
    }
  }, [buildPayload]);

  const confirmApply = useCallback(async () => {
    if (!pendingBody) return;
    setApplying(true);
    try {
      const resp = await putSection('storage', pendingBody);
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
      await refresh();
    } catch (e) {
      message.error(`Apply failed: ${e instanceof Error ? e.message : 'unknown'}`);
      setApplyOpen(false);
      setPendingBody(null);
      await refresh();
    } finally {
      setApplying(false);
    }
  }, [markApplied, pendingBody, refresh]);

  const cancelApply = useCallback(() => {
    setApplyOpen(false);
    setPendingBody(null);
  }, []);

  useApplyHandler('storage', runApply, dirty);

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
          description="Bucket policies are storage config. Review the section diff before applying."
          action={
            <Space>
              <Button size="small" onClick={discard} disabled={applying}>
                Discard
              </Button>
              <Button
                type="primary"
                size="small"
                onClick={runApply}
                loading={applying}
              >
                Review apply
              </Button>
            </Space>
          }
        />
      )}

      <div style={cardStyle}>
        <SectionHeader
          icon={<CloudOutlined />}
          title="Per-bucket policies"
          description={
            loading
              ? 'Loading...'
              : rows.length === 0
                ? 'No overrides. Buckets use the global compression default and are not publicly readable.'
                : `${rows.length} bucket${rows.length === 1 ? '' : 's'} with custom policies.`
          }
        />

        <div style={{ marginTop: 16, display: 'flex', flexDirection: 'column', gap: 12 }}>
          {rows.map((row, idx) => (
            <BucketCard
              key={idx}
              row={row}
              backends={backends}
              defaultBackend={defaultBackend}
              availableBuckets={availableBuckets.filter(
                (b) => !rows.some((r, i) => i !== idx && r.name === b)
              )}
              onChange={(patch) => updateRow(idx, patch)}
              onDelete={() => deleteRow(idx)}
              inputRadius={inputRadius}
            />
          ))}

          <Button
            icon={<PlusOutlined />}
            onClick={addRow}
            style={{ marginTop: 4, borderRadius: 8, fontFamily: 'var(--font-ui)' }}
            block
            type="dashed"
          >
            Add bucket policy
          </Button>
        </div>

        {dirty && (
          <Button
            type="primary"
            onClick={runApply}
            loading={applying}
            style={{ marginTop: 16, borderRadius: 8, fontWeight: 600 }}
            block
          >
            Review {rows.filter((r) => r.name.trim()).length} bucket polic
            {rows.filter((r) => r.name.trim()).length === 1 ? 'y' : 'ies'}
          </Button>
        )}
      </div>

      <ApplyDialog
        open={applyOpen}
        section="storage"
        response={applyResponse}
        onApply={confirmApply}
        onCancel={cancelApply}
        loading={applying}
      />

      {/* Informational footer about secret bucket policy mechanics */}
      <Text type="secondary" style={{ fontSize: 11, lineHeight: 1.6 }}>
        Per-bucket policies are written to <code>storage.buckets.&lt;name&gt;.*</code> in
        YAML. Entire-bucket public read uses the <code>public: true</code>
        {' '}shorthand; specific prefixes serialise as{' '}
        <code>public_prefixes: [&quot;...&quot;]</code>. The backend round-trips between
        the two forms losslessly.
      </Text>
    </div>
  );
}

// ── BucketCard ──────────────────────────────────────────────

interface CardProps {
  row: BucketPolicyRow;
  backends: BackendInfo[];
  availableBuckets: string[];
  /**
   * Name of the configured default backend. A bucket with no
   * explicit `backend` override routes here. The per-bucket
   * encryption badge resolves against this when the row's backend
   * field is empty.
   */
  defaultBackend: string | null;
  onChange: (patch: Partial<BucketPolicyRow>) => void;
  onDelete: () => void;
  inputRadius: { borderRadius: number };
}

function BucketCard({
  row,
  backends,
  availableBuckets,
  defaultBackend,
  onChange,
  onDelete,
  inputRadius,
}: CardProps) {
  const colors = useColors();

  const isPublic = row.publicMode !== 'none';
  const cardBorder = isPublic ? `${colors.ACCENT_AMBER}66` : colors.BORDER;
  const cardBg = isPublic ? `${colors.ACCENT_AMBER}0a` : colors.BG_ELEVATED;

  return (
    <div
      style={{
        border: `1px solid ${cardBorder}`,
        borderRadius: 10,
        background: cardBg,
        padding: 14,
        transition: 'all 0.15s',
      }}
    >
      {/* Header row: bucket name + backend + delete */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 12 }}>
        <SimpleAutoComplete
          value={row.name}
          onChange={(v) => onChange({ name: v.toLowerCase().replace(/[^a-z0-9.-]/g, '') })}
          options={availableBuckets}
          placeholder="Bucket name"
          style={{ flex: 1 }}
        />
        {backends.length > 0 && (
          <SimpleSelect
            value={row.backend}
            onChange={(v) => onChange({ backend: v })}
            placeholder="Route to..."
            allowClear
            size="small"
            style={{ width: 170 }}
            options={backends.map((b) => ({
              value: b.name,
              label: b.name,
              sublabel: b.backend_type,
            }))}
          />
        )}
        <Button
          size="small"
          danger
          icon={<DeleteOutlined />}
          onClick={onDelete}
          aria-label={`Remove ${row.name || 'bucket'}`}
        />
      </div>

      {/* Compression + alias + quota row */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 16,
          flexWrap: 'wrap',
          marginBottom: 12,
        }}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <Switch
            checked={row.compression}
            onChange={(v) => onChange({ compression: v })}
            size="small"
          />
          <Text
            style={{
              fontSize: 12,
              fontFamily: 'var(--font-ui)',
              color: row.compression ? colors.TEXT_PRIMARY : colors.ACCENT_AMBER,
            }}
          >
            {row.compression ? 'Compression' : 'No compression'}
          </Text>
          {row.compression && (
            <>
              <Text style={{ fontSize: 11, color: colors.TEXT_MUTED, marginLeft: 8 }}>
                Ratio:
              </Text>
              <InputNumber
                value={row.max_delta_ratio ?? undefined}
                onChange={(v) => onChange({ max_delta_ratio: v ?? null })}
                min={0}
                max={1}
                step={0.05}
                placeholder="global"
                style={{ width: 80, ...inputRadius }}
                size="small"
              />
            </>
          )}
          {/* Per-bucket encryption-at-rest badge. Resolved via the
             backend this bucket routes to — explicit `row.backend`,
             else `defaultBackend`, else the synthetic "default" entry
             surfaced by the server for the singleton-backend path.
             Each mode gets a distinct label so an operator can tell
             at a glance whether the bucket's storage is proxy-AES,
             SSE-KMS, SSE-S3, or plaintext. */}
          {(() => {
            const info = resolveBackendFor(row.backend, backends, defaultBackend);
            const summary = info?.encryption;
            const { label, tooltip, isEncrypted } = describeEncryption(summary);
            const tone = isEncrypted ? colors.ACCENT_GREEN : colors.TEXT_MUTED;
            return (
              <span
                title={tooltip}
                style={{
                  display: 'inline-flex',
                  alignItems: 'center',
                  gap: 4,
                  marginLeft: 12,
                  fontSize: 10,
                  fontWeight: 600,
                  letterSpacing: 0.4,
                  textTransform: 'uppercase',
                  padding: '2px 8px',
                  borderRadius: 10,
                  background: `${tone}22`,
                  color: tone,
                  cursor: 'help',
                }}
              >
                <span
                  style={{
                    width: 6,
                    height: 6,
                    borderRadius: '50%',
                    background: tone,
                  }}
                />
                {label}
              </span>
            );
          })()}
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <Text style={{ fontSize: 11, color: colors.TEXT_MUTED }}>Alias:</Text>
          <Input
            value={row.alias}
            onChange={(e) => onChange({ alias: e.target.value })}
            placeholder="same as name"
            style={{
              width: 140,
              ...inputRadius,
              fontFamily: 'var(--font-mono)',
              fontSize: 11,
            }}
            size="small"
          />
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <Text
            style={{
              fontSize: 11,
              color: row.quota_bytes != null ? colors.ACCENT_AMBER : colors.TEXT_MUTED,
            }}
          >
            Quota:
          </Text>
          <InputNumber
            value={
              row.quota_bytes != null
                ? Math.round(row.quota_bytes / (1024 * 1024 * 1024))
                : undefined
            }
            onChange={(v) => onChange({ quota_bytes: v != null ? v * 1024 * 1024 * 1024 : null })}
            min={0}
            placeholder="unlimited"
            style={{ width: 100, ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 11 }}
            size="small"
            addonAfter="GB"
          />
        </div>
      </div>

      {/* Anonymous read access — tri-state radio group (§7.5). */}
      <div
        style={{
          borderTop: `1px solid ${colors.BORDER}`,
          paddingTop: 10,
        }}
      >
        <Text
          style={{
            fontSize: 10,
            fontWeight: 700,
            letterSpacing: 0.5,
            textTransform: 'uppercase',
            color: isPublic ? colors.ACCENT_AMBER : colors.TEXT_MUTED,
            fontFamily: 'var(--font-ui)',
            display: 'block',
            marginBottom: 8,
          }}
        >
          Anonymous read access
          {isPublic && (
            <span
              style={{
                fontSize: 10,
                marginLeft: 8,
                fontWeight: 500,
                letterSpacing: 0,
                textTransform: 'none',
              }}
            >
              ⚠ publicly readable
            </span>
          )}
        </Text>
        <Radio.Group
          value={row.publicMode}
          onChange={(e) => {
            const mode = e.target.value as BucketPolicyRow['publicMode'];
            // Switching AWAY from prefixes clears them; switching TO
            // prefixes seeds an empty list so the editor appears.
            onChange({
              publicMode: mode,
              public_prefixes: mode === 'prefixes' ? (row.public_prefixes.length > 0 ? row.public_prefixes : ['']) : [],
            });
          }}
          style={{ display: 'flex', flexDirection: 'column', gap: 6 }}
        >
          <Radio value="none" style={{ alignItems: 'flex-start' }}>
            <div>
              <span style={{ fontSize: 13 }}>None (default)</span>
              <Text
                type="secondary"
                style={{ fontSize: 11, display: 'block', marginTop: 1 }}
              >
                Authenticated SigV4 requests only.
              </Text>
            </div>
          </Radio>
          <Radio value="entire" style={{ alignItems: 'flex-start' }}>
            <div>
              <span style={{ fontSize: 13 }}>Entire bucket</span>
              <Text
                type="secondary"
                style={{ fontSize: 11, display: 'block', marginTop: 1 }}
              >
                All GET/HEAD/LIST requests succeed without credentials.
                YAML: <code style={{ fontFamily: 'var(--font-mono)' }}>public: true</code>.
              </Text>
            </div>
          </Radio>
          <Radio value="prefixes" style={{ alignItems: 'flex-start' }}>
            <div style={{ width: '100%' }}>
              <span style={{ fontSize: 13 }}>Specific prefixes</span>
              <Text
                type="secondary"
                style={{ fontSize: 11, display: 'block', marginTop: 1 }}
              >
                Only keys under these prefixes are publicly readable.
                Use a trailing <code>/</code> for directory-aligned matching.
              </Text>
              {row.publicMode === 'prefixes' && (
                <div
                  style={{
                    marginTop: 8,
                    paddingLeft: 16,
                    display: 'flex',
                    flexDirection: 'column',
                    gap: 4,
                  }}
                >
                  {row.public_prefixes.map((prefix, pi) => (
                    <div
                      key={pi}
                      style={{ display: 'flex', alignItems: 'center', gap: 4 }}
                    >
                      <Input
                        value={prefix}
                        onChange={(e) => {
                          const next = row.public_prefixes.slice();
                          next[pi] = e.target.value;
                          onChange({ public_prefixes: next });
                        }}
                        onBlur={(e) => {
                          const v = e.target.value.trim();
                          if (v && !v.endsWith('/')) {
                            const next = row.public_prefixes.slice();
                            next[pi] = v + '/';
                            onChange({ public_prefixes: next });
                          }
                        }}
                        placeholder="e.g. builds/"
                        style={{
                          flex: 1,
                          ...inputRadius,
                          fontFamily: 'var(--font-mono)',
                          fontSize: 11,
                        }}
                        size="small"
                      />
                      <Button
                        type="text"
                        size="small"
                        danger
                        onClick={() => {
                          onChange({
                            public_prefixes: row.public_prefixes.filter((_, i) => i !== pi),
                          });
                        }}
                        style={{ padding: '0 8px', minWidth: 0 }}
                      >
                        ×
                      </Button>
                    </div>
                  ))}
                  <Button
                    type="text"
                    size="small"
                    icon={<PlusOutlined />}
                    onClick={() => {
                      onChange({ public_prefixes: [...row.public_prefixes, ''] });
                    }}
                    style={{
                      padding: '0 8px',
                      color: colors.TEXT_MUTED,
                      alignSelf: 'flex-start',
                      fontSize: 11,
                    }}
                  >
                    Add prefix
                  </Button>
                </div>
              )}
            </div>
          </Radio>
        </Radio.Group>
      </div>
    </div>
  );
}
