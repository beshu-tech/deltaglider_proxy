/**
 * The visible reason why proxy encryption is not offered on a backend
 * (encryption needs atomic writes, N2), with the doc link and "Check
 * again": a fresh probe of the backend (POST /backends/:name/capability-probe).
 * After the probe the backends and bucket lists refresh, so a backend
 * that now proves atomic writes enables the options at once.
 *
 * Renders nothing when the backend has atomic writes. Without a verdict
 * yet it shows a short note (the action checks).
 */
import { useState } from 'react';
import { Alert, Button, Typography } from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import type { BackendCapabilityVerdict } from '../adminApi';
import { probeBackendCapability } from '../adminApi';
import { atomicWrites, encryptionUnavailableReason, UNCHECKED_NOTE } from '../atomicWrites';
import { plainError, type PlainError } from '../plainError';
import { qk } from '../queries/keys';
import { LinkifiedText } from './LinkifiedText';
import PlainErrorAlert from './PlainErrorAlert';
import { SPACE } from './shared-styles';

const { Text } = Typography;

/** "Check again": the fresh probe, then the lists refresh. */
export function CheckAgainButton({ backend }: { backend: string }) {
  const qc = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<PlainError | null>(null);
  return (
    <>
      <Button
        size="small"
        icon={<ReloadOutlined />}
        loading={busy}
        onClick={async () => {
          setBusy(true);
          setError(null);
          try {
            await probeBackendCapability(backend);
            // The list key is the prefix of the origins and of the Create
            // dialog's preview: one invalidation refreshes all three.
            await qc.invalidateQueries({ queryKey: qk.backends.list() });
          } catch (e) {
            setError(plainError(e, `The proxy could not check ${backend}. Try again.`));
          } finally {
            setBusy(false);
          }
        }}
      >
        Check again
      </Button>
      {error && <PlainErrorAlert error={error} style={{ marginTop: SPACE.xs }} />}
    </>
  );
}

export default function AtomicWritesNotice({
  backend,
  capability,
  proxyUnavailable,
}: {
  backend: string;
  capability: BackendCapabilityVerdict | undefined;
  /** The server's sentence (`proxy_unavailable`), when it sent one. */
  proxyUnavailable?: string | null;
}) {
  const state = atomicWrites(capability);
  if (state.kind === 'atomic') return null;
  if (state.kind === 'unchecked') {
    return (
      <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: SPACE.xs }} data-testid="atomic-writes-unchecked">
        {UNCHECKED_NOTE}
      </Text>
    );
  }
  return (
    <Alert
      type="warning"
      showIcon
      data-testid="atomic-writes-missing"
      style={{ borderRadius: 6, fontSize: 12, marginBottom: SPACE.xs }}
      title={<LinkifiedText text={encryptionUnavailableReason(backend, state, proxyUnavailable) ?? ''} />}
      description={<CheckAgainButton backend={backend} />}
    />
  );
}
