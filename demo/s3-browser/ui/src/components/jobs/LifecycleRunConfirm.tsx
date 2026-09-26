import { Modal, Typography } from 'antd';
import type { JobRow } from '../../jobsView';
import { lifecycleRunLabel } from '../../jobsView';
import { useLifecyclePreview } from '../../queries/jobs';
import { normalizeUiError } from '../../errorHandling';
import LifecyclePreviewList from './LifecyclePreviewList';

interface Props {
  /** The lifecycle rule to run; null = closed. */
  row: JobRow | null;
  onRun: (row: JobRow) => void;
  onCancel: () => void;
}

/**
 * Lifecycle rules delete or move objects, so run-now shows the preview first
 * and runs only when the operator presses the button that names the count.
 */
export default function LifecycleRunConfirm({ row, onRun, onCancel }: Props) {
  const preview = useLifecyclePreview(row?.id ?? null);
  const data = preview.data;
  const label = data
    ? lifecycleRunLabel(data.objects_affected, data.candidates.map((c) => c.action))
    : 'Run';
  return (
    <Modal
      open={!!row}
      title={row ? `Run lifecycle rule "${row.name}" now?` : undefined}
      width={720}
      onCancel={onCancel}
      okText={label}
      okButtonProps={{ danger: true, disabled: !data, loading: preview.isFetching }}
      onOk={() => { if (row) onRun(row); }}
      destroyOnHidden
    >
      <Typography.Paragraph type="secondary" style={{ fontSize: 13 }}>
        The run acts on the objects that match the rule when it starts, which can differ slightly from this preview.
      </Typography.Paragraph>
      <LifecyclePreviewList
        preview={data}
        loading={preview.isLoading}
        error={preview.error}
        errorText={normalizeUiError(preview.error, 'Preview failed')}
      />
    </Modal>
  );
}
