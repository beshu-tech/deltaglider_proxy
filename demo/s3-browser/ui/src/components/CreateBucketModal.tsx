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
 * backend accepts that mode on a bucket (`bucket_encryption_modes`). A
 * chosen bucket encryption is written as the bucket's policy BEFORE the
 * bucket is created, so no object is ever written in the backend's mode
 * first. If the create then fails, the policy stays (the Buckets page shows
 * it as "bucket not found") and a retry creates the bucket under it.
 */
import { useEffect, useRef, useState } from 'react';
import { Alert, Modal, Input, Radio, Select, Typography, message } from 'antd';
import type { InputRef } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import { createBucket } from '../s3client';
import { getBackends, getSectionVersioned } from '../adminApi';
import type { BackendInfo } from '../adminApi';
import { normalizeUiError } from '../errorHandling';
import { bucketNameError } from '../bucketName';
import { applySection } from '../applySection';
import { encryptionWord, encryptsAtRest } from '../backendFacts';
import { newBucketPolicyBody, type NewBucketEncryption } from '../bucketEncryption';
import BucketKeySetup, { type BucketKeyValue } from './BucketKeySetup';

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
  const qc = useQueryClient();
  const [encChoice, setEncChoice] = useState<NewBucketEncryption['kind']>('same');
  const [keyValue, setKeyValue] = useState<BucketKeyValue | null>(null);
  const [createError, setCreateError] = useState<string | null>(null);

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
  // The backend the bucket lands on: the picked one, else the default.
  const target = backends.find((b) => b.name === (selectedBackend ?? defaultBackend)) ?? backends[0];
  const modes = canAdmin ? (target?.bucket_encryption_modes ?? []) : [];
  const offerKey = modes.includes('aes256-gcm-proxy');
  const offerOff = modes.includes('none') && encryptsAtRest(target?.encryption);
  const choice: NewBucketEncryption['kind'] =
    (encChoice === 'bucket-key' && !offerKey) || (encChoice === 'off' && !offerOff) ? 'same' : encChoice;
  const keyNotReady = choice === 'bucket-key' && !keyValue?.ready;

  const reset = () => {
    setName('');
    setSelectedBackend(undefined);
    setEncChoice('same');
    setKeyValue(null);
    setCreateError(null);
  };

  const handleCreate = async () => {
    const trimmed = name.trim();
    if (!trimmed || keyNotReady) return;
    setCreating(true);
    setCreateError(null);
    // The bucket's own encryption first: the policy is in place before the
    // bucket exists, so its first write already uses it.
    const enc: NewBucketEncryption =
      choice === 'bucket-key' && keyValue ? { kind: 'bucket-key', key: keyValue.key, keyId: keyValue.keyId } : { kind: choice === 'off' ? 'off' : 'same' };
    const pin = target && !target.is_synthesized ? target.name : null;
    const body = newBucketPolicyBody(trimmed, pin, enc);
    if (body) {
      try {
        const { version } = await getSectionVersioned('storage');
        // Off on an encrypting backend: the operator chose plaintext here.
        const resp = await applySection(qc, 'storage', body, version, enc.kind === 'off' ? { downgrades: [trimmed] } : undefined);
        if (!resp.ok) {
          setCreateError(resp.error || 'The proxy refused the encryption setting.');
          setCreating(false);
          return;
        }
      } catch (e: unknown) {
        setCreateError(`The bucket was not created: ${normalizeUiError(e, 'the encryption setting failed')}`);
        setCreating(false);
        return;
      }
    }
    try {
      // Only pass an explicit backend when there's a real choice to make.
      const explicitBackend = showPicker ? selectedBackend : undefined;
      await createBucket(trimmed, explicitBackend);
      messageApi.success(`Bucket "${trimmed}" created`);
      reset();
      onClose();
      onCreated(trimmed);
    } catch (e: unknown) {
      const msg = normalizeUiError(e, "Unknown error");
      if (body) {
        setCreateError(
          `The encryption setting is saved, but the bucket was not created: ${msg}. Click Create to try again, or remove the setting on the Buckets page.`
        );
      } else {
        messageApi.error(`Failed to create bucket: ${msg}`);
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
        onOk={handleCreate}
        confirmLoading={creating}
        okButtonProps={{
          disabled: !name.trim() || nameError !== null || (showPicker && !selectedBackend) || keyNotReady,
        }}
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
          onPressEnter={() => { if (!nameError && !keyNotReady) void handleCreate(); }}
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
        {canAdmin && target && (offerKey || offerOff) && (
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
              <Radio value="same">Same as backend ({encryptionWord(target.encryption)})</Radio>
              {offerKey && <Radio value="bucket-key">Encrypt with a bucket key</Radio>}
              {offerOff && <Radio value="off">Off for this bucket (plaintext)</Radio>}
            </Radio.Group>
            {choice === 'bucket-key' && name.trim() && !nameError && (
              <div style={{ marginTop: 8 }}>
                <BucketKeySetup bucket={name.trim()} backend={target.name} onChange={setKeyValue} />
              </div>
            )}
            {choice === 'bucket-key' && (!name.trim() || nameError) && (
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 6 }}>
                Type the bucket name first: the key id starts with it.
              </Text>
            )}
            {choice === 'off' && (
              <Text type="warning" style={{ display: 'block', fontSize: 11, marginTop: 6 }}>
                New writes to this bucket are stored as plaintext, although {target.name} encrypts.
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
