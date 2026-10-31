/**
 * ServeDeprecationNotice — an encryption site (one bucket's own block, or
 * a backend) that still sets the deprecated `plaintext_reads: serve`
 * (#120). It is honoured in this release: reads serve every object that
 * is not encrypted, also one that someone put directly on the storage.
 *
 * "Convert now" removes the line. The server then opens a conversion
 * window for each bucket with objects (the history stays readable) and
 * starts the re-encrypt job that converts it.
 */
import { useState } from 'react';
import { Alert, Button } from 'antd';

export default function ServeDeprecationNotice({
  subject,
  onConvert,
  disabled,
}: {
  subject: 'bucket' | 'backend';
  /** Absent: the notice offers no button (read-only). */
  onConvert?: () => Promise<unknown>;
  disabled?: boolean;
}) {
  const [busy, setBusy] = useState(false);
  const where = subject === 'bucket' ? 'this bucket' : 'the buckets of this backend';
  const convert = async () => {
    if (!onConvert) return;
    setBusy(true);
    try {
      await onConvert();
    } finally {
      setBusy(false);
    }
  };
  return (
    <Alert
      type="warning"
      showIcon
      data-testid="serve-deprecation-notice"
      style={{ marginTop: 8, borderRadius: 6, fontSize: 12 }}
      title="Unencrypted objects are served (deprecated setting)"
      description={`The ${subject} sets plaintext_reads: serve, which the next release no longer accepts. Reads in ${where} serve every object that is not encrypted, also one that someone put directly on the storage. Convert now removes the setting and starts the re-encrypt job; the objects stay readable while it runs.`}
      action={
        onConvert && (
          <Button size="small" disabled={disabled} loading={busy} onClick={() => void convert()}>
            Convert now
          </Button>
        )
      }
    />
  );
}
