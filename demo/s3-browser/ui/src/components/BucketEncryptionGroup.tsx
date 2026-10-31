/**
 * BucketEncryptionGroup — the Encryption group of an expanded bucket card
 * (issue #116, P2).
 *
 * Shows the server's `effective_encryption` of the bucket and changes the
 * bucket's own `encryption` block:
 *
 *   * Inherit — the backend's encryption (no block).
 *   * Encrypt with a bucket key — proxy AES-256-GCM with a key that only
 *     this bucket uses (generate or paste, see BucketKeySetup).
 *   * the native modes, when the server accepts them on a bucket
 *     (`bucket_modes`).
 *   * Off for this bucket — plaintext even on an encrypting backend. Not
 *     the same as Inherit: the bucket key stays as a decrypt-only legacy
 *     key, so the objects written under it stay readable.
 *
 * A change is one storage-section merge-patch of this bucket, applied at
 * once (not staged with the card's other fields): the key is shown once,
 * and the plan → diff → apply dialog shows what the server does with it.
 * A downgrade (new writes become plaintext) is confirmed first and carries
 * `X-DGP-Confirm-Encryption-Downgrade`; clearing a legacy key needs a
 * per-bucket scan that found no object under it and carries
 * `X-DGP-Confirm-Key-Removal`.
 *
 * Turning on proxy AES for a bucket that holds objects starts their
 * conversion on the server (window + re-encrypt job): the dialog names
 * the object count from the validate, and the conversion row shows its
 * state afterwards on the "Now:" line (ConversionStatusInline).
 */
import { useState } from 'react';
import { Alert, Button, Checkbox, Input, Radio, Space, Tag, Typography, message } from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import type { EffectiveEncryption, SectionApplyResponse } from '../adminApi';
import { ConfigConflictError, getSectionVersioned, validateSection } from '../adminApi';
import { applySection, persistFailureText, sectionApplyErrorText } from '../applySection';
import {
  bucketEncryptionBody,
  choiceBlocked,
  confirmFor,
  currentChoice,
  describeBucketEncryption,
  inheritLabel,
  isDowngrade,
  modeWord,
  offeredChoices,
  type BucketEncryptionChange,
  type BucketEncryptionChoice,
} from '../bucketEncryption';
import { confirmDialog } from '../confirmDialog';
import { plainError, type PlainError } from '../plainError';
import PlainErrorAlert from './PlainErrorAlert';
import { useColors } from '../ThemeContext';
import ApplyDialog from './ApplyDialog';
import BucketKeySetup, { type BucketKeyValue } from './BucketKeySetup';
import LegacyKeyBanner from './LegacyKeyBanner';
import { ConversionStatusInline } from './ConversionStatus';
import { conversionCountText, conversionRowState, offersPurge, versionedWarnings, withoutVersionedNotes } from '../conversionView';
import PurgeVersionsBox from './PurgeVersionsBox';
import { formRow } from './ruleEditorHelpers';
import { SPACE } from './shared-styles';

const { Text } = Typography;

/** A radio beside a two-line (or taller) option: the dot sits on the
 *  title line, not centred on the whole block (it read as the radio of
 *  the prefix input below the title). */
const RADIO_TOP = { icon: { alignSelf: 'flex-start', marginTop: 4 } } as const;

interface Props {
  bucket: string;
  /** The server's answer; null = the bucket routes to an undefined backend. */
  eff: EffectiveEncryption | null | undefined;
  /** Set while the card's staged edits are unsaved: an encryption change
   *  applies at once, so it waits for those to be applied or discarded. */
  blockedReason?: string | null;
  /** After a successful apply: the parent refreshes and offers re-encrypt.
   *  `converting`: the apply started (or reused) the bucket's conversion,
   *  so its job exists already (from the apply response, not a refetch: a
   *  small conversion can end before the refetch). */
  onApplied: (before: EffectiveEncryption, converting: boolean) => void;
}

const CHOICE_TITLE: Record<BucketEncryptionChoice, string> = {
  inherit: 'Inherit',
  'bucket-key': 'Encrypt with a bucket key',
  'sse-kms': 'SSE-KMS on this bucket',
  'sse-s3': 'SSE-S3 on this bucket',
  off: 'Off for this bucket',
};

