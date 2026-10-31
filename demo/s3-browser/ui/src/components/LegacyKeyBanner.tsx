/**
 * LegacyKeyBanner — the decrypt-only legacy key of one encryption site:
 * a backend (Backends page) or one bucket's own block (bucket card, issue
 * #116). "Check usage" runs the server's scan of the objects and delta
 * references that still carry the legacy key id; "Clear legacy key" is
 * enabled only when the scan answers `safe_to_clear`.
 *
 * With `bucket`, the scan asks for that bucket's own legacy key id
 * (`legacy-key-usage?bucket=`); the server still reads every bucket on the
 * backend, because a key id can sit in a neighbour.
 */
import { useState } from 'react';
import { Alert, Button, Space } from 'antd';
import type { LegacyKeyUsage } from '../adminApi';
import { LEGACY_KEY_SCAN_LIMIT } from '../adminApi';
import { useLegacyKeyUsage } from '../queries/backends';
import { confirmDialog } from '../confirmDialog';
import { plainError } from '../plainError';

const plural = (n: number, one: string, many: string) => `${n.toLocaleString('en-US')} ${n === 1 ? one : many}`;

/** One sentence on what the legacy-key scan found. */
function usageSummary(u: LegacyKeyUsage): string {
  const kid = u.legacy_key_id ?? '(unknown)';
  const where = plural(u.buckets.length, 'bucket', 'buckets');
  if (!u.complete) {
    return `The check stopped at its limit of ${u.limit.toLocaleString('en-US')} objects, so it cannot prove that no object uses the legacy key id ${kid}. Run a re-encrypt job on every bucket of this backend. Then check again through the admin API with a higher limit (GET /_/api/admin/backends/<name>/legacy-key-usage?limit=1000000).`;
  }
  const objects = u.objects_under_legacy_key;
  const refs = u.references_under_legacy_key;
  if (objects > 0 || refs > 0) {
    return `${plural(objects, 'object', 'objects')} and ${plural(refs, 'delta reference', 'delta references')} still use the legacy key id ${kid} (checked ${where}). Run a re-encrypt job on these buckets first.`;
  }
  if (u.errors.length > 0) {
    return `The check could not read everything, so it cannot prove that no object uses the legacy key id ${kid}.`;
  }
  return `No object uses the legacy key id ${kid}. The check read ${plural(u.objects_scanned, 'object', 'objects')} and ${plural(u.references_scanned, 'delta reference', 'delta references')} in ${where}.`;
}

export default function LegacyKeyBanner({
  backendName,
  bucket,
  readOnly,
  onClearLegacy,
}: {
  backendName: string;
  /** Scope: one bucket's own legacy key. */
  bucket?: string;
  readOnly?: boolean;
  onClearLegacy?: () => Promise<void>;
}) {
  // The check HEADs every object of the backend: it runs only on a click.
  const usage = useLegacyKeyUsage(backendName, bucket);
  const owner = bucket ? <>bucket <code>{bucket}</code></> : <>backend <code>{backendName}</code></>;
  const [clearing, setClearing] = useState(false);
  const u = usage.data;

  const clear = async () => {
    if (!onClearLegacy || !u) return;
    const ok = await confirmDialog({
      title: 'Clear the legacy key?',
      content: (
        <span>
          The proxy removes <code>legacy_key</code> and <code>legacy_key_id</code> from {owner}. An object encrypted under the legacy key id{' '}
          <code>{u.legacy_key_id}</code> cannot be read any more after this. The check found no such
          object, but keep a copy of the old key until you are sure, for example until the backups made
          before the rotation expire.
        </span>
      ),
      okText: 'Clear',
      danger: true,
    });
    if (!ok) return;
    setClearing(true);
    try {
      await onClearLegacy();
    } finally {
      setClearing(false);
    }
  };

  return (
    <Alert
      type="info"
      showIcon
      style={{ marginTop: 8, borderRadius: 6, fontSize: 12 }}
      title="Decrypt-only shim active"
      description={
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
          <span>
            A legacy key is configured on {bucket ? 'this bucket' : 'this backend'}. Objects written
            under that key still decrypt; new writes use the current mode. Clear the legacy key once
            no object uses it.
          </span>
          {!u && !usage.isFetching && !usage.error && (
            <span>
              Check usage reads the metadata of up to{' '}
              {LEGACY_KEY_SCAN_LIMIT.toLocaleString('en-US')} objects on this backend. On a local
              backend, 20,000 objects take about a second; a remote backend takes longer.
            </span>
          )}
          {usage.isFetching && <span>Checking which objects still use the legacy key…</span>}
          {usage.error && <span>{plainError(usage.error, 'Could not check for objects under the old key').text}</span>}
          {u && <span>{usageSummary(u)}</span>}
          {u && u.examples.length > 0 && (
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, overflowWrap: 'anywhere' }}>
              For example: {u.examples.join(', ')}
            </span>
          )}
          {u && u.errors.length > 0 && (
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, overflowWrap: 'anywhere' }}>
              Not readable: {u.errors.join('; ')}
            </span>
          )}
          <Space wrap>
            <Button size="small" onClick={() => void usage.refetch()} loading={usage.isFetching}>
              {u || usage.error ? 'Check again' : 'Check usage'}
            </Button>
            {onClearLegacy && !readOnly && (
              <Button
                size="small"
                danger
                disabled={!u?.safe_to_clear || usage.isFetching}
                loading={clearing}
                onClick={() => void clear()}
              >
                Clear legacy key
              </Button>
            )}
          </Space>
        </div>
      }
    />
  );
}
