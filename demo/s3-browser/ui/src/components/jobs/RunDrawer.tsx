/**
 * One run of a replication or lifecycle rule, on the Jobs page: what it
 * did, when, how it started, and its failed objects. The rule itself (its
 * definition, all its runs, Verify, Preview) is on its rule page.
 */
import { Button, Drawer, Tag, Typography } from 'antd';
import type { RunEntry } from '../../adminApi';
import { isActiveJobStatus, jobStatusLabel, jobStatusTone, kindLabel, rowMeterInput, runDuration, scopeText } from '../../jobsView';
import { runName, runOriginLabel } from '../../runsView';
import { useJobFailures } from '../../queries/jobs';
import { useNavigation } from '../../NavigationContext';
import { buildViewUrl } from '../../urlState';
import { useNow } from '../../hooks/useNow';
import { formatBytes } from '../../utils';
import TimeAgo from '../TimeAgo';
import OutcomeMeter from './OutcomeMeter';
import FailuresList from './FailuresList';
import './OutcomeMeter.css';

const { Text, Title } = Typography;

interface Props {
  run: RunEntry | null;
  onClose: () => void;
}

export default function RunDrawer({ run, onClose }: Props) {
  const { navigate } = useNavigation();
  const nowSec = useNow(run && isActiveJobStatus(run.status) ? 1000 : 60_000) / 1000;
  const failures = useJobFailures(run ? run.job_id : null, run?.run_id);
  if (!run) return <Drawer open={false} onClose={onClose} />;
  const p = run.progress;
  const facts: [string, React.ReactNode][] = [
    ['Started by', runOriginLabel(run)],
    ['Scope', scopeText(run.scope) || '—'],
    ['Started', <TimeAgo key="s" ts={run.started_at} />],
    [
      'Finished',
      run.finished_at ? <TimeAgo key="f" ts={run.finished_at} /> : isActiveJobStatus(run.status) ? 'still running' : '—',
    ],
    ['Took', runDuration(run.started_at, run.finished_at, nowSec) ?? '—'],
    [run.kind === 'replication' ? 'Copied' : 'Acted on', p.processed.toLocaleString()],
    ['Deleted', (p.deleted ?? 0).toLocaleString()],
    [run.kind === 'replication' ? 'Already in sync' : 'Skipped', p.skipped.toLocaleString()],
    // Lifecycle: why it skipped them (absent on a run older than the split).
    ...(p.not_due != null ? [['Not old enough yet', p.not_due.toLocaleString()] as [string, string]] : []),
    ...(p.excluded != null ? [['Not matched by the globs', p.excluded.toLocaleString()] as [string, string]] : []),
    ['Failed', p.failed.toLocaleString()],
    ['Data', formatBytes(p.bytes)],
  ];
  const rulePage = `storage/${run.kind}`;
  return (
    <Drawer
      open
      onClose={onClose}
      size="min(640px, 100%)"
      title={
        <span>
          {kindLabel(run.kind)} · <span style={{ fontFamily: 'var(--font-mono)' }}>{runName(run)}</span> · run #
          {run.run_id}
          <Tag color={jobStatusTone(run)} style={{ marginLeft: 8 }}>
            {jobStatusLabel(run)}
          </Tag>
        </span>
      }
      extra={
        <Button size="small" onClick={() => navigate(buildViewUrl('admin', rulePage, { job: run.job_id }))}>
          Open rule
        </Button>
      }
    >
      <div style={{ marginBottom: 12 }}>
        <OutcomeMeter {...rowMeterInput(run)} />
      </div>
      {facts.map(([k, v]) => (
        <div key={k} style={{ display: 'flex', gap: 12, padding: '6px 0' }}>
          <Text type="secondary" style={{ width: 110, flexShrink: 0, fontSize: 12 }}>
            {k}
          </Text>
          <Text style={{ fontSize: 13, minWidth: 0, wordBreak: 'break-word' }}>{v}</Text>
        </div>
      ))}
      <Title level={5} style={{ marginTop: 20 }}>
        Failures of this run
      </Title>
      <FailuresList data={failures.data} />
    </Drawer>
  );
}
