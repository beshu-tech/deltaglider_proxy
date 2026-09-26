import { Alert, Table, Typography } from 'antd';
import type { LifecyclePreview } from '../../adminApi';
import { formatBytes } from '../../utils';
import { LoadingState } from '../StatePlaceholders';

const { Text } = Typography;

interface Props {
  preview: LifecyclePreview | undefined;
  loading: boolean;
  error: unknown;
  errorText: string;
}

/**
 * The objects a lifecycle rule would act on: totals first (count and bytes),
 * then the candidate keys in a paged table. The server caps the candidate
 * list, so the table says when it shows fewer keys than the total.
 */
export default function LifecyclePreviewList({ preview, loading, error, errorText }: Props) {
  if (error) return <Alert type="error" showIcon title={errorText} />;
  if (loading || !preview) return <LoadingState label="Computing the preview…" />;
  const shown = preview.candidates.length;
  return (
    <div>
      <Text style={{ display: 'block', marginBottom: 8 }}>
        <strong>{preview.objects_affected.toLocaleString()}</strong> of {preview.objects_scanned.toLocaleString()} scanned
        objects would be affected, <strong>{formatBytes(preview.bytes_affected)}</strong> in total.
      </Text>
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
            render: (_, c) =>
              c.action === 'transition' ? `move to ${c.destination_bucket ?? '?'}` : c.action,
          },
          { key: 'size', title: 'Size', align: 'right', render: (_, c) => formatBytes(c.size) },
        ]}
      />
    </div>
  );
}
