/**
 * Start the one-off `backfill-metadata` job (Jobs → New job → Backfill
 * metadata…). The job stamps DeltaGlider's metadata (content hashes, sizes,
 * created-at) onto objects that were written to the backend without the
 * proxy, reading each object once to hash it and rewriting only its
 * metadata. POST /_/api/admin/jobs/backfill-metadata.
 */
import { useEffect, useState } from 'react';
import { Alert, Button, Checkbox, Modal, Typography, message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import { TagsOutlined } from '@ant-design/icons';
import { startBackfillMetadata } from '../adminApi';
import { qk } from '../queries/keys';
import { useColors } from '../ThemeContext';
import { useBucketOrigins } from '../queries/backends';
import { normalizeUiError } from '../errorHandling';

const { Text } = Typography;

interface Props {
  open: boolean;
  onClose: () => void;
}

export default function BackfillMetadataModal({ open, onClose }: Props) {
  const colors = useColors();
  const qc = useQueryClient();
  const originsQuery = useBucketOrigins({ enabled: open });
  const candidates = (originsQuery.data?.buckets ?? []).map((b) => b.name);
  const [selected, setSelected] = useState<string[]>([]);
  const [refreshLastModified, setRefreshLastModified] = useState(false);
  const [starting, setStarting] = useState(false);
  const [messageApi, msgCtx] = message.useMessage();

  // Each opening starts with nothing selected: an explicit opt-in per bucket.
  useEffect(() => {
    if (open) {
      setSelected([]);
      setRefreshLastModified(false);
    }
  }, [open]);

  const handleStart = async () => {
    if (selected.length === 0) return;
    setStarting(true);
    try {
      const res = await startBackfillMetadata(selected, refreshLastModified);
      if (res.started.length > 0) {
        messageApi.success(`Started for ${res.started.map((s) => s.bucket).join(', ')}`);
      }
      for (const e of res.errors) messageApi.error(`${e.bucket}: ${e.error}`);
      qc.invalidateQueries({ queryKey: qk.jobs.list() });
      if (res.errors.length === 0) onClose();
    } catch (e) {
      messageApi.error(normalizeUiError(e, 'Failed to start'));
    } finally {
      setStarting(false);
    }
  };

  return (
    <Modal
      open={open}
      onCancel={onClose}
      title={
        <span>
          <TagsOutlined style={{ marginRight: 8, color: colors.ACCENT_BLUE }} />
          Backfill object metadata
        </span>
      }
      footer={[
        <Button key="cancel" onClick={onClose}>
          Cancel
        </Button>,
        <Button key="start" type="primary" loading={starting} disabled={selected.length === 0} onClick={handleStart}>
          Start now ({selected.length} bucket{selected.length === 1 ? '' : 's'})
        </Button>,
      ]}
    >
      {msgCtx}
      <Text type="secondary" style={{ fontSize: 13, display: 'block', marginBottom: 12 }}>
        Objects that were written to the backend without the proxy (before it was installed, or by
        another tool) carry none of DeltaGlider&apos;s metadata, so the proxy cannot show their
        checksum or verify them. This one-off job reads each such object once to compute its
        hashes, then writes only the metadata. The object bytes are not uploaded again, and
        objects the proxy wrote are skipped.
      </Text>
      <Alert
        type="warning"
        showIcon
        style={{ marginBottom: 12, borderRadius: 8 }}
        title="While a bucket is being processed"
        description={
          <span style={{ fontSize: 12 }}>
            Reads keep working; <strong>uploads and deletes get a temporary 503</strong> (S3 clients
            retry automatically).
          </span>
        }
      />
      <Checkbox
        checked={refreshLastModified}
        onChange={(e) => setRefreshLastModified(e.target.checked)}
        style={{ marginBottom: 12 }}
      >
        <Text style={{ fontSize: 13 }}>
          Show backfilled objects as modified now. Leave this off to keep each object&apos;s
          Last-Modified time, so that sync tools and replication do not copy them again.
        </Text>
      </Checkbox>
      {candidates.length === 0 ? (
        <Text type="secondary">No buckets available.</Text>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
          {candidates.map((b) => (
            <Checkbox
              key={b}
              aria-label={b}
              checked={selected.includes(b)}
              onChange={(e) => setSelected((cur) => (e.target.checked ? [...cur, b] : cur.filter((x) => x !== b)))}
            >
              <Text code style={{ fontSize: 13 }}>
                {b}
              </Text>
            </Checkbox>
          ))}
        </div>
      )}
    </Modal>
  );
}
