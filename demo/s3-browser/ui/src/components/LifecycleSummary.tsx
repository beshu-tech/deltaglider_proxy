import { Tag, Typography } from 'antd';
import type { LifecycleAction, LifecycleConfig } from '../adminApi';
import { useColors } from '../ThemeContext';
import { actionKind, actionLabel } from './lifecyclePayload';
import { useJobs } from '../queries/jobs';
import { firstRunText } from './jobs/lifecycleText';

const { Text } = Typography;

/** Apply-dialog summary card describing the pending lifecycle plan. */
export function LifecycleApplySummary({ lifecycle }: { lifecycle: LifecycleConfig }) {
  const colors = useColors();
  // A rule that the proxy has not scheduled yet (new, or enabled for the
  // first time) first runs one tick after the apply: say when, so nobody is
  // surprised by a delete a minute later.
  const jobs = useJobs();
  const scheduled = new Set(
    (jobs.data?.jobs ?? []).filter((j) => j.kind === 'lifecycle' && j.next_due_at).map((j) => j.name),
  );
  const firstRun = firstRunText(lifecycle.tick_interval, Date.now());
  return (
    <div
      style={{
        border: `1px solid ${colors.BORDER}`,
        borderRadius: 8,
        padding: 12,
        background: 'var(--input-bg)',
      }}
    >
      <Text strong>Lifecycle plan</Text>
      <Text type="secondary" style={{ display: 'block', fontSize: 12, marginTop: 4 }}>
        Scheduler {lifecycle.enabled ? 'enabled' : 'disabled'} · tick {lifecycle.tick_interval} · {lifecycle.rules.length} rule{lifecycle.rules.length === 1 ? '' : 's'}
      </Text>
      <div style={{ marginTop: 12, display: 'flex', flexDirection: 'column', gap: 8 }}>
        {lifecycle.rules.length === 0 ? (
          <Text type="secondary" style={{ fontSize: 12 }}>No rules configured.</Text>
        ) : lifecycle.rules.map((rule) => (
          <div key={rule.name} style={{ fontSize: 12, lineHeight: 1.6 }}>
            <Text code>{rule.name}</Text>{' '}
            <Tag color={rule.enabled ? 'warning' : 'default'}>
              {rule.enabled ? `${actionLabel(rule.action)} enabled` : 'disabled'}
            </Tag>
            <div>
              <Text strong>Scope:</Text> {rule.bucket}/{rule.prefix || '*'}
              {actionKind(rule.action) === 'retain-newest'
                ? ` · keep newest ${(rule.action as Extract<LifecycleAction, { type: 'retain-newest' }>).count}`
                : ` · older than ${rule.expire_after || '—'}`}
            </div>
            {actionKind(rule.action) === 'transition' && typeof rule.action === 'object' && (() => {
              const t = rule.action as Extract<LifecycleAction, { type: 'transition' | 'archive' }>;
              return (
                <div>
                  <Text strong>Destination:</Text> {t.destination.bucket}/{t.destination.prefix || '*'}
                  {t.delete_source_after_success ? ' · deletes source after verified copy' : ' · keeps source'}
                </div>
              );
            })()}
            {rule.enabled && !scheduled.has(rule.name) && firstRun && (
              <div>
                <Text type="warning">New schedule: {firstRun}.</Text>{' '}
                {lifecycle.enabled
                  ? 'Open its Preview tab after the apply to see what it will do before then.'
                  : 'It runs only after you turn on "Run lifecycle rules on schedule".'}
              </div>
            )}
            {(rule.include_globs.length > 0 || rule.exclude_globs.length > 0) && (
              <div>
                Include: {rule.include_globs.length ? rule.include_globs.join(', ') : 'all'} · Exclude: {rule.exclude_globs.length ? rule.exclude_globs.join(', ') : 'none'}
              </div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
