/**
 * CreateBucketModal — the shared "create a bucket" dialog.
 *
 * Extracted from Sidebar so it can also be opened from the Backends admin
 * panel ("Create bucket here", pre-targeted to a specific backend). Owns the
 * name input, the optional backend picker, and the create call; the consumer
 * supplies `onCreated(name)` and refreshes its own view there (the Sidebar
 * reloads its local bucket list; the Backends panel invalidates its origin
 * query). Reuses `createBucket(name, backendName)` from s3client.
 *
 * Backend picker visibility: admin-only AND >1 backend (creating on a specific
 * backend is an admin-gated server operation; with a single backend there's
 * nothing to choose). When `presetBackend` is supplied (Backends panel), that
 * backend is preselected.
 *
 * Encryption (admin only, issue #116): "Same as backend (<mode>)" by
 * default, or "Encrypt with a bucket key" / "Off for this bucket" when the
 * server accepts that mode for the bucket. What the bucket gets by default
 * is the server's answer (GET /buckets/:name/encryption-preview), which
 * also knows settings that the name already has. A chosen block goes WITH
 * the create in one request (POST /api/admin/buckets): the server checks
 * the name first, and a failed create takes the route and the block back.
 * "Off" on an encrypting backend asks for a confirmation before it sends
 * the downgrade header.
 *
 * "Create and configure…" (admin only, a link beside the one primary
 * Create) creates the bucket the same way, then opens its card on the
 * Buckets page: compression, quota, public access and the rest.
 *
 * The buttons stay enabled; Create checks the form and says what is
 * missing under the field (a disabled button gave no reason). A name that
 * already exists is an error under the name, never a "created" toast.
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { Button, Modal, Input, Radio, Select, Typography, message } from 'antd';
import type { InputRef } from 'antd';
import { createBucket } from '../s3client';
import { getBucketEncryptionPreview } from '../adminApi';
import { useBackends } from '../queries/backends';
import { qk } from '../queries/keys';
import { ApiError } from '../errorHandling';
import { plainError, type PlainError } from '../adminApi';
import PlainErrorAlert from './PlainErrorAlert';
import { v } from '../validation';
import FormField from './FormField';
import { SPACE } from './shared-styles';
import { encryptionWord } from '../backendFacts';
import { atomicWrites, needsAtomicWrites } from '../atomicWrites';
import AtomicWritesNotice from './AtomicWritesNotice';
import { describeBucketEncryption, modeWord, newBucketEncryptionBlock, type NewBucketEncryption } from '../bucketEncryption';
import { confirmDialog } from '../confirmDialog';
import BucketKeySetup, { type BucketKeyValue } from './BucketKeySetup';
import { useNavigation } from '../NavigationContext';
import { bucketSettingsUrl } from '../urlState';

const { Text } = Typography;

interface Props {
  open: boolean;
  /** When set (e.g. from the Backends panel), preselect this backend. */
  presetBackend?: string;
  /** Admin sessions can target a specific backend; others create on the default. */
  canAdmin: boolean;
  onClose: () => void;
  /** Called after a successful create. The consumer refreshes its own view. */
  onCreated: (name: string) => void;
}

