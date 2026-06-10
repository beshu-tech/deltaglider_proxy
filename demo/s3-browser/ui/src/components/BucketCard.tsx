/**
 * BucketCard — the per-bucket policy render unit for {@link BucketsPanel}.
 *
 * Extracted verbatim from BucketsPanel for file-size / readability. The
 * card owns the compression / backend / alias / quota controls and the
 * tri-state anonymous-read radio group; the specific-prefixes editor is
 * delegated to the sibling {@link PrefixListEditor}.
 *
 * Props carry exactly the data + callbacks the card used as closures
 * before the split — nothing is re-derived here that the parent used to
 * own. In particular `onPrefixesChange` still routes a functional
 * transform through the parent's functional `setRows`, so prefix edits
 * never read a stale closure (recent bug fix preserved).
 */
import { Button, Input, InputNumber, Radio, Select, Typography } from 'antd';
import { DeleteOutlined } from '@ant-design/icons';
import type { BackendInfo } from '../adminApi';
import { resolveBackendFor, describeEncryption } from '../encryptionUi';
import { useColors } from '../ThemeContext';
import SimpleAutoComplete from './SimpleAutoComplete';
import { formRow } from './ruleEditorHelpers';
import type { BucketPolicyRow, PrefixEntry } from './bucketPolicyPayload';
import { freshId } from './bucketPolicyPayload';
import PrefixListEditor from './PrefixListEditor';

const { Text } = Typography;

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
  /** Apply a functional transform to this row's prefix list, by id,
   *  via the parent's functional setRows — never a stale closure. */
  onPrefixesChange: (fn: (prev: PrefixEntry[]) => PrefixEntry[]) => void;
  onDelete: () => void;
  inputRadius: { borderRadius: number };
}

export default function BucketCard({
  row,
  backends,
  availableBuckets,
  defaultBackend,
  onChange,
  onPrefixesChange,
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
          <Select
            value={row.backend || undefined}
            // AntD Select emits `undefined` on clear; BucketPolicyRow.backend is
            // a non-optional string, so coerce back to '' (no-backend = route to
            // the default). Passing undefined through corrupted the row and
            // crashed resolveBackendFor's .trim().
            onChange={(v) => onChange({ backend: v ?? '' })}
            placeholder="Route to..."
            allowClear
            size="small"
            showSearch
            optionFilterProp="label"
            style={{ width: 170 }}
            options={backends.map((b) => ({
              value: b.name,
              label: b.name,
              sublabel: b.backend_type,
            }))}
            optionRender={(opt) => (
              <div>
                <div>{opt.data.label}</div>
                {opt.data.sublabel && (
                  <div style={{ fontSize: 11, opacity: 0.65 }}>{opt.data.sublabel}</div>
                )}
              </div>
            )}
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
      <div style={formRow(16, { flexWrap: 'wrap', marginBottom: 12 })}>
        <div style={formRow(8, { flexWrap: 'wrap' })}>
          <Text style={{ fontSize: 12, fontFamily: 'var(--font-ui)', color: colors.TEXT_MUTED }}>
            Compression
          </Text>
          <Select
            size="small"
            style={{ minWidth: 200, ...inputRadius }}
            value={
              row.compression === null ? 'inherit' : row.compression ? 'on' : 'off'
            }
            onChange={(v) => {
              if (v === 'inherit') onChange({ compression: null });
              else onChange({ compression: v === 'on' });
            }}
            options={[
              {
                value: 'inherit',
                label: 'Default (on) — no override',
              },
              { value: 'on', label: 'On — explicit in YAML' },
              { value: 'off', label: 'Off — explicit in YAML' },
            ]}
          />
          {row.compression !== false && (
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
        <div style={formRow(6)}>
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
        <div style={formRow(6)}>
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
              public_prefixes:
                mode === 'prefixes'
                  ? row.public_prefixes.length > 0
                    ? row.public_prefixes
                    : [{ id: freshId(), value: '' }]
                  : [],
            });
          }}
          style={formRow(6, { flexDirection: 'column', alignItems: 'stretch' })}
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
                <PrefixListEditor
                  prefixes={row.public_prefixes}
                  onPrefixesChange={onPrefixesChange}
                  inputRadius={inputRadius}
                />
              )}
            </div>
          </Radio>
        </Radio.Group>
      </div>
    </div>
  );
}
