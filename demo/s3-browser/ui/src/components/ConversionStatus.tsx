/**
 * The conversion state of a bucket (docs/plan/encryption-conversion-marker.md).
 * While the conversion window is open, clients read the bucket's plaintext
 * history and the re-encrypt job converts it.
 *
 *   * running  — the job of this node: its progress ("starting…" before
 *                the first count), a link to the run.
 *   * resume   — the window is open and no job runs here (none was
 *                queued, or it ended with failures): Resume.
 *   * stopped  — the deadline passed before the job finished: reads of
 *                the history fail. Warning colour, Resume (it keeps the
 *                original cutoff and gets a new deadline).
 *
 * Two shapes, one set of words (`conversionView.ts`):
 *
 *   * `ConversionStatusInline` continues a status line of the bucket card:
 *     "Now: Encrypted · bucket key k · encrypting the existing objects,
 *     42% · View run".
 *   * `ConversionStatusLine` is one line of the backend card per bucket:
 *     the bucket name as a mono label, then a sentence.
 *
 * Objects that the job converted although clients could not read them
 * (stored after the cutoff) are counted, with a link to the run's
 * failures. Resume is `POST /jobs/reencrypt` for the bucket: the job
 * extends the window by identity. The bucket list refetches on its own
 * while a conversion runs (`useBucketOrigins`), so the state follows the
 * job and the status goes away when the window closes.
 */
import { useState, type ReactNode } from 'react';
import { Button, Typography, message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import type { ConversionView } from '../adminApi';
import { startReencrypt } from '../adminApi';
import { conversionPhrase, conversionRowState, runId, sentence, type ConversionRowState } from '../conversionView';
import { useNavigation } from '../NavigationContext';
import { plainError } from '../plainError';
import { qk } from '../queries/keys';
import { buildViewUrl } from '../urlState';
import { SPACE } from './shared-styles';

const { Text } = Typography;

interface Props {
  bucket: string;
  view: ConversionView | null | undefined;
  /** No Resume button (read-only view). */
  readOnly?: boolean;
}

const LINK = { padding: 0, height: 'auto', fontSize: 12 } as const;

/** The links and the Resume button of a conversion state. */
function useActions(bucket: string, view: ConversionView, state: ConversionRowState, readOnly?: boolean): ReactNode[] {
  const qc = useQueryClient();
  const { navigate } = useNavigation();
  const [busy, setBusy] = useState(false);
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
  const out: ReactNode[] = [];
  const job = view.job_on_this_node;
  if (state.jobId !== null) {
    out.push(
      <Button key="run" type="link" size="small" style={LINK} onClick={() => openRun(state.jobId!)}>
        View run
      </Button>,
    );
  }
  if (view.adopted_after_cutoff > 0 && job) {
    out.push(
      <Button key="adopted" type="link" size="small" style={LINK} onClick={() => openRun(job.id, 'failures')}>
        {view.adopted_after_cutoff === 1
          ? '1 object stored after the cutoff'
          : `${view.adopted_after_cutoff} objects stored after the cutoff`}
      </Button>,
    );
  }
  if (state.kind !== 'running' && !readOnly) {
    out.push(
      <Button key="resume" size="small" loading={busy} onClick={() => void resume()}>
        Resume
      </Button>,
    );
  }
  return out;
}

function deadlineTitle(view: ConversionView): string | undefined {
  return view.deadline ? `The conversion window closes at ${new Date(view.deadline * 1000).toLocaleString()}.` : undefined;
}

/** Separator of a status line. */
const DOT = <span aria-hidden> · </span>;

/** The conversion state as the continuation of the bucket card's "Now:"
 *  line; nothing without a conversion. */
export function ConversionStatusInline({ bucket, view, readOnly }: Props) {
  const state = conversionRowState(view);
  if (!state || !view) return null;
  return <InlineBody bucket={bucket} view={view} state={state} readOnly={readOnly} />;
}

function InlineBody({ bucket, view, state, readOnly }: Props & { view: ConversionView; state: ConversionRowState }) {
  const actions = useActions(bucket, view, state, readOnly);
  return (
    <span data-testid="conversion-status" data-state={state.kind}>
      {DOT}
      <Text type={state.kind === 'stopped' ? 'warning' : 'secondary'} style={{ fontSize: 12 }} title={deadlineTitle(view)}>
        {conversionPhrase(state)}
      </Text>
      {actions.map((a, i) => (
        <span key={i}>
          {DOT}
          {a}
        </span>
      ))}
    </span>
  );
}

/** One line of the backend card: the bucket's name as a label, then the
 *  state as a sentence; nothing without a conversion. */
export function ConversionStatusLine({ bucket, view, readOnly }: Props) {
  const state = conversionRowState(view);
  if (!state || !view) return null;
  return <LineBody bucket={bucket} view={view} state={state} readOnly={readOnly} />;
}

function LineBody({ bucket, view, state, readOnly }: Props & { view: ConversionView; state: ConversionRowState }) {
  const actions = useActions(bucket, view, state, readOnly);
  return (
    <div
      data-testid="conversion-status"
      data-state={state.kind}
      style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', gap: SPACE.xs, fontSize: 12 }}
    >
      <code style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>{bucket}</code>
      <Text
        type={state.kind === 'stopped' ? 'warning' : 'secondary'}
        style={{ fontSize: 12 }}
        title={deadlineTitle(view)}
      >
        {sentence(conversionPhrase(state))}
      </Text>
      {actions}
    </div>
  );
}