export default function CreateBucketModal({
  open,
  presetBackend,
  canAdmin,
  onClose,
  onCreated,
}: Props) {
  const [name, setName] = useState('');
  const [creating, setCreating] = useState(false);
  const [selectedBackend, setSelectedBackend] = useState<string | undefined>(undefined);
  const inputRef = useRef<InputRef>(null);
  const [messageApi, contextHolder] = message.useMessage();
  const [encChoice, setEncChoice] = useState<NewBucketEncryption['kind']>('same');
  const [keyValue, setKeyValue] = useState<BucketKeyValue | null>(null);
  const [createError, setCreateError] = useState<PlainError | null>(null);
  // Create was clicked: the form now shows what is missing.
  const [tried, setTried] = useState(false);
  // The name the server (or the bucket list) said exists.
  const [takenName, setTakenName] = useState<string | null>(null);
  const { navigate } = useNavigation();

  // Focus the name field shortly after opening.
  useEffect(() => {
    if (!open) return;
    const id = window.setTimeout(() => inputRef.current?.focus(), 80);
    return () => window.clearTimeout(id);
  }, [open]);

  // The backend list (admin only), shared with the Backends page: a
  // "Check again" (which invalidates it) refreshes this dialog too.
  const backendsQuery = useBackends({ enabled: open && canAdmin });
  const backends = useMemo(() => (canAdmin ? (backendsQuery.data?.backends ?? []) : []), [canAdmin, backendsQuery.data]);
  const defaultBackend = (canAdmin && backendsQuery.data?.default_backend) || undefined;
  // Preselect once per opening: explicit preset > default > first.
  const preselected = useRef(false);
  useEffect(() => {
    if (!open) {
      preselected.current = false;
      return;
    }
    if (preselected.current || !backendsQuery.data) return;
    preselected.current = true;
    const preferred = presetBackend || backendsQuery.data.default_backend || backendsQuery.data.backends?.[0]?.name;
    setSelectedBackend(preferred || undefined);
  }, [open, presetBackend, backendsQuery.data]);

  const showPicker = canAdmin && backends.length > 1;
  const nameError = v.bucketName()(name.trim());
  // The backend the bucket lands on: the picked one, else the default
  // (a name, not an encryption: the server answers that below).
  const targetName = selectedBackend ?? defaultBackend ?? backends[0]?.name;
  const trimmedName = name.trim();
  const preview = useQuery({
    queryKey: qk.backends.encryptionPreview(trimmedName, targetName),
    queryFn: () => getBucketEncryptionPreview(trimmedName, targetName!),
    enabled: open && canAdmin && !!targetName && !!trimmedName && v.bucketName()(trimmedName) === null,
    retry: false,
  }).data;
  const modes = preview?.bucket_modes ?? [];
  const offerKey = modes.includes('aes256-gcm-proxy');
  const offerOff = modes.includes('none') && !!preview && preview.mode !== 'none';
  // Encryption needs atomic writes (N2): on a backend without them the
  // choices that encrypt with the proxy are off, with the reason. The
  // server's answer for this bucket wins over the backend list.
  const targetInfo = backends.find((b) => b.name === targetName);
  const capability = preview?.capability ?? targetInfo?.capability;
  const proxyUnavailable = preview?.proxy_unavailable ?? targetInfo?.proxy_unavailable ?? null;
  const atomic = atomicWrites(capability);
  const atomicReason = targetName ? needsAtomicWrites(targetName, atomic) : null;
  const sameEncryptsProxy = preview?.mode === 'aes256-gcm-proxy';
  const noAtomic = atomic.kind === 'missing';
  // The bucket key shows also when the server left it out for lack of
  // atomic writes (disabled, with the reason), as on the bucket card.
  const showKey = offerKey || !!proxyUnavailable;
  const available = (c: NewBucketEncryption['kind']): boolean =>
    c === 'same' ? !(noAtomic && sameEncryptsProxy) : c === 'bucket-key' ? offerKey && !noAtomic : offerOff;
  // An option that is not available is never the selection: the dialog
  // moves to the first one that is (Off, on a proxy-encrypting backend
  // without atomic writes).
  const choice: NewBucketEncryption['kind'] = available(encChoice)
    ? encChoice
    : ((['same', 'off', 'bucket-key'] as const).find(available) ?? encChoice);
  const choiceNeedsAtomic = choice === 'bucket-key' || (choice === 'same' && sameEncryptsProxy);
  const keyNotReady = choice === 'bucket-key' && !keyValue?.ready;
  const emptyName = !trimmedName;
  const nameMessage =
    takenName !== null && takenName === trimmedName
      ? `A bucket named ${trimmedName} already exists. Choose another name.`
      : emptyName
        ? tried
          ? 'Enter a bucket name.'
          : null
        : nameError;
  const backendMissing = showPicker && !selectedBackend;
  const formInvalid =
    emptyName || nameError !== null || backendMissing || keyNotReady || nameMessage !== null || (noAtomic && choiceNeedsAtomic) || !available(choice);

  const reset = () => {
    setName('');
    setSelectedBackend(undefined);
    setEncChoice('same');
    setKeyValue(null);
    setCreateError(null);
    setTried(false);
    setTakenName(null);
  };

  /** `openSettings`: after the create, open the bucket's settings card. */
  const handleCreate = async (openSettings = false) => {
    const trimmed = name.trim();
    // A second Enter while the first create runs must not send another.
    if (creating) return;
    setTried(true);
    if (formInvalid) {
      // The encryption choice says what is wrong under the group; the
      // name field takes the focus only when the name is the problem.
      if (nameMessage !== null || emptyName || nameError !== null) inputRef.current?.focus();
      return;
    }
    const enc: NewBucketEncryption =
      choice === 'bucket-key' && keyValue ? { kind: 'bucket-key', key: keyValue.key, keyId: keyValue.keyId } : { kind: choice === 'off' ? 'off' : 'same' };
    const encryption = newBucketEncryptionBlock(enc);
    // Plaintext where the backend encrypts: the operator confirms it here,
    // and only then does the request carry the downgrade header.
    if (enc.kind === 'off') {
      setCreating(true);
      const ok = await confirmDialog({
        title: `Store ${trimmed} as plaintext?`,
        content: `Backend ${targetName} encrypts its buckets. New writes to ${trimmed} will not be encrypted at rest.`,
        okText: 'Store as plaintext',
        danger: true,
      });
      setCreating(false);
      if (!ok) return;
    }
    setCreating(true);
    setCreateError(null);
    try {
      // With a block, the create goes to the named target in one request;
      // otherwise only pass an explicit backend when there is a choice.
      const explicitBackend = encryption ? targetName : showPicker ? selectedBackend : undefined;
      await createBucket(
        trimmed,
        explicitBackend,
        encryption ? { encryption, confirm: enc.kind === 'off' ? { downgrades: [trimmed] } : undefined } : undefined,
      );
      messageApi.success(`Bucket "${trimmed}" created`);
      reset();
      onClose();
      onCreated(trimmed);
      if (openSettings) navigate(bucketSettingsUrl(trimmed));
    } catch (e: unknown) {
      // An existing name: S3 says BucketAlreadyOwnedByYou / -Exists, the
      // admin route answers 409.
      if (
        e instanceof ApiError &&
        // A 409 for lack of atomic writes is not a taken name.
        ((e.status === 409 && e.code !== 'atomic_writes_required') ||
          e.code === 'BucketAlreadyOwnedByYou' ||
          e.code === 'BucketAlreadyExists')
      ) {
        setTakenName(trimmed);
        inputRef.current?.focus();
      } else {
        const why = plainError(e, 'The proxy did not create the bucket.');
        setCreateError({ text: `The bucket was not created. ${why.text}`, details: why.details });
      }
    } finally {
      setCreating(false);
    }
  };

  return (
    <>
      {contextHolder}
      <Modal
        title="Create bucket"
        open={open}
        okText="Create"
        onOk={() => void handleCreate()}
        confirmLoading={creating}
        footer={(_, { OkBtn, CancelBtn }) => (
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
            {canAdmin && (
              <Button
                type="link"
                style={{ paddingInline: 0, marginRight: 'auto' }}
                disabled={creating}
                title="Create the bucket, then open its settings: quota, compression, public access"
                onClick={() => void handleCreate(true)}
              >
                Create and configure…
              </Button>
            )}
            <span style={{ marginLeft: 'auto', display: 'inline-flex', gap: 8 }}>
              <CancelBtn />
              <OkBtn />
            </span>
          </div>
        )}
        onCancel={() => {
          if (creating) return;
          reset();
          onClose();
        }}
        destroyOnHidden
      >
        {/* A visible label, like every other form (UX harvest D60);
            FormField gives the error its sentence, icon colour and
            aria-invalid. */}
        <FormField
          label="Bucket name"
          helpText="Lowercase letters, digits, dots and hyphens, 3 to 63 characters."
          error={nameMessage ?? undefined}
          style={{ marginBottom: 0 }}
        >
          <Input
            ref={inputRef}
            data-testid="bucket-name"
            placeholder="releases"
            value={name}
            // Keep what the user typed and say what is wrong: a silent rewrite
            // ("My_Bucket" → "mybucket") creates a bucket nobody asked for.
            onChange={(e) => {
              setName(e.target.value);
              setCreateError(null);
            }}
            onPressEnter={() => void handleCreate()}
            style={{ fontFamily: 'var(--font-mono)' }}
          />
        </FormField>
        {showPicker && (
          <div style={{ marginTop: 12 }} data-testid="bucket-backend-select">
            <Select
              aria-label="Backend"
              value={selectedBackend}
              onChange={(value) => setSelectedBackend(value || undefined)}
              options={backends.map((backend) => ({
                value: backend.name,
                label: backend.name === defaultBackend ? `${backend.name} (default)` : backend.name,
                sublabel: `${backend.backend_type} · ${encryptionWord(backend.encryption)}`,
              }))}
              optionRender={(opt) => (
                <div>
                  <div>{opt.data.label}</div>
                  {opt.data.sublabel && (
                    <Text type="secondary" style={{ fontSize: 11, display: 'block' }}>{opt.data.sublabel}</Text>
                  )}
                </div>
              )}
              showSearch={{ optionFilterProp: 'label' }}
              placeholder="Choose backend"
              status={tried && backendMissing ? 'error' : undefined}
              style={{ width: '100%' }}
              size="middle"
            />
            {tried && backendMissing && (
              <Text role="alert" type="danger" style={{ display: 'block', marginTop: SPACE.xs, fontSize: 12 }}>
                Choose a backend.
              </Text>
            )}
            <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: SPACE.xs }}>
              Created on the selected backend. Changing it later requires migrating data.
            </Text>
          </div>
        )}
        {canAdmin && preview && (showKey || offerOff) && (
          <div style={{ marginTop: 12 }} data-testid="bucket-encryption-choice">
            <Text strong style={{ display: 'block', fontSize: 12, marginBottom: 4 }}>
              Encryption
            </Text>
            {(noAtomic || choiceNeedsAtomic) && targetName && (
              <AtomicWritesNotice backend={targetName} capability={capability} proxyUnavailable={proxyUnavailable} />
            )}
            <Radio.Group
              aria-label="Encryption of the new bucket"
              value={choice}
              onChange={(e) => setEncChoice(e.target.value)}
              style={{ display: 'flex', flexDirection: 'column', gap: 4 }}
            >
              <Radio value="same" disabled={!available('same')}>
                {preview.source === 'bucket'
                  ? `Keep the settings this name already has (${describeBucketEncryption(preview).label})`
                  : `Same as backend (${modeWord(preview.mode, preview.encrypts)})`}
                {!available('same') && atomicReason && <DisabledReason text={atomicReason} />}
              </Radio>
              {showKey && (
                <Radio value="bucket-key" disabled={!available('bucket-key')}>
                  Encrypt with a bucket key
                  {!available('bucket-key') && atomicReason && <DisabledReason text={atomicReason} />}
                </Radio>
              )}
              {offerOff && <Radio value="off">Off for this bucket (plaintext)</Radio>}
            </Radio.Group>
            {tried && !available(choice) && atomicReason && (
              <Text role="alert" type="danger" style={{ display: 'block', marginTop: SPACE.xs, fontSize: 12 }}>
                {atomicReason} Choose another encryption, or another backend.
              </Text>
            )}
            {choice === 'bucket-key' && name.trim() && !nameError && (
              <div style={{ marginTop: 8 }}>
                <BucketKeySetup bucket={name.trim()} backend={preview.backend} onChange={setKeyValue} />
              </div>
            )}
            {tried && keyNotReady && name.trim() && !nameError && (
              <Text role="alert" type="danger" style={{ display: 'block', marginTop: SPACE.xs, fontSize: 12 }}>
                Confirm that you saved the key: type its last 6 characters.
              </Text>
            )}
            {choice === 'bucket-key' && (!name.trim() || nameError) && (
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: SPACE.xs }}>
                Type the bucket name first: the key id starts with it.
              </Text>
            )}
            {choice === 'off' && (
              <Text type="warning" style={{ display: 'block', fontSize: 11, marginTop: SPACE.xs }}>
                New writes to this bucket are stored as plaintext, although {preview.backend} encrypts. You confirm this when you click Create.
              </Text>
            )}
          </div>
        )}
        {createError && <PlainErrorAlert error={createError} style={{ marginTop: 12 }} />}
      </Modal>
    </>
  );
}

/** The reason of a disabled option, inside its label: visible, and part of
 *  the radio's accessible name. */
function DisabledReason({ text }: { text: string }) {
  return (
    <Text type="secondary" style={{ fontSize: 11, display: 'block' }}>
      {text}
    </Text>
  );
}
