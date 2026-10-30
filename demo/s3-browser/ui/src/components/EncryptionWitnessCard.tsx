/**
 * Encryption witness — the System page card for several instances that
 * share a `config_sync_bucket` (issue #116 P3).
 *
 * Each instance compares the encryption of its new writes with the setting
 * that the cluster published. Where the two differ, THIS instance refuses
 * writes to that storage. The card shows the verdict (agree / mismatch /
 * unknown / unavailable), each mismatch (this instance's form against the
 * published form and the instance that published it), and a publish action
 * per mismatch and for all of them. A publish of a plaintext form is a
 * downgrade: it asks for a confirmation and sends it in the
 * `X-DGP-Confirm-Encryption-Downgrade` header.
 *
 * A single instance has no witness (the server answers `null`): the card
 * renders nothing.
 */
import { useEffect, useRef, useState } from 'react';
import { Alert, Button, Tag, Typography, message } from 'antd';
import { SafetyCertificateOutlined } from '@ant-design/icons';
import { useEncryptionWitness, usePublishEncryptionWitness } from '../queries/encryptionWitness';
import { useAdminConfig } from '../queries/config';
import {
  describeIntent,
  isPlaintext,
  publishPlan,
  storageLabel,
  type WitnessConfig,
  type WitnessMismatch,
  type WitnessState,
} from '../encryptionWitness';
import { confirmDialog } from '../confirmDialog';
import { normalizeUiError } from '../errorHandling';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';

const { Text } = Typography;

/** The id of the card: the bucket card's warning links here. */
export const WITNESS_CARD_ID = 'encryption-witness';

const STATE_TEXT: Record<WitnessState, { tag: string; tone: 'success' | 'warning' | 'error' | 'default'; body: string }> = {
  agree: {
    tag: 'Agree',
    tone: 'success',
    body: 'This instance encrypts new writes in the same way as the setting that the cluster published.',
  },
  mismatch: {
    tag: 'Mismatch',
    tone: 'warning',
    body: 'This instance encrypts some storages differently from the setting that the cluster published. It refuses writes to those storages until the two agree. Reads keep working.',
  },
  unknown: {
    tag: 'Unknown',
    tone: 'warning',
    body: 'The sync bucket did not answer, so this instance cannot compare its setting with the cluster. When the last good comparison is older than two polls, this instance refuses writes to the buckets that the witness covers.',
  },
  unavailable: {
    tag: 'Unavailable',
    tone: 'error',
    body: 'This instance cannot reach the witness at all. It refuses writes to the buckets that the witness covers. Reads keep working.',
  },
};

export default function EncryptionWitnessCard() {
  const { cardStyle } = useCardStyles();
  const colors = useColors();
  const witness = useEncryptionWitness();
  const { data: cfg } = useAdminConfig();
  const publish = usePublishEncryptionWitness();
  const [publishError, setPublishError] = useState<string | null>(null);
  const ref = useRef<HTMLDivElement>(null);

  const shown = witness.data != null;
  // The bucket card's warning links here with ?focus=encryption-witness.
  useEffect(() => {
    if (!shown) return;
    if (new URLSearchParams(window.location.search).get('focus') !== WITNESS_CARD_ID) return;
    ref.current?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }, [shown]);

  // A single instance: no witness, no card. While the first answer loads,
  // nothing either (a single instance would see a card flash).
  if (witness.data === null) return null;
  if (witness.data === undefined && !witness.error) return null;

  const run = async (mismatches: WitnessMismatch[], config: WitnessConfig) => {
    const plan = publishPlan(mismatches, config);
    if (plan.downgrades.length > 0) {
      const ok = await confirmDialog({
        title: 'Publish plaintext for the cluster?',
        content: (
          <>
            <p>
              This instance stores {plan.downgrades.map((s) => <Text code key={s}>{s}</Text>)} in plaintext. If you
              publish that, the other instances must also write in plaintext: an instance that still encrypts
              refuses writes there until its setting agrees.
            </p>
            <p>New writes are then not encrypted. Objects that are already stored keep their encryption.</p>
          </>
        ),
        okText: 'Publish plaintext',
        danger: true,
      });
      if (!ok) return;
    }
    setPublishError(null);
    try {
      const res = await publish.mutateAsync(plan);
      message.success(`Published this instance's setting. The witness now reports: ${res.status}.`);
    } catch (e) {
      setPublishError(normalizeUiError(e, 'The publish failed'));
    }
  };

  const status = witness.data;
  const text = status ? STATE_TEXT[status.status] : null;
  const mismatches = status?.mismatches ?? [];

  return (
    <div ref={ref} id={WITNESS_CARD_ID}>
      <div style={{ ...cardStyle, marginBottom: 0 }}>
        <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 16 }}>
          <SectionHeader
            icon={<SafetyCertificateOutlined />}
            title="Encryption witness"
            description="For running more than one proxy instance. Each instance compares how it encrypts new writes with the setting that the cluster published in the sync bucket."
          />
          {text && (
            <Tag color={text.tone} data-testid="witness-status">
              {text.tag}
            </Tag>
          )}
        </div>
        {witness.error && !status && (
          <Alert type="error" showIcon title={normalizeUiError(witness.error, 'Could not load the witness status')} />
        )}
        {text && <Text type="secondary" style={{ display: 'block', marginBottom: 12 }}>{text.body}</Text>}
        {status?.error && (
          <Alert type="warning" showIcon title="Last sync error" description={status.error} style={{ marginBottom: 12 }} />
        )}
        {mismatches.length > 0 && (
          <div role="list" aria-label="Encryption mismatches" style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
            {mismatches.map((m) => (
              <div
                role="listitem"
                key={`${m.backend}/${m.bucket}`}
                style={{
                  border: `1px solid ${colors.ACCENT_AMBER}55`,
                  borderRadius: 8,
                  padding: '10px 12px',
                  display: 'flex',
                  flexWrap: 'wrap',
                  alignItems: 'center',
                  gap: 12,
                }}
              >
                <div style={{ flex: '1 1 320px', minWidth: 0, fontSize: 12.5, lineHeight: 1.6 }}>
                  <div>
                    <Text strong>{storageLabel(m)}</Text>
                    <Text type="secondary"> on backend </Text>
                    <Text code>{m.backend}</Text>
                  </div>
                  <div>
                    <Text type="secondary">This instance: </Text>
                    <Text>{describeIntent(m.local)}</Text>
                  </div>
                  <div>
                    <Text type="secondary">Published: </Text>
                    <Text>{describeIntent(m.published)}</Text>
                    {m.set_by && <Text type="secondary"> (set by {m.set_by})</Text>}
                  </div>
                </div>
                <Button
                  size="small"
                  danger={isPlaintext(m.local)}
                  disabled={!cfg || publish.isPending}
                  onClick={() => cfg && void run([m], cfg)}
                  aria-label={`Publish this instance's setting for ${storageLabel(m)} on ${m.backend}`}
                  title="Make this instance's setting the one that the cluster uses. The other instances refuse writes to this storage until their setting agrees."
                >
                  Publish this instance&apos;s setting
                </Button>
              </div>
            ))}
            {mismatches.length > 1 && (
              <div>
                <Button
                  disabled={!cfg || publish.isPending}
                  onClick={() => cfg && void run(mismatches, cfg)}
                  title="Publish this instance's setting for every storage in the list."
                >
                  Publish all
                </Button>
              </div>
            )}
          </div>
        )}
        {publishError && (
          <Alert type="error" showIcon title={publishError} style={{ marginTop: 12 }} />
        )}
      </div>
    </div>
  );
}
