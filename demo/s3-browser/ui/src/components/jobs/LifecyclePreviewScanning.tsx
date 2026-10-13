import { Typography } from 'antd';
import { useLifecyclePreviewProgress } from '../../queries/jobs';

/**
 * While a preview request waits, the counters of the proxy's walk, read
 * every second: "Scanning… 4,210 objects so far, 12 match." Renders nothing
 * when no preview is pending.
 */
export default function LifecyclePreviewScanning({ jobId, active }: { jobId: string | null; active: boolean }) {
  const progress = useLifecyclePreviewProgress(jobId, active);
  if (!active) return null;
  const walk = progress.data?.running ? progress.data : undefined;
  return (
    <Typography.Text type="secondary" role="status" style={{ display: 'block', marginBottom: 8, fontSize: 12 }}>
      {walk
        ? `Scanning… ${walk.objects_scanned.toLocaleString()} objects so far` +
          (walk.objects_affected > 0 ? `, ${walk.objects_affected.toLocaleString()} match.` : '.')
        : 'Scanning…'}
    </Typography.Text>
  );
}
