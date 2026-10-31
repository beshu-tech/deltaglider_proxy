import { Tag, Typography } from 'antd';
import type { JobFailureEntry } from '../../adminApi';
import { failuresCountText, groupFailureErrors } from '../../jobsView';
import { useColors } from '../../ThemeContext';
import TimeAgo from '../TimeAgo';
import RecordList from './RecordList';
import { CheckCircleOutlined } from '@ant-design/icons';
import { EmptyState } from '../StatePlaceholders';

const { Text } = Typography;

/**
 * The failed objects of a job or of one run: how many, the errors that
 * repeat, and one row per object.
 */
export default function FailuresList({
  data,
}: {
  data?: { failures: JobFailureEntry[]; total?: number | null };
}) {
  const c = useColors();
  const failures = data?.failures ?? [];
  const failuresCount = failuresCountText(failures.length, data?.total);
  const errorGroups = groupFailureErrors(failures);
  return (
    <div>
      {failuresCount && (
        <Text type="secondary" style={{ display: 'block', fontSize: 12, marginBottom: 8 }}>
          {failuresCount}
        </Text>
      )}
      {errorGroups.length > 0 && (
        <div style={{ marginBottom: 12, fontSize: 12 }} aria-label="Repeated errors">
          {errorGroups.slice(0, 3).map((g) => (
            <div key={g.error} style={{ display: 'flex', gap: 8, padding: '2px 0' }}>
              <Text strong style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
                {g.count}×
              </Text>
              <span className="dg-fail-text" style={{ color: c.ACCENT_RED }}>
                {g.error}
              </span>
            </div>
          ))}
        </div>
      )}
      <RecordList
        rows={failures}
        rowKey={(f) => String(f.id)}
        empty={<EmptyState compact icon={<CheckCircleOutlined />} title="No recorded failures" />}
        columns={[
          {
            key: 'when',
            label: 'When',
            track: 'max-content',
            render: (f) => (
              <span style={{ whiteSpace: 'nowrap' }}>
                <TimeAgo ts={f.occurred_at} />
                {f.trigger && (
                  <Tag
                    style={{ marginLeft: 4, fontSize: 11 }}
                    title={f.trigger === 'event' ? 'A live copy, made when the object changed' : 'A run of the rule'}
                  >
                    {f.trigger === 'event' ? 'live copy' : 'run'}
                  </Tag>
                )}
              </span>
            ),
          },
          {
            key: 'object',
            label: 'Object',
            track: 'minmax(0,1.2fr)',
            render: (f) => {
              const obj = `${f.bucket ? `${f.bucket}/` : ''}${f.object_key || '(job-level)'}`;
              return (
                <span className="dg-fail-text" title={obj} style={{ fontFamily: 'var(--font-mono)' }}>
                  {obj}
                  {f.dest_key && f.dest_key !== f.object_key && (
                    <span style={{ color: c.TEXT_MUTED }}> → {f.dest_key}</span>
                  )}
                </span>
              );
            },
          },
          {
            key: 'error',
            label: 'Error',
            track: 'minmax(0,1.5fr)',
            render: (f) => (
              <span className="dg-fail-text" title={f.error} style={{ color: c.ACCENT_RED }}>
                {f.error}
              </span>
            ),
          },
        ]}
      />
    </div>
  );
}
