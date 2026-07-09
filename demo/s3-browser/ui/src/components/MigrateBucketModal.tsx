/**
 * MigrateBucketModal — move a bucket's objects to a different backend as
 * a DURABLE BACKGROUND JOB (write-gated, resumable; the old synchronous
 * endpoint is gone). On 202 the modal closes and points the operator at
 * the Jobs page; the bucket row meanwhile shows the busy chip + progress.
 *
 * Two entry points: pre-targeted from a bucket card (`bucket` set), or
 * the Jobs page's "New job → Migrate bucket…" (`bucket` null → internal
 * bucket picker).
 */
import { useState } from 'react';
import { Modal, Select, Checkbox, Typography, Alert, message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import { createMigrateJob } from '../adminApi';
import { qk } from '../queries/keys';
import { useBackends, useBucketOrigins } from '../queries/backends';
import { normalizeUiError } from '../errorHandling';

const { Text } = Typography;

interface Props {
  open: boolean;
  /** Pre-targeted bucket, or null for the internal picker (Jobs page). */
  bucket: string | null;
  onClose: () => void;
  onStarted?: () => void;
}

export default function MigrateBucketModal({ open, bucket, onClose, onStarted }: Props) {
  const qc = useQueryClient();
  const [pickedBucket, setPickedBucket] = useState<string | undefined>(undefined);
  const [target, setTarget] = useState<string | undefined>(undefined);
  const [deleteSource, setDeleteSource] = useState(false);
  const [starting, setStarting] = useState(false);
  const [messageApi, contextHolder] = message.useMessage();

  const backendsQuery = useBackends();
  const originsQuery = useBucketOrigins({ enabled: open });
  const backends = backendsQuery.data?.backends ?? [];
  const defaultBackend = backendsQuery.data?.default_backend ?? null;
  const origins = originsQuery.data?.buckets ?? [];

  const effectiveBucket = bucket ?? pickedBucket;
  const currentBackend =
    origins.find((o) => o.name === effectiveBucket)?.backend_name ?? defaultBackend;
  const targets = backends.filter((b) => b.name !== currentBackend);

  const reset = () => {
    setPickedBucket(undefined);
    setTarget(undefined);
    setDeleteSource(false);
  };

  const handleStart = async () => {
    if (!effectiveBucket || !target) return;
    setStarting(true);
    try {
      await createMigrateJob(effectiveBucket, target, deleteSource);
      messageApi.success(
        `Migration of ${effectiveBucket} → ${target} started — track progress on the Jobs page`
      );
      qc.invalidateQueries({ queryKey: qk.jobs.list() });
      qc.invalidateQueries({ queryKey: qk.backends.origins() });
      reset();
      onClose();
      onStarted?.();
    } catch (e) {
      messageApi.error(normalizeUiError(e, 'Failed to start migration'));
    } finally {
      setStarting(false);
    }
  };

  return (
    <Modal
      open={open}
      onCancel={() => {
        reset();
        onClose();
      }}
      onOk={() => void handleStart()}
      okText="Start migration"
      okButtonProps={{ disabled: !effectiveBucket || !target, loading: starting }}
      title={effectiveBucket ? `Migrate ${effectiveBucket} to another backend` : 'Migrate a bucket'}
    >
      {contextHolder}
      <Text type="secondary" style={{ fontSize: 13, display: 'block', marginBottom: 12 }}>
        Runs as a background job: every object is copied to the target backend, verified,
        and only then is the bucket re-routed. The job survives restarts and resumes
        where it left off.
      </Text>
      <Alert
        type="warning"
        showIcon
        style={{ marginBottom: 12, borderRadius: 8 }}
        message="While the migration runs"
        description={
          <span style={{ fontSize: 12 }}>
            Reads keep working; <strong>uploads and deletes get a temporary 503</strong> until
            the switch-over (S3 clients retry automatically).
          </span>
        }
      />
      {bucket === null && (
        <div style={{ marginBottom: 12 }}>
          <Text strong style={{ fontSize: 13, display: 'block', marginBottom: 6 }}>
            Bucket
          </Text>
          <Select
            style={{ width: '100%' }}
            placeholder="Pick a bucket"
            value={pickedBucket}
            onChange={setPickedBucket}
            options={origins.map((o) => ({
              value: o.name,
              label: `${o.name} (on ${o.backend_name ?? defaultBackend ?? 'default'})`,
            }))}
          />
        </div>
      )}
      <div style={{ marginBottom: 12 }}>
        <Text strong style={{ fontSize: 13, display: 'block', marginBottom: 6 }}>
          Target backend
        </Text>
        <Select
          style={{ width: '100%' }}
          placeholder={
            targets.length === 0 ? 'No other backend available' : 'Pick the destination backend'
          }
          value={target}
          onChange={setTarget}
          disabled={targets.length === 0}
          options={targets.map((b) => ({
            value: b.name,
            label: `${b.name} (${b.backend_type})`,
          }))}
        />
      </div>
      <Checkbox checked={deleteSource} onChange={(e) => setDeleteSource(e.target.checked)}>
        <Text style={{ fontSize: 13 }}>
          Delete source objects after the switch-over{' '}
          <Text type="secondary">(default keeps them as a safety copy)</Text>
        </Text>
      </Checkbox>
    </Modal>
  );
}
