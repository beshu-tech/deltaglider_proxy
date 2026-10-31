import { Table, Typography } from 'antd';
import type { LifecyclePreview } from '../../adminApi';
import { formatBytes } from '../../utils';
import { approxTimeUntil } from '../../jobsView';
import { ErrorState, LoadingState } from '../StatePlaceholders';
import { lifecycleActionCounts, lifecycleActionLabel, lifecycleSkipText, previewAgeText } from './lifecycleText';

const { Text } = Typography;

interface Props {
  preview: LifecyclePreview | undefined;
  loading: boolean;
  error: unknown;
  /** Computes the preview again (the error state's Retry). */
  onRetry?: () => void;
  /** When the GUI received the preview (ms since the epoch): shown only when the proxy
   *  did not say when its walk ended (`computed_at`). */
  receivedAt?: number;
  /** Now (ms since the epoch), to say how old the walk is. */
  nowMs?: number;
}

/** "on 11/12/2026, 11:00:00 AM (in ~13 h)" */
function dueText(nextDueAt: string): string {
  const at = Date.parse(nextDueAt);
  const until = approxTimeUntil(at, Date.now());
  return `on ${new Date(at).toLocaleString()} (${until === 'now' ? 'now' : `in ${until}`})`;
}

/**
 * The objects a lifecycle rule would act on: totals first (count and bytes),
 * then the candidate keys in a paged table. The server caps the candidate
 * list, so the table says when it shows fewer keys than the total. When
 * nothing matches, it says when the first object becomes due instead.
 */
export default function LifecyclePreviewList({ preview, loading, error, onRetry, receivedAt, nowMs }: Props) {
  if (error) return <ErrorState compact error={error} what="the preview" onRetry={onRetry} />;
  if (loading || !preview) return <LoadingState variant="inline" label="Computing the preview…" />;
  const shown = preview.candidates.length;
  const scannedAt = preview.computed_at ? Date.parse(preview.computed_at) : receivedAt;
  const secondary = { display: 'block', marginBottom: 8, fontSize: 12 } as const;
  const scanned = scannedAt
    ? ` at ${new Date(scannedAt).toLocaleTimeString()}${nowMs ? ` (${previewAgeText(scannedAt, nowMs)})` : ''}`
    : '';
  const skip = lifecycleSkipText(preview);
  if (preview.objects_affected === 0) {
    return (
      <div>
        <Text style={{ display: 'block', marginBottom: 8 }}>
          {preview.next_due_at
            ? `Nothing to do now. The first object becomes due ${dueText(preview.next_due_at)}.`
            : 'Nothing to do now: no scanned object matches the rule.'}
        </Text>
        <Text type="secondary" style={secondary}>
          Scanned {preview.objects_scanned.toLocaleString()} objects{scanned}.
        </Text>
        {skip && <Text type="secondary" style={secondary}>{skip}</Text>}
      </div>
    );
  }
  return (
    <div>
      <Text style={{ display: 'block', marginBottom: 8 }}>
        <strong>{preview.objects_affected.toLocaleString()}</strong> of {preview.objects_scanned.toLocaleString()} scanned
        objects would be affected
        {lifecycleActionCounts(preview) ? ` (${lifecycleActionCounts(preview)})` : ''},{' '}
        <strong>{formatBytes(preview.bytes_affected)}</strong> in total
        {preview.stored_bytes_affected != null && preview.stored_bytes_affected !== preview.bytes_affected
          ? ` (${formatBytes(preview.stored_bytes_affected)} on the backend)`
          : ''}
        .
      </Text>
      {skip && <Text type="secondary" style={secondary}>{skip}</Text>}
      {scannedAt ? (
        <Text type="secondary" style={secondary}>
          Scanned {preview.objects_scanned.toLocaleString()} objects{scanned}.
        </Text>
      ) : null}
      {preview.next_due_at ? (
        <Text type="secondary" style={secondary}>
          The next object becomes due {dueText(preview.next_due_at)}.
        </Text>
      ) : null}
      {shown < preview.objects_affected && (
        <Text type="secondary" style={{ display: 'block', marginBottom: 8, fontSize: 12 }}>
          Showing the first {shown.toLocaleString()} keys.
        </Text>
      )}
      <Table
        size="small"
        rowKey={(c) => `${c.bucket}/${c.key}`}
        dataSource={preview.candidates}
        pagination={{ pageSize: 10, hideOnSinglePage: true, showSizeChanger: false }}
        locale={{ emptyText: 'Nothing to do: no object matches the rule now.' }}
        columns={[
          {
            key: 'key',
            title: 'Object',
            render: (_, c) => (
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, wordBreak: 'break-all' }}>{c.key}</span>
            ),
          },
          {
            key: 'action',
            title: 'Action',
            render: (_, c) => (
              <span style={{ fontSize: 12, wordBreak: 'break-all' }}>{lifecycleActionLabel(c)}</span>
            ),
          },
          {
            key: 'created',
            title: 'Created',
            render: (_, c) => (
              <span style={{ fontSize: 12, whiteSpace: 'nowrap' }}>{new Date(c.created_at).toLocaleDateString()}</span>
            ),
          },
          {
            key: 'size',
            title: 'Size',
            align: 'right',
            render: (_, c) =>
              c.stored_size != null && c.stored_size !== c.size
                ? `${formatBytes(c.size)} (${formatBytes(c.stored_size)} stored)`
                : formatBytes(c.size),
          },
        ]}
      />
    </div>
  );
}
