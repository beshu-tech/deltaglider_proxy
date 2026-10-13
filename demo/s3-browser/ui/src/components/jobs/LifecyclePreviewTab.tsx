import { Button } from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import { useLifecyclePreview, useRefreshLifecyclePreview } from '../../queries/jobs';
import { normalizeUiError } from '../../errorHandling';
import LifecyclePreviewList from './LifecyclePreviewList';
import LifecyclePreviewScanning from './LifecyclePreviewScanning';

/**
 * Drawer tab: the saved rule's dry run, with a refresh. Mounts lazily (on tab
 * open) and shows the cached preview while it is fresh (useLifecyclePreview).
 * "Refresh preview" asks the proxy for a new walk; it is disabled while a walk
 * runs, because a second click would only wait for the same walk; the
 * walk's live counters show instead.
 */
export default function LifecyclePreviewTab({ jobId }: { jobId: string }) {
  const preview = useLifecyclePreview(jobId);
  const refresh = useRefreshLifecyclePreview(jobId);
  const busy = preview.isFetching || refresh.isPending;
  const error = refresh.error ?? preview.error;
  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: 8 }}>
        <Button size="small" icon={<ReloadOutlined />} loading={busy} disabled={busy} onClick={() => refresh.mutate()}>
          Refresh preview
        </Button>
      </div>
      <LifecyclePreviewScanning jobId={jobId} active={busy} />
      <LifecyclePreviewList
        preview={preview.data}
        loading={preview.isLoading || (refresh.isPending && !preview.data)}
        error={error}
        errorText={normalizeUiError(error, 'Preview failed')}
        receivedAt={preview.dataUpdatedAt}
      />
    </div>
  );
}
