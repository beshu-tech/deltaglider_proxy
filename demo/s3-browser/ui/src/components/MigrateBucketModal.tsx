/**
 * MigrateBucketModal — move a bucket's objects to a different backend as
 * a DURABLE BACKGROUND JOB (write-gated, resumable; the old synchronous
 * endpoint is gone). On 202 the modal closes and points the operator at
 * the Jobs page; the bucket row meanwhile shows the busy chip + progress.
 *
 * The target picker shows each backend's health and encryption; a backend
 * that the health loop gates cannot be picked. A move after which the
 * bucket's new writes are plaintext needs an explicit acknowledgement. The
 * source side is the server's `effective_encryption` of the bucket: a
 * bucket's own encryption block moves with the bucket (issue #116), so only
 * an inherited encryption can be lost by the move. A start
 * that the server refuses (409: gated target, non-empty destination) shows
 * its reason inside the dialog.
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
import { ApiError } from '../errorHandling';
import { plainError } from '../plainError';
import {
  MIRROR_OPTION_LABEL,
  encryptionWord,
  healthWord,
  isGated,
  migrateErrorInModalWords,
} from '../backendFacts';
import { describeBucketEncryption, encryptsAfterMigrate } from '../bucketEncryption';

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
  const [mirror, setMirror] = useState(false);
  const [starting, setStarting] = useState(false);
  const [plaintextAck, setPlaintextAck] = useState(false);
  /** Why the server refused the last start, in the dialog's words. */
  const [startError, setStartError] = useState<string | null>(null);
  const [messageApi, contextHolder] = message.useMessage();

  const backendsQuery = useBackends();
  const originsQuery = useBucketOrigins({ enabled: open });
  const backends = backendsQuery.data?.backends ?? [];
  const origins = originsQuery.data?.buckets ?? [];

  const effectiveBucket = bucket ?? pickedBucket;
  // Unknown until the origins load: the default backend is not a safe guess
  // for a bucket that lives on another backend.
  const origin = origins.find((o) => o.name === effectiveBucket);
  const currentBackend = originsQuery.isSuccess ? (origin?.backend_name ?? null) : null;
  const source = backends.find((b) => b.name === currentBackend);
  const sourceEnc = origin?.effective_encryption ?? null;
  const targets = originsQuery.isSuccess ? backends.filter((b) => b.name !== currentBackend) : [];
  const targetInfo = backends.find((b) => b.name === target);
  const targetGated = isGated(targetInfo?.health);
  const dropsEncryption =
    !!targetInfo && !!sourceEnc?.encrypts && !encryptsAfterMigrate(sourceEnc, targetInfo.encryption);
  const canStart =
    !!effectiveBucket && !!target && !targetGated && (!dropsEncryption || plaintextAck);

  const reset = () => {
    setPickedBucket(undefined);
    setTarget(undefined);
    setDeleteSource(false);
    setMirror(false);
    setPlaintextAck(false);
    setStartError(null);
  };

  const handleStart = async () => {
    if (!canStart || !effectiveBucket || !target) return;
    setStarting(true);
    setStartError(null);
    try {
      await createMigrateJob(
        effectiveBucket,
        target,
        deleteSource,
        mirror ? 'mirror' : 'empty',
        // The acknowledged move to plaintext carries the server's confirmation.
        dropsEncryption ? { downgrades: [effectiveBucket] } : undefined,
      );
      messageApi.success(
        `Migration of ${effectiveBucket} → ${target} started — track progress on the Jobs page`
      );
      qc.invalidateQueries({ queryKey: qk.jobs.list() });
      qc.invalidateQueries({ queryKey: qk.backends.origins() });
      reset();
      onClose();
      onStarted?.();
    } catch (e) {
      // A refusal (4xx) carries the server's reason: show it in the dialog,
      // next to the options that change it.
      if (e instanceof ApiError && e.status >= 400 && e.status < 500 && e.detail) {
        setStartError(migrateErrorInModalWords(e.detail));
      } else {
        messageApi.error(plainError(e, 'Could not start the migration').text);
      }
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
      okButtonProps={{ disabled: !canStart, loading: starting }}
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
        title="While the migration runs"
        description={
          <span style={{ fontSize: 12 }}>
            Reads keep working; <strong>uploads and deletes get a temporary 503</strong> until
            the switch-over (S3 clients retry automatically).
          </span>
        }
      />
      {bucket === null && (
        <div style={{ marginBottom: 12 }}>
          <Text id="migrate-bucket-label" strong style={{ fontSize: 13, display: 'block', marginBottom: 4 }}>
            Bucket
          </Text>
          <Select
            aria-labelledby="migrate-bucket-label"
            style={{ width: '100%' }}
            placeholder="Pick a bucket"
            value={pickedBucket}
            onChange={(v) => {
              setPickedBucket(v);
              setTarget(undefined);
              setPlaintextAck(false);
              setStartError(null);
            }}
            options={origins.map((o) => ({
              value: o.name,
              label: `${o.name} (on ${o.backend_name ?? 'default'})`,
            }))}
          />
        </div>
      )}
      <div style={{ marginBottom: 12 }}>
        <Text id="migrate-target-label" strong style={{ fontSize: 13, display: 'block', marginBottom: 4 }}>
          Target backend
        </Text>
        {source && (
          <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: 4 }}>
            Now on <Text code>{source.name}</Text> ({describeBucketEncryption(sourceEnc).label}).
            {sourceEnc?.source === 'bucket' ? ' The bucket\'s own encryption moves with it.' : ''}
          </Text>
        )}
        <Select
          aria-labelledby="migrate-target-label"
          style={{ width: '100%' }}
          placeholder={
            targets.length === 0 ? 'No other backend available' : 'Pick the destination backend'
          }
          value={target}
          onChange={(v) => {
            setTarget(v);
            setPlaintextAck(false);
            setStartError(null);
          }}
          disabled={targets.length === 0}
          options={targets.map((b) => ({
            value: b.name,
            // A gated backend fails every copy: it cannot be picked.
            disabled: isGated(b.health),
            label: `${b.name} (${b.backend_type}) · ${encryptionWord(b.encryption)} · ${
              isGated(b.health) ? `unavailable: ${healthWord(b.health)}` : healthWord(b.health)
            }`,
          }))}
        />
      </div>
      {targetInfo && targetGated && (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 12, borderRadius: 8 }}
          title={`Backend ${targetInfo.name} is unavailable: ${healthWord(targetInfo.health)}`}
          description={
            <span style={{ fontSize: 12 }}>
              Every copy to it would fail. Fix the backend first (Storage → Backends → Test
              connection).
            </span>
          }
        />
      )}
      {targetInfo && dropsEncryption && (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 12, borderRadius: 8 }}
          title={`Objects will be stored unencrypted at rest on ${targetInfo.name}`}
          description={
            <div style={{ fontSize: 12 }}>
              <div style={{ marginBottom: 4 }}>
                The bucket is encrypted at rest now ({describeBucketEncryption(sourceEnc).label}).
                It takes its encryption from its backend, and the target backend{' '}
                <Text code>{targetInfo.name}</Text> does not encrypt, so every copied object is
                written as plaintext.
              </div>
              <Checkbox checked={plaintextAck} onChange={(e) => setPlaintextAck(e.target.checked)}>
                <span style={{ fontSize: 12 }}>
                  I understand that the objects are stored unencrypted on {targetInfo.name}
                </span>
              </Checkbox>
            </div>
          }
        />
      )}
      <Checkbox checked={deleteSource} onChange={(e) => setDeleteSource(e.target.checked)}>
        <Text style={{ fontSize: 13 }}>
          Delete source objects after the switch-over{' '}
          <Text type="secondary">(default keeps them as a safety copy)</Text>
        </Text>
      </Checkbox>
      <Text type="secondary" style={{ fontSize: 12, display: 'block', margin: '8px 0 4px 24px' }}>
        If the destination bucket already holds objects, the job refuses to start: an old safety
        copy there would bring back objects that were deleted since.
      </Text>
      <Checkbox
        checked={mirror}
        onChange={(e) => {
          setMirror(e.target.checked);
          setStartError(null);
        }}
      >
        <Text style={{ fontSize: 13 }}>{MIRROR_OPTION_LABEL}</Text>
      </Checkbox>
      {mirror && (
        <Alert
          type="error"
          showIcon
          style={{ marginTop: 8, borderRadius: 8 }}
          title="Destination objects can be deleted"
          description={
            <span style={{ fontSize: 12 }}>
              Objects in the destination bucket that the source does not hold{' '}
              <strong>are deleted before the switch-over</strong>. Each delete is written to the
              audit log.
            </span>
          }
        />
      )}
      {startError && (
        <Alert
          type="error"
          showIcon
          style={{ marginTop: 12, borderRadius: 8 }}
          title="The migration did not start"
          description={<span style={{ fontSize: 12 }}>{startError}</span>}
        />
      )}
    </Modal>
  );
}
