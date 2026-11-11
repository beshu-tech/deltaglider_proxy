import { useState } from 'react';
import { Button, Progress, message } from 'antd';
import { DeleteOutlined } from '@ant-design/icons';
import { cancelDeleteJob } from '../adminApi';
import { plainError } from '../plainError';
import { type MaintenanceJobView, deleteJobText } from '../maintenanceStatus';
import { noticeBand } from '../themeVars';

interface Props {
  /** The bucket's running delete jobs (the bucket status `deletes`). */
  jobs: MaintenanceJobView[];
  /** The signed-in user's name: the line names who started a job only when it is someone else. */
  me: string | null;
  /** After a cancel request: read the bucket status again. */
  onChanged: () => void;
}

/**
 * The bucket's running delete jobs, one line each, with Cancel for the
 * jobs this session may stop (`may_cancel`, decided by the server). A delete holds no gate, so the bucket stays
 * writable and the line is calm (blue), not the amber of a job that
 * blocks writes.
 */
export default function DeleteJobsBanner({ jobs, me, onChanged }: Props) {
  const [stopping, setStopping] = useState<number | null>(null);
  if (jobs.length === 0) return null;
  const cancel = async (id: number) => {
    setStopping(id);
    try {
      await cancelDeleteJob(id);
      onChanged();
    } catch (e) {
      message.error(plainError(e, 'Could not cancel the delete job').text);
    } finally {
      setStopping(null);
    }
  };
  return (
    <div
      role="status"
      aria-label="Running deletes"
      style={{
        display: 'flex',
        flexDirection: 'column',
        gap: 8,
        padding: '8px 16px',
        ...noticeBand('ACCENT_BLUE'),
      }}
    >
      {jobs.map((job) => {
        const total = job.objects_total ?? 0;
        const done = job.objects_done + job.objects_failed;
        const percent = total > 0 ? Math.min(99, Math.floor((done / total) * 100)) : 0;
        return (
          <div key={job.id} data-delete-job={job.id} style={{ display: 'flex', alignItems: 'center', gap: 16 }}>
            <DeleteOutlined aria-hidden style={{ opacity: 0.7 }} />
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontSize: 13, fontWeight: 600 }}>{deleteJobText(job, me)}</div>
              <Progress
                aria-label={`Delete ${job.id} progress`}
                percent={percent}
                status="active"
                showInfo={false}
                size="small"
                style={{ margin: '4px 0 0', maxWidth: 420 }}
              />
            </div>
            {job.may_cancel === true && job.status !== 'cancelling' && (
              <Button
                size="small"
                onClick={() => void cancel(job.id)}
                loading={stopping === job.id}
                aria-label={`Cancel delete ${job.id}`}
              >
                Cancel
              </Button>
            )}
          </div>
        );
      })}
    </div>
  );
}
