/**
 * The conversion state of a bucket (docs/plan/encryption-conversion-marker.md).
 * Until the deadline, clients read the bucket's unencrypted objects and
 * the re-encrypt job encrypts them.
 *
 *   * running  — the job of this proxy: its progress ("starting…" before
 *                the first count), a link to the run.
 *   * resume   — no job runs here (none was queued, it ended with
 *                failures, or it was cancelled): the deadline as a date,
 *                and Resume.
 *   * stopped  — the deadline passed before the job finished: reads of
 *                unencrypted objects fail. Warning colour, Resume (it
 *                keeps the original start and gets a new deadline).
 *
 * Two shapes, one set of words (`conversionView.ts`):
 *
 *   * `ConversionStatusInline` continues a status line of the bucket card:
 *     "Now: Encrypted · bucket key k · encrypting the existing objects,
 *     42% · View run".
 *   * `ConversionStatusLine` is one line of the backend card per bucket:
 *     the bucket name as a mono label, then a sentence.
 *
 * Objects that the job encrypted although they were stored after
 * encryption started are counted: a link to the run's failures when the
 * job ran on this proxy, plain text otherwise.
 *
 * Resume is `POST /jobs/reencrypt` for the bucket: a job request, not a
 * config write, so a read-only encryption (key from an env var) still
 * offers it; these components render only on admin pages. On a versioned
 * bucket Resume first asks whether the job deletes the old unencrypted
 * versions (the box starts unchecked). The bucket list refetches on its
 * own while a job runs (`useBucketOrigins`), so the state follows the job
 * and the status goes away when the conversion ends.
 */
import { useState, type ReactNode } from 'react';
import { Button, Space, Typography, message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import type { ConversionView } from '../adminApi';
import { startReencrypt } from '../adminApi';
import { confirmDialog } from '../confirmDialog';
import { adoptedText, conversionPhrase, conversionRowState, runId, sentence, type ConversionRowState } from '../conversionView';
import { useNavigation } from '../NavigationContext';
import { plainError } from '../plainError';
import { qk } from '../queries/keys';
import { buildViewUrl } from '../urlState';
import PurgeVersionsBox from './PurgeVersionsBox';
import { SPACE } from './shared-styles';

const { Text } = Typography;

interface Props {
  bucket: string;
  view: ConversionView | null | undefined;
}

const LINK = { padding: 0, height: 'auto', fontSize: 12 } as const;

/** On a versioned bucket: ask whether the job deletes the old unencrypted
 *  versions. `null` = cancelled. */
async function askPurge(bucket: string): Promise<boolean | null> {
  // The dialog's content is static: the box reports into a local.
  let purge = false;
  const ok = await confirmDialog({
    title: `Resume the conversion of ${bucket}?`,
    content: (
      <Space orientation="vertical" size={SPACE.xs} style={{ width: '100%' }}>
        <Text>
          The backend keeps old versions of bucket {bucket}. The job encrypts the current objects; their
          old unencrypted versions stay on the backend.
        </Text>
        <PurgeVersionsBox
          onChange={(v) => {
            purge = v;
          }}
        />
      </Space>
    ),
    okText: 'Resume',
  });
  return ok ? purge : null;
}

/** The links and the Resume button of a conversion state. */
function useActions(bucket: string, view: ConversionView, state: ConversionRowState): ReactNode[] {
  const qc = useQueryClient();
  const { navigate } = useNavigation();
  const [busy, setBusy] = useState(false);
  const openRun = (jobId: number, tab?: string) =>
    navigate(buildViewUrl('admin', 'jobs', tab ? { job: runId(jobId), tab } : { job: runId(jobId) }));
  const resume = async () => {
    const purge = view.versioned ? await askPurge(bucket) : false;
    if (purge === null) return;
    setBusy(true);
    try {
      const r = await startReencrypt([bucket], { purgePlaintextVersions: purge, openWindow: true });
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
      <Button
        key="run"
        type="link"
        size="small"
        style={LINK}
        aria-label={`View the run of ${bucket}`}
        onClick={() => openRun(state.jobId!)}
      >
        View run
      </Button>,
    );
  }
  if (view.adopted_after_cutoff > 0) {
    const text = adoptedText(view.adopted_after_cutoff);
    out.push(
      job ? (
        <Button key="adopted" type="link" size="small" style={LINK} onClick={() => openRun(job.id, 'failures')}>
          {text} (see the run)
        </Button>
      ) : (
        // The job ran on another proxy: no run here to link to.
        <Text key="adopted" type="secondary" style={{ fontSize: 12 }}>
          {text}
        </Text>
      ),
    );
  }
  if (state.kind !== 'running') {
    out.push(
      <Button
        key="resume"
        size="small"
        loading={busy}
        aria-label={`Resume the conversion of ${bucket}`}
        onClick={() => void resume()}
      >
        Resume
      </Button>,
    );
  }
  return out;
}

/** Separator of a status line. */
const DOT = <span aria-hidden> · </span>;

/** The conversion state as the continuation of the bucket card's "Now:"
 *  line; nothing without a conversion. */
export function ConversionStatusInline({ bucket, view }: Props) {
  const state = conversionRowState(view);
  if (!state || !view) return null;
  return <InlineBody bucket={bucket} view={view} state={state} />;
}

function InlineBody({ bucket, view, state }: Props & { view: ConversionView; state: ConversionRowState }) {
  const actions = useActions(bucket, view, state);
  return (
    <span data-testid="conversion-status" data-state={state.kind}>
      {DOT}
      <Text type={state.kind === 'stopped' ? 'warning' : 'secondary'} style={{ fontSize: 12 }}>
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
export function ConversionStatusLine({ bucket, view }: Props) {
  const state = conversionRowState(view);
  if (!state || !view) return null;
  return <LineBody bucket={bucket} view={view} state={state} />;
}

function LineBody({ bucket, view, state }: Props & { view: ConversionView; state: ConversionRowState }) {
  const actions = useActions(bucket, view, state);
  return (
    <div
      data-testid="conversion-status"
      data-state={state.kind}
      style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', gap: SPACE.xs, fontSize: 12 }}
    >
      <code style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>{bucket}</code>
      <Text type={state.kind === 'stopped' ? 'warning' : 'secondary'} style={{ fontSize: 12 }}>
        {sentence(conversionPhrase(state))}
      </Text>
      {actions}
    </div>
  );
}
