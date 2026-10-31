/**
 * ConversionStatusRow — one compact row about a bucket's conversion to
 * encryption (docs/plan/encryption-conversion-marker.md). While the
 * conversion window is open, clients read the bucket's plaintext history
 * and the re-encrypt job converts it.
 *
 *   * running  — the job of this node: its progress, a link to the run.
 *   * resume   — the window is open and no job runs here (none was
 *                queued, or it ended with failures): Resume.
 *   * stopped  — the deadline passed before the job finished: reads of
 *                the history fail. Warning style, Resume (it keeps the
 *                original cutoff and gets a new deadline).
 *
 * Objects that the job converted although clients could not read them
 * (stored after the cutoff) are counted, with a link to the run's
 * failures. Resume is `POST /jobs/reencrypt` for the bucket: the job
 * extends the window by identity.
 */
import { useState } from 'react';
import { Button, Typography, message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import type { ConversionView } from '../adminApi';
import { startReencrypt } from '../adminApi';
import { conversionRowState, runId } from '../conversionView';
import { useNavigation } from '../NavigationContext';
import { plainError } from '../plainError';
import { qk } from '../queries/keys';
import { buildViewUrl } from '../urlState';
import { SPACE } from './shared-styles';

const { Text } = Typography;

interface Props {
  bucket: string;
  view: ConversionView | null | undefined;
  /** Name the bucket in the row (the backend card lists several). */
  showBucket?: boolean;
  /** No Resume button (read-only view). */
  readOnly?: boolean;
}

export default function ConversionStatusRow({ bucket, view, showBucket, readOnly }: Props) {
  const qc = useQueryClient();
  const { navigate } = useNavigation();
  const [busy, setBusy] = useState(false);
  const state = conversionRowState(view);
  if (!state || !view) return null;

  const openRun = (jobId: number, tab?: string) =>
    navigate(buildViewUrl('admin', 'jobs', tab ? { job: runId(jobId), tab } : { job: runId(jobId) }));

  const resume = async () => {
    setBusy(true);
    try {
      const r = await startReencrypt([bucket]);
      const error = r.errors[0]?.error;
      if (error) message.error(`Resume of ${bucket}: ${error}`);
      else message.success(`Conversion of ${bucket} resumed`);
      await qc.invalidateQueries({ queryKey: qk.backends.origins() });
    } catch (e) {
      message.error(plainError(e, `The conversion of ${bucket} could not be resumed.`).text);
    } finally {
      setBusy(false);
    }
  };

  const who = showBucket ? `${bucket}: ` : '';
  let text: string;
  let type: 'secondary' | 'warning' | undefined;
  switch (state.kind) {
    case 'running':
      text = `${who}encrypting the existing objects${state.percent === null ? '' : `, ${state.percent}%`}. They stay readable meanwhile.`;
      type = 'secondary';
      break;
    case 'resume':
      text = state.failed
        ? `${who}the encryption of the existing objects stopped with failures. They stay readable until the window closes.`
        : `${who}the existing objects are not all encrypted, and no job runs on this instance. They stay readable until the window closes.`;
      type = 'secondary';
      break;
    case 'stopped':
      text = `${who}the conversion stopped before it finished. Objects that are not encrypted yet fail to read until it resumes.`;
      type = 'warning';
      break;
  }
  const deadline = view.deadline ? new Date(view.deadline * 1000).toLocaleString() : undefined;

  return (
    <div
      data-testid="conversion-status-row"
      data-state={state.kind}
      style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: SPACE.xs, marginTop: SPACE.xs }}
    >
      <Text type={type} style={{ fontSize: 12 }} title={deadline ? `The conversion window closes at ${deadline}.` : undefined}>
        {text}
      </Text>
      {view.adopted_after_cutoff > 0 && state.kind !== 'stopped' && view.job_on_this_node && (
        <Button size="small" type="link" style={{ padding: 0, fontSize: 12 }} onClick={() => openRun(view.job_on_this_node!.id, 'failures')}>
          {view.adopted_after_cutoff === 1
            ? '1 object stored after the cutoff'
            : `${view.adopted_after_cutoff} objects stored after the cutoff`}
        </Button>
      )}
      {state.jobId !== null && (
        <Button size="small" type="link" style={{ padding: 0, fontSize: 12 }} onClick={() => openRun(state.jobId!)}>
          View run
        </Button>
      )}
      {state.kind !== 'running' && !readOnly && (
        <Button size="small" loading={busy} onClick={() => void resume()}>
          Resume
        </Button>
      )}
    </div>
  );
}
