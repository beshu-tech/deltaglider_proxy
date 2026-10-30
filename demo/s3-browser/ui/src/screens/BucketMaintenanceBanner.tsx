import { Progress, Spin } from 'antd';
import { activePercent, browserBannerText, phaseLabel, type MaintenanceJobView } from '../maintenanceStatus';

/** The bucket's active maintenance job (re-encrypt, migrate, …) above the listing. */
export default function BucketMaintenanceBanner({ job, writesBlocked }: { job: MaintenanceJobView; writesBlocked: boolean }) {
  return (
    <div
      role="status"
      data-writes-blocked={writesBlocked ? 'true' : 'false'}
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 14,
        padding: '10px 16px',
        // Amber while writes are refused; calm once they work again.
        background: writesBlocked ? 'rgba(217, 119, 6, 0.10)' : 'rgba(37, 99, 235, 0.08)',
        borderBottom: writesBlocked ? '1px solid rgba(217, 119, 6, 0.35)' : '1px solid rgba(37, 99, 235, 0.25)',
      }}
    >
      <Spin size="small" />
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontSize: 13, fontWeight: 600 }}>
          {browserBannerText(job, writesBlocked)}
        </div>
        <Progress
          aria-label="Bucket maintenance progress"
          percent={activePercent(job) ?? 100}
          status="active"
          showInfo={activePercent(job) != null}
          size="small"
          strokeColor="#d97706"
          style={{ margin: '4px 0 0', maxWidth: 420 }}
        />
      </div>
      <span style={{ fontSize: 12, opacity: 0.75, whiteSpace: 'nowrap' }}>
        {phaseLabel(job)}
      </span>
    </div>
  );
}
