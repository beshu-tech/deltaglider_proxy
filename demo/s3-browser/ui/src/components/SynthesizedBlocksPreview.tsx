/**
 * SynthesizedBlocksPreview — the read-only bottom half of the
 * Admission panel (§7.1 of the admin UI revamp plan).
 *
 * The server synthesises an `allow-anonymous` block for every bucket
 * with a non-empty `public_prefixes` list (see
 * `src/admission/default_chain.rs` in the backend). Those blocks are
 * authoritative — editing them directly on Admission would create
 * two sources of truth for the same intent. This panel mirrors the
 * synthesised chain so operators can see the full evaluator order
 * at a glance, but the rows are locked and link to the Storage tab
 * where the source-of-truth lives.
 */
import { Tag, Typography, Tooltip } from 'antd';
import { LockOutlined, RightOutlined } from '@ant-design/icons';
import type { AdminConfig } from '../adminApi';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

interface Props {
  bucketPolicies: AdminConfig['bucket_policies'];
  /** Navigate to the Storage/Buckets tab. */
  onEditInStorage: (bucket: string) => void;
}

interface SynthesisedRow {
  bucket: string;
  name: string;
  prefixes: string[];
}

/**
 * Walk the bucket_policies record and produce one synthesised row
 * per bucket with a non-empty `public_prefixes`. The backend uses
 * `public-prefix:<bucket>` as the block name; we mirror that so the
 * operator can match trace output against this list.
 */
function synthesise(policies: AdminConfig['bucket_policies']): SynthesisedRow[] {
  const out: SynthesisedRow[] = [];
  for (const [bucket, policy] of Object.entries(policies)) {
    const prefixes = policy.public_prefixes ?? [];
    if (prefixes.length === 0) continue;
    out.push({
      bucket,
      name: `public-prefix:${bucket}`,
      prefixes,
    });
  }
  // Stable alphabetical order so reloads don't shuffle the display.
  out.sort((a, b) => a.bucket.localeCompare(b.bucket));
  return out;
}

export default function SynthesizedBlocksPreview({
  bucketPolicies,
  onEditInStorage,
}: Props) {
  const { BORDER, BG_CARD, TEXT_MUTED } = useColors();
  const rows = synthesise(bucketPolicies);

  if (rows.length === 0) {
    return (
      <Text type="secondary" style={{ fontStyle: 'italic' }}>
        No buckets expose public prefixes — no synthesised blocks.
      </Text>
    );
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      {rows.map((row) => (
        <div
          key={row.name}
          style={{
            border: `1px solid ${BORDER}`,
            background: BG_CARD,
            borderRadius: 8,
            padding: '8px 10px',
            display: 'grid',
            gridTemplateColumns: 'auto 1fr auto auto',
            gap: 12,
            alignItems: 'center',
            opacity: 0.75,
          }}
          aria-label={`synthesised block ${row.name}`}
        >
          <Tooltip title="Read-only — edit via Storage → Buckets" placement="left">
            <LockOutlined style={{ color: TEXT_MUTED, fontSize: 14 }} />
          </Tooltip>
          <div style={{ minWidth: 0 }}>
            <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
              <Text
                style={{
                  fontFamily: 'var(--font-mono)',
                  color: TEXT_MUTED,
                }}
              >
                {row.name}
              </Text>
              <Tag color="green">allow-anonymous</Tag>
            </div>
            <Text
              type="secondary"
              style={{
                fontSize: 12,
                display: 'block',
                whiteSpace: 'nowrap',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
              }}
            >
              bucket: {row.bucket} · prefixes:{' '}
              {row.prefixes
                .map((p) => (p === '' ? '(entire bucket)' : p))
                .join(', ')}
            </Text>
          </div>
          <span />
          <Tooltip title="Edit this bucket's public prefixes in Storage">
            <button
              onClick={() => onEditInStorage(row.bucket)}
              style={{
                background: 'transparent',
                border: 'none',
                color: TEXT_MUTED,
                cursor: 'pointer',
                padding: 4,
                display: 'flex',
                alignItems: 'center',
                gap: 4,
                fontSize: 11,
              }}
            >
              Storage <RightOutlined />
            </button>
          </Tooltip>
        </div>
      ))}
    </div>
  );
}