export default function BucketEncryptionGroup({ bucket, eff, blockedReason, onApplied }: Props) {
  const colors = useColors();
  const qc = useQueryClient();
  const [choice, setChoice] = useState<BucketEncryptionChoice | null>(null);
  const [rotating, setRotating] = useState(false);
  const [keyValue, setKeyValue] = useState<BucketKeyValue | null>(null);
  const [kmsKeyId, setKmsKeyId] = useState('');
  const [bucketKeyEnabled, setBucketKeyEnabled] = useState(true);
  const [dialog, setDialog] = useState<{ response: SectionApplyResponse; change: BucketEncryptionChange } | null>(null);
  // The dialog's purge box: unchecked each time the dialog opens.
  const [purge, setPurge] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<PlainError | null>(null);

  // Sentence case, like the other group titles of the bucket card (D67).
  const groupLabel: React.CSSProperties = {
    fontSize: 13,
    fontWeight: 600,
    fontFamily: 'var(--font-ui)',
    display: 'block',
    marginBottom: SPACE.xs,
    color: colors.TEXT_MUTED,
  };

  if (!eff) {
    return (
      <div style={{ marginTop: SPACE.sm }}>
        <Text style={groupLabel}>Encryption</Text>
        <Text type="secondary" style={{ fontSize: 12 }}>
          {describeBucketEncryption(eff).tooltip}
        </Text>
      </div>
    );
  }

  const current = currentChoice(eff);
  const selected = choice ?? current;
  const editing = rotating || selected !== current;
  const ownKey = eff.source === 'bucket' && eff.mode === 'aes256-gcm-proxy' && eff.has_key;
  const rotateBlocked = choiceBlocked(eff, 'off'); // the same slot rule as any retirement
  const readOnly = !!eff.key_env;
  const desc = describeBucketEncryption(eff);

  const cancel = () => {
    setChoice(null);
    setRotating(false);
    setKeyValue(null);
    setKmsKeyId('');
    setError(null);
  };

  const pendingChange = (): BucketEncryptionChange | null => {
    if (rotating || selected === 'bucket-key') {
      return keyValue?.ready ? { kind: 'bucket-key', key: keyValue.key, keyId: keyValue.keyId } : null;
    }
    switch (selected) {
      case 'inherit':
        return { kind: 'inherit' };
      case 'off':
        return { kind: 'off' };
      case 'sse-kms':
        return kmsKeyId.trim() ? { kind: 'sse-kms', kmsKeyId: kmsKeyId.trim(), bucketKeyEnabled } : null;
      case 'sse-s3':
        return { kind: 'sse-s3' };
      default:
        return null;
    }
  };

  /** Validate (with the confirmation headers), then open the apply dialog. */
  const review = async (change: BucketEncryptionChange) => {
    setError(null);
    if (isDowngrade(eff, change)) {
      const ok = await confirmDialog({
        title: `Store new writes to ${bucket} as plaintext?`,
        content: (
          <span>
            New writes to <code>{bucket}</code> are no longer encrypted at rest. Objects that are
            already encrypted stay encrypted and readable
            {eff.source === 'bucket' && eff.has_key ? ': the bucket key stays as a decrypt-only legacy key' : ''}.
            To decrypt them too, run the re-encrypt job afterwards.
          </span>
        ),
        okText: 'Store as plaintext',
        danger: true,
      });
      if (!ok) return;
    }
    setBusy(true);
    try {
      const response = await validateSection('storage', bucketEncryptionBody(bucket, eff, change), confirmFor(bucket, eff, change));
      setPurge(false);
      setDialog({ response, change });
    } catch (e) {
      setError(plainError(e, 'The proxy refused the change.'));
    } finally {
      setBusy(false);
    }
  };

  const apply = async (change: BucketEncryptionChange): Promise<boolean> => {
    setBusy(true);
    setError(null);
    try {
      const { version } = await getSectionVersioned('storage');
      const options = { ...confirmFor(bucket, eff, change), purgePlaintextVersions: purge && offersPurge(dialog?.response.conversion_preview) };
      const resp = await applySection(qc, 'storage', bucketEncryptionBody(bucket, eff, change), version, options);
      if (!resp.ok) {
        setError(plainError(resp.error || '', 'The proxy refused the change.'));
        return false;
      }
      const lost = persistFailureText(resp);
      if (lost) message.warning(lost, 10);
      else if (resp.conversions?.length)
        message.success(`Encryption of ${bucket} updated. A re-encrypt job encrypts its existing objects in the background.`);
      else message.success(`Encryption of ${bucket} updated`);
      cancel();
      onApplied(eff, (resp.conversions ?? []).some((c) => c.bucket === bucket));
      return true;
    } catch (e) {
      // A version conflict has its own sentence; any other failure is plain words.
      setError(
        e instanceof ConfigConflictError
          ? { text: sectionApplyErrorText(e, '') }
          : plainError(e, 'The proxy could not apply the encryption change. Try again.'),
      );
      return false;
    } finally {
      setBusy(false);
    }
  };

  const change = editing ? pendingChange() : null;
  const conversionText = conversionCountText(dialog?.response.conversion_preview);
  const versioned = versionedWarnings(dialog?.response.conversion_preview);
  // The summary says the versioning note in its own words: the dialog's
  // warning list drops the server's copy (it names an HTTP header).
  const dialogResponse = dialog && {
    ...dialog.response,
    warnings: withoutVersionedNotes(dialog.response.warnings, dialog.response.conversion_preview),
  };
  const enabling = change ? !eff.encrypts && change.kind !== 'off' && change.kind !== 'clear-legacy' && (change.kind !== 'inherit' || eff.inherited.encrypts) : false;
  const summary = dialog && (
    <Space orientation="vertical" size={6} style={{ width: '100%', fontSize: 12 }}>
      {dialog.change.kind === 'bucket-key' && (
        <Text style={{ fontSize: 12 }}>
          New writes to <code>{bucket}</code> are encrypted with key id <code>{dialog.change.keyId}</code>.
          {rotating || ownKey ? ' The current key stays as a decrypt-only legacy key.' : ''}
        </Text>
      )}
      {conversionText ? (
        <Text style={{ fontSize: 12 }} data-testid="conversion-count">
          {conversionText}
        </Text>
      ) : (
        (enabling || dialog.change.kind === 'bucket-key') && (
          <Text style={{ fontSize: 12 }}>
            Only new writes change. Objects already in the bucket stay as they are on disk until the
            re-encrypt job rewrites them; after you apply, the proxy offers that job.
          </Text>
        )
      )}
      {versioned.map((w) => (
        <Text key={w} type="secondary" style={{ fontSize: 12 }} data-testid="conversion-versioned">
          {w}
        </Text>
      ))}
      {offersPurge(dialog.response.conversion_preview) && (
        <PurgeVersionsBox checked={purge} onChange={setPurge} />
      )}
      {(enabling || dialog.change.kind === 'bucket-key' || conversionText) && (
        <Text type="warning" style={{ fontSize: 12 }}>
          While the re-encrypt job runs on the bucket, reads keep working, but uploads and deletes
          get a temporary 503 (S3 clients retry automatically).
        </Text>
      )}
      {isDowngrade(eff, dialog.change) && (
        <Text type="danger" style={{ fontSize: 12 }}>
          New writes to <code>{bucket}</code> are stored as plaintext.
        </Text>
      )}
    </Space>
  );

  return (
    <div style={{ marginTop: SPACE.sm }} data-testid="bucket-encryption-group">
      <Text style={groupLabel}>
        Encryption
        {/* Marks the one immediate control of a card whose other fields are
            staged, before the user touches it. */}
        <Tag
          title="An encryption change is not staged with the other settings: it has its own Review & apply, and applies at once."
          style={{ fontSize: 10, margin: '0 0 0 8px', padding: '0 8px', lineHeight: '16px', textTransform: 'none', letterSpacing: 0, fontWeight: 500 }}
        >
          applies at once
        </Tag>
        {eff.key_env && (
          <Tag style={{ fontSize: 10, fontFamily: 'var(--font-mono)', margin: '0 0 0 8px', padding: '0 8px', lineHeight: '16px', textTransform: 'none', letterSpacing: 0 }}>
            from env {eff.key_env}
          </Tag>
        )}
      </Text>
      <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: SPACE.xs }}>
        Now: {desc.label}
        {/* The conversion continues the state line; no separate block. */}
        {!editing && conversionRowState(eff.conversion) ? (
          // Resume is a job request, not a config write: offered also
          // when an env var makes the encryption read-only.
          <ConversionStatusInline bucket={bucket} view={eff.conversion} />
        ) : (
          '.'
        )}
      </Text>
      {blockedReason && !readOnly && (
        <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: SPACE.xs }}>
          {blockedReason}
        </Text>
      )}
      <Radio.Group
        aria-label="Encryption of this bucket"
        value={selected}
        disabled={readOnly || !!blockedReason || busy}
        onChange={(e) => {
          setRotating(false);
          setError(null);
          setChoice(e.target.value === current ? null : e.target.value);
        }}
        style={formRow(6, { flexDirection: 'column', alignItems: 'stretch' })}
      >
        {offeredChoices(eff).map((c) => {
          const blocked = choiceBlocked(eff, c);
          const sub =
            c === 'inherit'
              ? `New writes follow backend ${eff.backend}.`
              : c === 'bucket-key'
                ? `Proxy AES-256-GCM with a key that only this bucket uses. Other buckets on ${eff.backend} are not affected.`
                : c === 'off'
                  ? `New writes are stored as plaintext, even when ${eff.backend} encrypts. A bucket key stays as a decrypt-only legacy key.`
                  : `${modeWord(c === 'sse-kms' ? 'sse-kms' : 'sse-s3')}: the storage encrypts new writes of this bucket.`;
          return (
            <Radio
              key={c}
              value={c}
              // A Radio's own `disabled` wins over the group's, so repeat it.
              disabled={!!blocked || readOnly || !!blockedReason || busy}
              style={{ alignItems: 'flex-start' }}
              styles={RADIO_TOP}
              title={blocked ?? undefined}
            >
              <div>
                <span style={{ fontSize: 13 }}>{c === 'inherit' ? inheritLabel(eff) : CHOICE_TITLE[c]}</span>
                <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 1 }}>
                  {blocked && c !== current ? blocked : sub}
                </Text>
              </div>
            </Radio>
          );
        })}
      </Radio.Group>

      {ownKey && !editing && !readOnly && (
        <Button
          size="small"
          icon={<ReloadOutlined />}
          style={{ marginTop: SPACE.xs }}
          disabled={!!rotateBlocked || !!blockedReason}
          title={rotateBlocked ?? undefined}
          onClick={() => setRotating(true)}
        >
          Rotate key
        </Button>
      )}

      {eff.source === 'bucket' && eff.legacy_key_id && !editing && (
        <LegacyKeyBanner
          backendName={eff.backend}
          bucket={bucket}
          readOnly={readOnly || !!blockedReason}
          onClearLegacy={async () => {
            await apply({ kind: 'clear-legacy' });
          }}
        />
      )}

      {editing && (
        <div style={{ marginTop: SPACE.sm, paddingLeft: SPACE.xxs }}>
          {(rotating || selected === 'bucket-key') && (
            <BucketKeySetup
              bucket={bucket}
              backend={eff.backend}
              usedKeyIds={[eff.key_id, eff.legacy_key_id].filter((x): x is string => !!x)}
              onChange={setKeyValue}
            />
          )}
          {selected === 'sse-kms' && !rotating && (
            <>
              <Input
                aria-label="KMS key ARN or alias"
                size="small"
                value={kmsKeyId}
                onChange={(e) => setKmsKeyId(e.target.value)}
                placeholder="arn:aws:kms:eu-central-1:123456789012:key/abcd-efgh"
                style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}
              />
              <Checkbox checked={bucketKeyEnabled} onChange={(e) => setBucketKeyEnabled(e.target.checked)} style={{ marginTop: SPACE.xs }}>
                <Text style={{ fontSize: 12 }}>Enable S3 bucket keys (fewer KMS requests)</Text>
              </Checkbox>
            </>
          )}
          {selected === 'off' && !rotating && eff.encrypts && (
            <Alert
              type="warning"
              showIcon
              style={{ borderRadius: 6, fontSize: 12 }}
              title="New writes to this bucket will be plaintext"
              description="Objects that are already encrypted stay readable. Run the re-encrypt job afterwards to decrypt them."
            />
          )}
          {error && <PlainErrorAlert error={error} style={{ marginTop: SPACE.xs }} />}
          <Space style={{ marginTop: SPACE.sm }}>
            <Button size="small" onClick={cancel} disabled={busy}>
              Cancel
            </Button>
            <Button
              size="small"
              type="primary"
              loading={busy}
              // Staged edits elsewhere on the page wait for nothing: this
              // change applies at once, so it waits for them instead.
              disabled={!change || !!blockedReason}
              title={blockedReason ?? undefined}
              onClick={() => change && !blockedReason && void review(change)}
            >
              Review &amp; apply
            </Button>
          </Space>
        </div>
      )}
      {!editing && error && <PlainErrorAlert error={error} style={{ marginTop: SPACE.xs }} />}

      <ApplyDialog
        open={dialog !== null}
        section="storage"
        response={dialogResponse ?? null}
        summary={summary}
        loading={busy}
        onCancel={() => setDialog(null)}
        onApply={() => {
          const c = dialog?.change;
          if (!c) return;
          // Staged edits appeared while the dialog was open: do not apply.
          if (blockedReason) {
            setDialog(null);
            setError({ text: blockedReason });
            return;
          }
          void apply(c).then(() => setDialog(null));
        }}
      />
    </div>
  );
}
