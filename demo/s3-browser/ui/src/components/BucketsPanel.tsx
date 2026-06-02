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
  Modal,
  Space,
  Typography,
  message,
} from 'antd';
import {
  CloudOutlined,
  ExclamationCircleOutlined,
  PlusOutlined,
} from '@ant-design/icons';
import type { AdminConfig, BackendInfo } from '../adminApi';
import { getBackends } from '../adminApi';
import { useAdminConfig } from '../queries/config';
import { listBuckets } from '../s3client';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import ApplyDialog from './ApplyDialog';
import BucketCard from './BucketCard';
import { useApplyHandler } from '../useDirtySection';
import { useSectionEditor } from '../useSectionEditor';
import type { BucketPolicyRow } from './bucketPolicyPayload';
import { buildBucketPayload, freshId, policyToRow } from './bucketPolicyPayload';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

type BucketWire = { buckets: AdminConfig['bucket_policies'] };

export default function BucketsPanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();

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
    dirtyKey: 'configuration/storage/buckets',
    initial: [],
    onSessionExpired,
    noun: 'bucket policies',
    // The fetch path also side-loads backends / availableBuckets via a
    // separate effect below; `pick` only owns the row-editing shape.
    pick: (body) => {
      const nextRows = Object.entries(body.buckets || {}).map(([name, p]) =>
        policyToRow(name, p)
      );
      nextRows.sort((a, b) => a.name.localeCompare(b.name));
      return nextRows;
    },
    // `toPayload` re-runs the pure builder. The guarded `runApply`
    // below blocks the apply on validation failure, so this only runs
    // for a valid set of rows; the `{}` fallback is unreachable in
    // practice but keeps the type non-null.
    toPayload: (v) => {
      const res = buildBucketPayload(v);
      return res.ok ? res.body : { buckets: {} };
    },
  });

  // Config (backends + default backend) comes from the shared cache.
  const { data: cfg } = useAdminConfig();
  // /api/admin/backends is the fallback when the config response doesn't
  // carry a `backends` array (legacy shapes); available buckets come from
  // the S3 list endpoint. Neither is part of the storage section body, so
  // they're side-loaded independently of the section editor.
  const [fallbackBackends, setFallbackBackends] = useState<BackendInfo[]>([]);
  const [availableBuckets, setAvailableBuckets] = useState<string[]>([]);

  // getAdminConfig resolves to `null` on a 401; surface as session-expiry.
  useEffect(() => {
    if (cfg === null) onSessionExpired?.();
  }, [cfg, onSessionExpired]);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [bs, realBuckets] = await Promise.all([
          getBackends().then((r) => r.backends).catch(() => [] as BackendInfo[]),
          listBuckets().catch(() => [] as Array<{ name: string }>),
        ]);
        if (cancelled) return;
        setFallbackBackends(bs);
        setAvailableBuckets(realBuckets.map((b) => b.name));
      } catch (e) {
        if (cancelled) return;
        if (e instanceof Error && e.message.includes('401')) onSessionExpired?.();
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [onSessionExpired]);

  // Prefer the /api/admin/config response's `backends` array — it
  // synthesises a "default" entry on the singleton-backend path, so the
  // per-bucket encryption badge works uniformly regardless of YAML shape.
  // Fall back to /api/admin/backends when the primary endpoint doesn't
  // carry backends (legacy response shapes).
  const backends =
    cfg?.backends && cfg.backends.length > 0 ? cfg.backends : fallbackBackends;
  const defaultBackend = cfg?.default_backend ?? null;

  // Guarded apply: run the client-side duplicate-name check first and
  // surface the error, otherwise delegate to the section editor's
  // validate → ApplyDialog → PUT flow (which re-derives the same body
  // via `toPayload`, keeping the validate/PUT body byte-identical).
  const runApply = useCallback(async () => {
    const res = buildBucketPayload(rows);
    if (!res.ok) {
      message.error(res.error);
      return;
    }
    await editorRunApply();
  }, [rows, editorRunApply]);

  // ⌘S routes through the guard (registered after the hook's own
  // handler, so this most-recently-mounted one wins the dispatch).
  useApplyHandler('configuration/storage/buckets', runApply, dirty);

  const updateRow = (id: string, patch: Partial<BucketPolicyRow>) => {
    setRows((cur) => cur.map((r) => (r._id === id ? { ...r, ...patch } : r)));
  };

  const addRow = () => {
    setRows((cur) => [
      ...cur,
      {
        _id: freshId(),
        name: '',
        compression: null,
        max_delta_ratio: null,
        backend: '',
        alias: '',
        publicMode: 'none',
        public_prefixes: [],
        quota_bytes: null,
      },
    ]);
  };

  const deleteRow = (id: string, name: string) => {
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
        setRows((cur) => cur.filter((r) => r._id !== id));
      },
    });
  };

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
          {rows.map((row) => (
            <BucketCard
              key={row._id}
              row={row}
              backends={backends}
              defaultBackend={defaultBackend}
              availableBuckets={availableBuckets.filter(
                (b) => !rows.some((r) => r._id !== row._id && r.name === b)
              )}
              onChange={(patch) => updateRow(row._id, patch)}
              onPrefixesChange={(fn) =>
                setRows((cur) =>
                  cur.map((r) =>
                    r._id === row._id
                      ? { ...r, public_prefixes: fn(r.public_prefixes) }
                      : r
                  )
                )
              }
              onDelete={() => deleteRow(row._id, row.name)}
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
