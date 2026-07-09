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
 */
import { useEffect, useRef, useState } from 'react';
import { Modal, Input, Select, Typography, message } from 'antd';
import type { InputRef } from 'antd';
import { createBucket } from '../s3client';
import { getBackends } from '../adminApi';
import type { BackendInfo } from '../adminApi';
import { normalizeUiError } from '../errorHandling';

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

  const handleCreate = async () => {
    const trimmed = name.trim();
    if (!trimmed) return;
    setCreating(true);
    try {
      // Only pass an explicit backend when there's a real choice to make.
      const explicitBackend = showPicker ? selectedBackend : undefined;
      await createBucket(trimmed, explicitBackend);
      messageApi.success(`Bucket "${trimmed}" created`);
      setName('');
      setSelectedBackend(undefined);
      onClose();
      onCreated(trimmed);
    } catch (e: unknown) {
      const msg = normalizeUiError(e, "Unknown error");
      messageApi.error(`Failed to create bucket: ${msg}`);
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
        okButtonProps={{ disabled: !name.trim() || (showPicker && !selectedBackend) }}
        onCancel={() => {
          if (creating) return;
          setName('');
          setSelectedBackend(undefined);
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
          // Normalize to a valid S3 bucket name as you type: lowercase, and only
          // [a-z0-9.-]. S3 backends reject uppercase ("InvalidBucketName"), and a
          // filesystem backend would accept it inconsistently — so we keep the
          // name portable across backends. Mirrors BucketCard's name field.
          onChange={(e) => setName(e.target.value.toLowerCase().replace(/[^a-z0-9.-]/g, ''))}
          onPressEnter={handleCreate}
          style={{ fontFamily: 'var(--font-mono)' }}
        />
        {showPicker && (
          <div style={{ marginTop: 12 }} data-testid="bucket-backend-select">
            <Select
              aria-label="Backend"
              value={selectedBackend}
              onChange={(value) => setSelectedBackend(value || undefined)}
              options={backends.map((backend) => ({
                value: backend.name,
                label: backend.name === defaultBackend ? `${backend.name} (default)` : backend.name,
                sublabel: backend.backend_type,
              }))}
              optionRender={(opt) => (
                <div>
                  <div>{opt.data.label}</div>
                  {opt.data.sublabel && (
                    <div style={{ fontSize: 11, opacity: 0.65 }}>{opt.data.sublabel}</div>
                  )}
                </div>
              )}
              showSearch
              optionFilterProp="label"
              placeholder="Choose backend"
              style={{ width: '100%' }}
              size="middle"
            />
            <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 6 }}>
              Created on the selected backend. Changing it later requires migrating data.
            </Text>
          </div>
        )}
      </Modal>
    </>
  );
}
