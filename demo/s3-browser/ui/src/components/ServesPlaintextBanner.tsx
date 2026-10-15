/**
 * ServesPlaintextBanner — an encryption site (one bucket's own block, or a
 * backend) that encrypts new writes and still serves unencrypted objects
 * (`plaintext_reads: serve`, issue #120).
 *
 * "Refuse unencrypted objects" sets `plaintext_reads: refuse`. After that,
 * a read of an unencrypted object fails with an error, so the click asks
 * first. The dialog shows the last re-encrypt job of each affected bucket,
 * from the job list (GET /jobs, which the Buckets and Jobs pages already
 * read).
 */
import { useState } from 'react';
import { Alert, Button, Typography } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import { getJobs } from '../adminApi';
import { confirmDialog } from '../confirmDialog';
import { lastReencryptText, type JobRow } from '../jobsView';
import { qk } from '../queries/keys';

const { Text } = Typography;

/** How many buckets the dialog lists by name. */
const LISTED_BUCKETS = 10;

export default function ServesPlaintextBanner({
  subject,
  buckets,
  onRefuse,
  disabled,
}: {
  subject: 'bucket' | 'backend';
  /** The buckets whose reads change: the job status of each is shown. */
  buckets: readonly string[];
  /** Absent: the banner offers no button (read-only). */
  onRefuse?: () => Promise<unknown>;
  disabled?: boolean;
}) {
  const qc = useQueryClient();
  const [busy, setBusy] = useState(false);
  const where = subject === 'bucket' ? 'this bucket' : 'the buckets of this backend';

  const refuse = async () => {
    if (!onRefuse) return;
    setBusy(true);
    try {
      // The cached list when it is fresh; one GET /jobs otherwise.
      const rows: readonly JobRow[] | null =
        buckets.length > 0
          ? await qc
              .fetchQuery({ queryKey: qk.jobs.list(), queryFn: getJobs, staleTime: 5_000 })
              .then((d) => d.jobs)
              .catch(() => null)
          : null;
      const lines = rows ? buckets.map((b) => lastReencryptText(rows, b)) : [];
      const ok = await confirmDialog({
        title: 'Refuse unencrypted objects?',
        content: (
          <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
            <span>
              After this, an object in {where} that is not encrypted fails to read with an error. Run the
              re-encrypt job first, and make sure that it finished with no failures.
            </span>
            {buckets.length > 0 && !rows && <Text type="secondary">The job list could not be read.</Text>}
            {lines.length > 0 && (
              <div data-testid="refuse-plaintext-jobs" style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
                {lines.slice(0, LISTED_BUCKETS).map((l) => (
                  <Text key={l.text} type={l.clean ? 'success' : 'warning'}>
                    {l.text}
                  </Text>
                ))}
                {lines.length > LISTED_BUCKETS && (
                  <Text type="secondary">And {lines.length - LISTED_BUCKETS} more buckets.</Text>
                )}
              </div>
            )}
          </div>
        ),
        okText: 'Refuse unencrypted objects',
        danger: true,
      });
      if (ok) await onRefuse();
    } finally {
      setBusy(false);
    }
  };

  return (
    <Alert
      type="info"
      showIcon
      style={{ marginTop: 8, borderRadius: 6, fontSize: 12 }}
      title="Unencrypted objects are served"
      description={`The ${subject} encrypts new writes, and it also serves objects that are not encrypted, so that the history of ${where} stays readable until the re-encrypt job converts it. When the job has finished, refuse unencrypted objects: then nobody can put one directly on the storage and have the proxy serve it.`}
      action={
        onRefuse && (
          <Button size="small" disabled={disabled} loading={busy} onClick={() => void refuse()}>
            Refuse unencrypted objects
          </Button>
        )
      }
    />
  );
}
