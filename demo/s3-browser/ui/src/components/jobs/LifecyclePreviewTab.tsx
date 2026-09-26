import { Button } from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import { useLifecyclePreview } from '../../queries/jobs';
import { normalizeUiError } from '../../errorHandling';
import LifecyclePreviewList from './LifecyclePreviewList';

/** Drawer tab: the saved rule's dry run, with a refresh. Mounts lazily (on tab open). */
export default function LifecyclePreviewTab({ jobId }: { jobId: string }) {
  const preview = useLifecyclePreview(jobId);
  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: 8 }}>
        <Button size="small" icon={<ReloadOutlined />} loading={preview.isFetching} onClick={() => void preview.refetch()}>
          Refresh preview
        </Button>
      </div>
      <LifecyclePreviewList
        preview={preview.data}
        loading={preview.isLoading}
        error={preview.error}
        errorText={normalizeUiError(preview.error, 'Preview failed')}
      />
    </div>
  );
}
