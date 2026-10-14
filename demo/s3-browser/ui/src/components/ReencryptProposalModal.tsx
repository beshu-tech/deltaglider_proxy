/**
 * Post-encryption-change proposal: offer the one-off "re-encrypt existing
 * objects" maintenance job for the buckets whose encryption changed.
 *
 * Shown by BackendsPanel right after a successful encryption apply
 * (enable / rotate / disable). The job itself is uniform — "rewrite every
 * object whose at-rest state doesn't match the new config" — so the same
 * modal serves all three transitions; only the copy changes.
 *
 * Also reusable as the manual "[Later]" path from the Buckets page
 * (single-bucket preset).
 *
 * Jobs-page mode (`pickBuckets`) ignores `transition`: the job has nothing
 * to do with a key change there, so the copy says what the job does to each
 * object. It lists only buckets whose backend a re-encrypt can change
 * (the server's `effective_encryption.reencrypt`), each with its backend
 * and encryption, behind a search box. Every per-bucket fact comes from
 * the server's `effective_encryption`: a bucket with its own encryption
 * block is listed by its own setting, not by its backend's.
 */
import { useEffect, useMemo, useState } from 'react';
import { Alert, Button, Checkbox, Input, Modal, Typography, message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import { SyncOutlined } from '@ant-design/icons';
import { startReencrypt } from '../adminApi';
import { qk } from '../queries/keys';
import { useColors } from '../ThemeContext';
import { useBucketOrigins } from '../queries/backends';
import { normalizeUiError } from '../errorHandling';
import { describeBucketEncryption, type ReencryptTransition } from '../bucketEncryption';

const { Text } = Typography;


interface Props {
  open: boolean;
  transition: ReencryptTransition;
  /** Buckets eligible for the job (the buckets whose encryption changed). */
  buckets: string[];
  /** Context: the backend whose change this proposal follows, if any. */
  backendName?: string;
  /** Jobs-page mode: ignore the preset list and pick from ALL buckets. */
  pickBuckets?: boolean;
  onClose: () => void;
}

const TITLES: Record<ReencryptTransition, string> = {
  encrypt: 'Encrypt existing objects?',
  rotate: 'Re-encrypt existing objects with the new key?',
  decrypt: 'Decrypt existing objects?',
};

const PICK_TITLE = 'Re-encrypt existing objects';
const PICK_EXPLAIN =
  "A one-off background job rewrites each object that is not stored under its bucket's current setting (the bucket's own encryption, or its backend's). A plaintext object gets encrypted, and an object under an older key gets re-encrypted with the current key. In a bucket whose encryption is off but that can still read an older key, an encrypted object gets decrypted. Objects that already match are skipped, and the job resumes automatically if the proxy restarts.";

interface PickRow {
  bucket: string;
  backend: string | null;
  encryptionLabel: string;
}

const EXPLAIN: Record<ReencryptTransition, string> = {
  encrypt:
    'Encryption applies to NEW writes only. Objects already stored in these buckets are still plaintext on disk until rewritten.',
  rotate:
    'The new key applies to NEW writes only. Existing objects remain encrypted under the previous key until rewritten.',
  decrypt:
    'Disabling encryption affects NEW writes only. Existing objects remain encrypted on disk until rewritten (readable only while the legacy key shim is configured).',
};

export default function ReencryptProposalModal({
  open,
  transition,
  buckets,
  backendName,
  pickBuckets = false,
  onClose,
}: Props) {
  const colors = useColors();
  const qc = useQueryClient();
  const originsQuery = useBucketOrigins({ enabled: open && pickBuckets });
  const [search, setSearch] = useState('');
  const { pickRows, skippedPlaintext } = useMemo(() => {
    const rows: PickRow[] = [];
    let skipped = 0;
    for (const o of originsQuery.data?.buckets ?? []) {
      const eff = o.effective_encryption;
      if (!eff?.reencrypt) {
        skipped += 1;
        continue;
      }
      const enc = describeBucketEncryption(eff).label;
      rows.push({
        bucket: o.name,
        backend: eff.backend,
        encryptionLabel: eff.legacy_key_id ? `${enc}, legacy key ${eff.legacy_key_id}` : enc,
      });
    }
    return { pickRows: rows, skippedPlaintext: skipped };
  }, [originsQuery.data]);
  const needle = search.trim().toLowerCase();
  const visibleRows = needle
    ? pickRows.filter(
        (r) => r.bucket.toLowerCase().includes(needle) || (r.backend ?? '').toLowerCase().includes(needle)
      )
    : pickRows;
  const candidates = pickBuckets ? pickRows.map((r) => r.bucket) : buckets;
  const [selected, setSelected] = useState<string[]>(buckets);
  const [starting, setStarting] = useState(false);
  const [messageApi, msgCtx] = message.useMessage();

  useEffect(() => {
    // Jobs-page mode starts with NOTHING selected (explicit opt-in);
    // the post-apply proposal preselects the affected backend's buckets.
    if (open) {
      setSelected(pickBuckets ? [] : buckets);
      setSearch('');
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, pickBuckets, candidates.join('|')]);

  const handleStart = async () => {
    if (selected.length === 0) return;
    setStarting(true);
    try {
      const res = await startReencrypt(selected);
      if (res.started.length > 0) {
        messageApi.success(
          `Started for ${res.started.map((s) => s.bucket).join(', ')} — track progress on the Jobs page`
        );
      }
      for (const e of res.errors) {
        messageApi.error(`${e.bucket}: ${e.error}`);
      }
      qc.invalidateQueries({ queryKey: qk.jobs.list() });
      // Only the buckets that failed stay selected: a second Start must not
      // send the started ones again.
      const failed = new Set(res.errors.map((e) => e.bucket));
      setSelected((cur) => cur.filter((b) => failed.has(b)));
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
          <SyncOutlined style={{ marginRight: 8, color: colors.ACCENT_BLUE }} />
          {pickBuckets ? PICK_TITLE : TITLES[transition]}
        </span>
      }
      footer={[
        <Button key="later" onClick={onClose}>
          Later
        </Button>,
        <Button
          key="start"
          type="primary"
          loading={starting}
          disabled={selected.length === 0}
          onClick={handleStart}
        >
          Start now ({selected.length} bucket{selected.length === 1 ? '' : 's'})
        </Button>,
      ]}
    >
      {msgCtx}
      {pickBuckets ? (
        <Text type="secondary" style={{ fontSize: 13, display: 'block', marginBottom: 12 }}>
          {PICK_EXPLAIN}
        </Text>
      ) : (
        <Text type="secondary" style={{ fontSize: 13, display: 'block', marginBottom: 12 }}>
          {EXPLAIN[transition]} A one-off background job rewrites every object whose at-rest
          state doesn't match its bucket's current setting
          {backendName ? <> (after the change on <Text code>{backendName}</Text>)</> : null}.
          Already-matching objects are skipped, and the job resumes automatically if the proxy
          restarts.
        </Text>
      )}
      <Alert
        type="warning"
        showIcon
        style={{ marginBottom: 12, borderRadius: 8 }}
        title="While a bucket is being processed"
        description={
          <span style={{ fontSize: 12 }}>
            Reads keep working; <strong>uploads and deletes get a temporary 503</strong> (S3
            clients retry automatically). Rewritten objects keep their Last-Modified time and
            their content, so sync tools do not download them again.
          </span>
        }
      />
      {pickBuckets ? (
        <>
          <Input.Search
            allowClear
            aria-label="Search buckets"
            placeholder="Search buckets or backends"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            style={{ marginBottom: 8 }}
          />
          {skippedPlaintext > 0 && (
            <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: 8 }}>
              {skippedPlaintext} bucket{skippedPlaintext === 1 ? ' is' : 's are'} not listed: the
              job has nothing to change there (plaintext with no older key, or a native mode).
            </Text>
          )}
          {visibleRows.length === 0 ? (
            <Text type="secondary">
              {pickRows.length === 0 ? 'No bucket has objects that the job can change.' : 'No bucket matches the search.'}
            </Text>
          ) : (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 6, maxHeight: 320, overflowY: 'auto' }}>
              {visibleRows.map((r) => (
                <Checkbox
                  key={r.bucket}
                  aria-label={r.bucket}
                  checked={selected.includes(r.bucket)}
                  onChange={(e) =>
                    setSelected((cur) =>
                      e.target.checked ? [...cur, r.bucket] : cur.filter((x) => x !== r.bucket)
                    )
                  }
                >
                  <Text code style={{ fontSize: 13 }}>
                    {r.bucket}
                  </Text>{' '}
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    on {r.backend ?? 'default'} · {r.encryptionLabel}
                  </Text>
                </Checkbox>
              ))}
            </div>
          )}
        </>
      ) : candidates.length === 0 ? (
        <Text type="secondary">No buckets available.</Text>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
          {candidates.map((b) => (
            <Checkbox
              key={b}
              checked={selected.includes(b)}
              onChange={(e) =>
                setSelected((cur) =>
                  e.target.checked ? [...cur, b] : cur.filter((x) => x !== b)
                )
              }
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
