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
 * "Create and open settings" (admin only) creates the bucket the same way,
 * then opens its card on the Buckets page: compression, quota, public
 * access and the rest of its settings.
 */
import { useEffect, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { Alert, Button, Modal, Input, Radio, Select, Typography, message } from 'antd';
import type { InputRef } from 'antd';
import { createBucket } from '../s3client';
import { getBackends, getBucketEncryptionPreview } from '../adminApi';
import type { BackendInfo } from '../adminApi';
import { normalizeUiError } from '../errorHandling';
import { bucketNameError } from '../bucketName';
import { encryptionWord } from '../backendFacts';
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
  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [defaultBackend, setDefaultBackend] = useState<string | undefined>(undefined);
  const [selectedBackend, setSelectedBackend] = useState<string | undefined>(undefined);
  const inputRef = useRef<InputRef>(null);
  const [messageApi, contextHolder] = message.useMessage();
  const [encChoice, setEncChoice] = useState<NewBucketEncryption['kind']>('same');
  const [keyValue, setKeyValue] = useState<BucketKeyValue | null>(null);
  const [createError, setCreateError] = useState<string | null>(null);
  const { navigate } = useNavigation();

  // Focus the name field shortly after opening.
  useEffect(() => {
    if (!open) return;
    const id = window.setTimeout(() => inputRef.current?.focus(), 80);
    return () => window.clearTimeout(id);
  }, [open]);

  // Load the backend list (admin only). Preselect: explicit preset > default > first.
  useEffect(() => {
    if (!open || !canAdmin) return;
    let cancelled = false;
    getBackends()
      .then((resp) => {
        if (cancelled) return;
        const list = resp.backends || [];
        setBackends(list);
        setDefaultBackend(resp.default_backend || undefined);
        const preferred = presetBackend || resp.default_backend || list[0]?.name;
        setSelectedBackend(preferred || undefined);
      })
      .catch(() => {
        if (cancelled) return;
        setBackends([]);
        setDefaultBackend(undefined);
        setSelectedBackend(undefined);
      });
    return () => {
      cancelled = true;
    };
  }, [open, canAdmin, presetBackend]);

  const showPicker = canAdmin && backends.length > 1;
  const nameError = bucketNameError(name.trim());
  // The backend the bucket lands on: the picked one, else the default
  // (a name, not an encryption: the server answers that below).
  const targetName = selectedBackend ?? defaultBackend ?? backends[0]?.name;
  const trimmedName = name.trim();
  const preview = useQuery({
    queryKey: ['bucket-encryption-preview', trimmedName, targetName],
    queryFn: () => getBucketEncryptionPreview(trimmedName, targetName!),
    enabled: open && canAdmin && !!targetName && !!trimmedName && bucketNameError(trimmedName) === null,
    retry: false,
  }).data;
  const modes = preview?.bucket_modes ?? [];
  const offerKey = modes.includes('aes256-gcm-proxy');
  const offerOff = modes.includes('none') && !!preview && preview.mode !== 'none';
  const choice: NewBucketEncryption['kind'] =
    (encChoice === 'bucket-key' && !offerKey) || (encChoice === 'off' && !offerOff) ? 'same' : encChoice;
  const keyNotReady = choice === 'bucket-key' && !keyValue?.ready;
  const createDisabled = !name.trim() || nameError !== null || (showPicker && !selectedBackend) || keyNotReady;

  const reset = () => {
    setName('');
    setSelectedBackend(undefined);
    setEncChoice('same');
    setKeyValue(null);
    setCreateError(null);
  };

  /** `openSettings`: after the create, open the bucket's settings card. */
  const handleCreate = async (openSettings = false) => {
    const trimmed = name.trim();
    // A second Enter while the first create runs must not send another.
    if (creating || !trimmed || keyNotReady) return;
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
      const msg = normalizeUiError(e, "Unknown error");
      if (encryption) setCreateError(`The bucket was not created: ${msg}`);
      else messageApi.error(`Failed to create bucket: ${msg}`);
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
        okButtonProps={{ disabled: createDisabled }}
        footer={(_, { OkBtn, CancelBtn }) => (
          <>
            <CancelBtn />
            {canAdmin && (
              <Button disabled={createDisabled || creating} onClick={() => void handleCreate(true)}>
                Create and open settings
              </Button>
            )}
            <OkBtn />
          </>
        )}
        onCancel={() => {
          if (creating) return;
          reset();
          onClose();
        }}
        destroyOnHidden
      >
        <Input
          ref={inputRef}
          data-testid="bucket-name"
          placeholder="Bucket name"
          aria-label="Bucket name"
          value={name}
          // Keep what the user typed and say what is wrong: a silent rewrite
          // ("My_Bucket" → "mybucket") creates a bucket nobody asked for.
          onChange={(e) => setName(e.target.value)}
          onPressEnter={() => { if (!creating && !nameError && !keyNotReady) void handleCreate(); }}
          status={nameError ? 'error' : undefined}
          style={{ fontFamily: 'var(--font-mono)' }}
        />
        {nameError && (
          <Text role="alert" type="danger" style={{ display: 'block', marginTop: 6, fontSize: 12 }}>
            {nameError}
          </Text>
        )}
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
                    <div style={{ fontSize: 11, opacity: 0.65 }}>{opt.data.sublabel}</div>
                  )}
                </div>
              )}
              showSearch={{ optionFilterProp: 'label' }}
              placeholder="Choose backend"
              style={{ width: '100%' }}
              size="middle"
            />
            <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 6 }}>
              Created on the selected backend. Changing it later requires migrating data.
            </Text>
          </div>
        )}
        {canAdmin && preview && (offerKey || offerOff) && (
          <div style={{ marginTop: 12 }} data-testid="bucket-encryption-choice">
            <Text strong style={{ display: 'block', fontSize: 12, marginBottom: 4 }}>
              Encryption
            </Text>
            <Radio.Group
              aria-label="Encryption of the new bucket"
              value={choice}
              onChange={(e) => setEncChoice(e.target.value)}
              style={{ display: 'flex', flexDirection: 'column', gap: 4 }}
            >
              <Radio value="same">
                {preview.source === 'bucket'
                  ? `Keep the settings this name already has (${describeBucketEncryption(preview).label})`
                  : `Same as backend (${modeWord(preview.mode, preview.encrypts)})`}
              </Radio>
              {offerKey && <Radio value="bucket-key">Encrypt with a bucket key</Radio>}
              {offerOff && <Radio value="off">Off for this bucket (plaintext)</Radio>}
            </Radio.Group>
            {choice === 'bucket-key' && name.trim() && !nameError && (
              <div style={{ marginTop: 8 }}>
                <BucketKeySetup bucket={name.trim()} backend={preview.backend} onChange={setKeyValue} />
              </div>
            )}
            {choice === 'bucket-key' && (!name.trim() || nameError) && (
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 6 }}>
                Type the bucket name first: the key id starts with it.
              </Text>
            )}
            {choice === 'off' && (
              <Text type="warning" style={{ display: 'block', fontSize: 11, marginTop: 6 }}>
                New writes to this bucket are stored as plaintext, although {preview.backend} encrypts. You confirm this when you click Create.
              </Text>
            )}
          </div>
        )}
        {createError && (
          <Alert type="error" showIcon style={{ marginTop: 12, borderRadius: 6, fontSize: 12 }} title={createError} />
        )}
      </Modal>
    </>
  );
}
