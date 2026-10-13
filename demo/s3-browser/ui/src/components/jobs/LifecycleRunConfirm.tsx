import { useEffect, useRef, useState } from 'react';
import { Button, Modal, Typography, message } from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import type { JobRow } from '../../jobsView';
import { lifecycleRunLabel } from '../../jobsView';
import { useJobRuns, useLifecyclePreview, useRefreshLifecyclePreview } from '../../queries/jobs';
import { normalizeUiError } from '../../errorHandling';
import { useNow } from '../../hooks/useNow';
import LifecyclePreviewList from './LifecyclePreviewList';
import LifecyclePreviewScanning from './LifecyclePreviewScanning';
import { confirmNeedsFreshPreview, isSettledRunStatus, lifecycleRunResultText } from './lifecycleText';

interface Props {
  /** The lifecycle rule to run; null = closed. */
  row: JobRow | null;
  onRun: (row: JobRow) => void;
  onCancel: () => void;
}

/**
 * Lifecycle rules delete or move objects, so run-now shows the preview first
 * and runs only when the operator presses the button that names the count.
 * The count must be the one the run acts on: when the preview it has (the
 * Preview tab's, or the proxy's own cache of up to a minute) is older than a
 * few seconds, the dialog asks the proxy for a new walk, and it says how old
 * the walk is. "Refresh preview" also asks for a new walk.
 *
 * After the operator runs the rule, it watches that run and shows a toast
 * with its counts when it ends.
 */
export default function LifecycleRunConfirm({ row, onRun, onCancel }: Props) {
  const id = row?.id ?? null;
  const preview = useLifecyclePreview(id);
  const refresh = useRefreshLifecyclePreview(id);
  const now = useNow(row ? 1000 : 3_600_000);
  const data = preview.data;
  const busy = preview.isFetching || refresh.isPending;

  // One fresh walk per opening, when the preview at hand is old.
  const askedFresh = useRef<string | null>(null);
  useEffect(() => {
    if (!id) {
      askedFresh.current = null;
      return;
    }
    if (askedFresh.current === id || preview.isFetching || refresh.isPending) return;
    if (preview.isLoading) return;
    if (confirmNeedsFreshPreview(data?.computed_at, Date.now())) {
      askedFresh.current = id;
      refresh.mutate();
    } else {
      askedFresh.current = id;
    }
  }, [id, data?.computed_at, preview.isFetching, preview.isLoading, refresh]);

  const label = data
    ? lifecycleRunLabel(data.objects_affected, data.candidates.map((c) => c.action))
    : 'Run';
  const error = refresh.error ?? preview.error;

  // ── Result toast: watch the run that the operator started. ──
  const [watch, setWatch] = useState<{ id: string; name: string; after: number } | null>(null);
  const runs = useJobRuns(watch?.id ?? null);
  const [messageApi, msgCtx] = message.useMessage();
  useEffect(() => {
    if (!watch) return;
    const run = (runs.data?.runs ?? []).find((r) => r.started_at >= watch.after);
    if (!run || !isSettledRunStatus(run.status)) return;
    const { type, text } = lifecycleRunResultText(watch.name, run);
    void messageApi.open({ type, content: text, duration: 8 });
    setWatch(null);
  }, [watch, runs.data, messageApi]);
  // The runs list polls only while it holds an active run: refetch until the
  // new run shows up.
  const refetchRuns = runs.refetch;
  useEffect(() => {
    if (!watch) return;
    const t = window.setInterval(() => void refetchRuns(), 2000);
    return () => window.clearInterval(t);
  }, [watch, refetchRuns]);

  return (
    <>
      {msgCtx}
      <Modal
        open={!!row}
        title={row ? `Run lifecycle rule "${row.name}" now?` : undefined}
        width={720}
        onCancel={onCancel}
        okText={label}
        okButtonProps={{ danger: true, disabled: !data || busy, loading: busy }}
        onOk={() => {
          if (!row) return;
          setWatch({ id: row.id, name: row.name, after: Math.floor(Date.now() / 1000) - 2 });
          onRun(row);
        }}
        destroyOnHidden
      >
        <div style={{ display: 'flex', gap: 12, alignItems: 'flex-start' }}>
          <Typography.Paragraph type="secondary" style={{ fontSize: 13, flex: 1 }}>
            The run acts on the objects that match the rule when it starts. Objects that become due, or that someone
            writes, after the scan below can change the count.
          </Typography.Paragraph>
          <Button size="small" icon={<ReloadOutlined />} loading={busy} disabled={busy} onClick={() => refresh.mutate()}>
            Refresh preview
          </Button>
        </div>
        <LifecyclePreviewScanning jobId={id} active={busy} />
        <LifecyclePreviewList
          preview={data}
          loading={preview.isLoading || (refresh.isPending && !data)}
          error={error}
          errorText={normalizeUiError(error, 'Preview failed')}
          receivedAt={preview.dataUpdatedAt}
          nowMs={now}
        />
      </Modal>
    </>
  );
}
