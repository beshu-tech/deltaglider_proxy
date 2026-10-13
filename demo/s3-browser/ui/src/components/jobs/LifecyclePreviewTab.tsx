import { Button } from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import { useLifecyclePreview, useRefreshLifecyclePreview } from '../../queries/jobs';
import { normalizeUiError } from '../../errorHandling';
import LifecyclePreviewList from './LifecyclePreviewList';

/**
 * Drawer tab: the saved rule's dry run, with a refresh. Mounts lazily (on tab
 * open) and shows the cached preview while it is fresh (useLifecyclePreview).
 * "Refresh preview" asks the proxy for a new walk; it is disabled while a walk
 * runs, because a second click would only wait for the same walk.
 */
export default function LifecyclePreviewTab({ jobId }: { jobId: string }) {
  const preview = useLifecyclePreview(jobId);
  const refresh = useRefreshLifecyclePreview(jobId);
  const busy = preview.isFetching || refresh.isPending;
  const error = refresh.error ?? preview.error;
  const computedAt = preview.data?.computed_at ? Date.parse(preview.data.computed_at) : preview.dataUpdatedAt;
  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: 8 }}>
        <Button size="small" icon={<ReloadOutlined />} loading={busy} disabled={busy} onClick={() => refresh.mutate()}>
          Refresh preview
        </Button>
      </div>
      <LifecyclePreviewList
        preview={preview.data}
        loading={preview.isLoading || (refresh.isPending && !preview.data)}
        error={error}
        errorText={normalizeUiError(error, 'Preview failed')}
        computedAt={computedAt}
      />
    </div>
  );
}
