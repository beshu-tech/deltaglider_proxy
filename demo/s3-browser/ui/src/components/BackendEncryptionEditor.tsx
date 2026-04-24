/**
 * BackendEncryptionEditor — per-backend encryption subsection inside
 * each card on BackendsPanel.
 *
 * Wraps four mode choices ({none, aes256-gcm-proxy, sse-kms, sse-s3})
 * + the proxy-AES key-generation flow (via `aesKeyGen`). On Apply,
 * constructs a targeted `storage` section PUT that replaces ONLY
 * this backend's `encryption` block and leaves every sibling backend
 * and every non-encryption field on this backend untouched.
 *
 * Layer isolation: the editor knows nothing about other backends.
 * The parent BackendsPanel hands it the current summary + a save
 * callback that composes the section-PUT body from the pending
 * change + the rest of the backends list. This keeps the editor
 * testable in isolation and prevents it from accidentally stomping
 * sibling config.
 */
import { useState } from 'react';
import { Alert, Button, Checkbox, Input, Space, Typography, message } from 'antd';
import {
  LockOutlined,
  SafetyOutlined,
  WarningOutlined,
  CheckCircleOutlined,
  ReloadOutlined,
} from '@ant-design/icons';
import type { BackendEncryptionSummary, BackendEncryptionMode } from '../adminApi';
import SimpleSelect from './SimpleSelect';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import { generateAesKeyHex } from '../aesKeyGen';

const { Text } = Typography;

/**
 * Wire shape of one backend's encryption block as it appears inside
 * a section-PUT body under `storage.backend_encryption` (singleton
 * path) or `storage.backends[i].encryption` (list path).
 *
 * Optional fields match the server's `BackendEncryptionConfig` enum —
 * the operator sends only the fields their chosen mode uses; the
 * server validates via `Config::check`.
 */
export interface BackendEncryptionPatch {
  mode: BackendEncryptionMode;
  key?: string;
  key_id?: string;
  kms_key_id?: string;
  bucket_key_enabled?: boolean;
  legacy_key?: string | null;
  legacy_key_id?: string | null;
}

interface Props {
  /** Backend name — used only for the env-var hint. */
  backendName: string;
  /** Current server-side summary (non-secret only). */
  current: BackendEncryptionSummary;
  /**
   * Called when the operator clicks Apply. Parent is responsible
   * for: (1) composing the full section-PUT body, (2) running the
   * validate+confirm flow if desired, (3) persisting, and (4)
   * refreshing the summary after success.
   *
   * Returns a Promise that resolves on success. Editor uses the
   * resolution to clear its pending-change state.
   */
  onApply: (patch: BackendEncryptionPatch) => Promise<void>;
}

export default function BackendEncryptionEditor({ backendName, current, onApply }: Props) {
  const colors = useColors();
  const { cardStyle, inputRadius } = useCardStyles();

  // Pending edit state. `null` = no pending change (summary is the
  // authoritative view). Non-null = operator is staging something.
  const [pending, setPending] = useState<BackendEncryptionPatch | null>(null);
  // Proxy-AES key generation UX: show the generated hex ONCE, gate
  // Apply behind a "stored safely" checkbox so the operator can't
  // accidentally click through without saving it off-box.
  const [pendingKey, setPendingKey] = useState<string | null>(null);
  const [storedSafelyChecked, setStoredSafelyChecked] = useState(false);
  const [applying, setApplying] = useState(false);

  const tone = current.mode !== 'none' ? colors.ACCENT_GREEN : colors.TEXT_MUTED;

  // Mode options shown in the dropdown. SSE-KMS/SSE-S3 are S3-only,
  // but the editor doesn't know the backend type here — the server
  // rejects native modes on filesystem via Config::check and the
  // resulting warning surfaces in the Apply dialog. Keeping the
  // dropdown uniform keeps the UI shape predictable; the error
  // path is always specific.
  const modeOptions: Array<{ value: BackendEncryptionMode; label: string }> = [
    { value: 'none', label: 'None (plaintext)' },
    { value: 'aes256-gcm-proxy', label: 'AES-256-GCM (proxy-side)' },
    { value: 'sse-kms', label: 'SSE-KMS (AWS KMS)' },
    { value: 'sse-s3', label: 'SSE-S3 (AWS-managed AES256)' },
  ];

  const startEdit = (mode: BackendEncryptionMode) => {
    if (mode === 'aes256-gcm-proxy') {
      // Generate a fresh key up front. The operator confirms
      // they've stored it before we allow Apply.
      const key = generateAesKeyHex();
      setPendingKey(key);
      setStoredSafelyChecked(false);
      setPending({ mode, key });
    } else if (mode === 'sse-kms') {
      setPendingKey(null);
      setStoredSafelyChecked(false);
      setPending({ mode, kms_key_id: '', bucket_key_enabled: true });
    } else if (mode === 'sse-s3') {
      setPendingKey(null);
      setStoredSafelyChecked(false);
      setPending({ mode });
    } else {
      // none → disable. No further input needed.
      setPendingKey(null);
      setStoredSafelyChecked(false);
      setPending({ mode: 'none' });
    }
  };

  const cancelEdit = () => {
    setPending(null);
    setPendingKey(null);
    setStoredSafelyChecked(false);
  };

  const copyKey = async () => {
    if (!pendingKey) return;
    try {
      await navigator.clipboard.writeText(pendingKey);
      message.success('Key copied to clipboard');
    } catch {
      message.error('Copy failed — select and copy manually');
    }
  };

  const canApply = (): boolean => {
    if (!pending) return false;
    switch (pending.mode) {
      case 'aes256-gcm-proxy':
        // Key must be set AND operator must confirm they saved it.
        return Boolean(pending.key) && storedSafelyChecked;
      case 'sse-kms':
        return Boolean(pending.kms_key_id && pending.kms_key_id.trim());
      case 'sse-s3':
      case 'none':
        return true;
    }
  };

  const doApply = async () => {
    if (!pending || !canApply()) return;
    setApplying(true);
    try {
      await onApply(pending);
      cancelEdit();
    } catch {
      // Parent already surfaces the error message; editor just
      // stays in the current state so the operator can retry.
    } finally {
      setApplying(false);
    }
  };

  // Header row: current status + top-level action.
  const statusLine = (() => {
    if (current.mode === 'none') {
      return (
        <Text style={{ fontSize: 13, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
          Encryption: <strong style={{ color: colors.TEXT_MUTED }}>DISABLED</strong>
        </Text>
      );
    }
    const modeLabel =
      current.mode === 'aes256-gcm-proxy'
        ? 'AES-256-GCM (proxy)'
        : current.mode === 'sse-kms'
          ? 'SSE-KMS'
          : 'SSE-S3';
    const detail =
      current.mode === 'aes256-gcm-proxy' && current.key_id
        ? `key id ${current.key_id}`
        : current.mode === 'sse-kms' && current.kms_key_id
          ? current.kms_key_id
          : null;
    return (
      <Text style={{ fontSize: 13, fontFamily: 'var(--font-ui)' }}>
        Encryption: <strong style={{ color: tone }}>{modeLabel}</strong>
        {detail && (
          <span style={{ color: colors.TEXT_MUTED, marginLeft: 6, fontFamily: 'var(--font-mono)', fontSize: 12 }}>
            ({detail})
          </span>
        )}
      </Text>
    );
  })();

  return (
    <div
      style={{
        ...cardStyle,
        marginTop: 8,
        padding: '10px 12px',
        background: colors.BG_ELEVATED,
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <LockOutlined style={{ fontSize: 14, color: tone }} />
          {statusLine}
        </div>
        {!pending && (
          <Space>
            {current.mode === 'aes256-gcm-proxy' && (
              <Button
                size="small"
                icon={<ReloadOutlined />}
                onClick={() => startEdit('aes256-gcm-proxy')}
              >
                Rotate key
              </Button>
            )}
            <SimpleSelect
              value={current.mode}
              onChange={(v) => startEdit(v as BackendEncryptionMode)}
              options={modeOptions}
              size="small"
              style={{ width: 220 }}
            />
          </Space>
        )}
      </div>

      {/* Shim banner. Info-level: the shim is an intentional
         transition state, not an error. Points the operator at the
         follow-up action (clear legacy_key when objects are gone). */}
      {current.shim_active && !pending && (
        <Alert
          type="info"
          showIcon
          style={{ marginTop: 8, borderRadius: 6, fontSize: 12 }}
          message="Decrypt-only shim active"
          description={
            <span>
              A legacy key is configured on this backend. Historical objects
              stamped with that key still decrypt; new writes use the current
              mode. Clear <code>legacy_key</code> once all legacy-stamped
              objects have been re-written or deleted.
            </span>
          }
        />
      )}

      {/* Per-mode edit surface. Renders only when `pending` is set. */}
      {pending && (
        <div style={{ marginTop: 10 }}>
          {pending.mode === 'none' && (
            <Alert
              type="warning"
              showIcon
              icon={<WarningOutlined />}
              style={{ borderRadius: 6, fontSize: 12 }}
              message="Disabling encryption"
              description={
                <span>
                  New writes to backend <code>{backendName}</code> will go to disk as plaintext.
                  Historical encrypted objects stay readable as long as a key is configured; if
                  you later remove the key entirely, those reads will fail rather than silently
                  returning ciphertext.
                </span>
              }
            />
          )}

          {pending.mode === 'aes256-gcm-proxy' && pendingKey && (
            <>
              <Alert
                type="error"
                showIcon
                icon={<WarningOutlined />}
                style={{ borderRadius: 6, fontSize: 12, marginBottom: 8 }}
                message="If you lose this key, encrypted objects on this backend are unrecoverable."
                description="DeltaGlider does not back up your encryption key. Copy it to a password manager / secrets vault BEFORE clicking Apply."
              />
              <span style={{ fontSize: 11, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
                Generated key (64 hex chars, shown ONCE)
              </span>
              <Input.TextArea
                value={pendingKey}
                readOnly
                autoSize={{ minRows: 2, maxRows: 2 }}
                style={{
                  ...inputRadius,
                  marginTop: 4,
                  fontFamily: 'var(--font-mono)',
                  fontSize: 11,
                  letterSpacing: 0.5,
                }}
              />
              <Space style={{ marginTop: 8 }}>
                <Button size="small" onClick={copyKey}>
                  Copy to clipboard
                </Button>
                <Button
                  size="small"
                  icon={<SafetyOutlined />}
                  onClick={() => {
                    const k = generateAesKeyHex();
                    setPendingKey(k);
                    setPending({ mode: 'aes256-gcm-proxy', key: k });
                    setStoredSafelyChecked(false);
                  }}
                >
                  Re-generate
                </Button>
              </Space>
              <div style={{ marginTop: 8 }}>
                <Checkbox
                  checked={storedSafelyChecked}
                  onChange={(e) => setStoredSafelyChecked(e.target.checked)}
                >
                  <Text style={{ fontSize: 12 }}>
                    I have stored this key safely. I understand that losing it makes encrypted
                    objects on backend <code>{backendName}</code> unrecoverable.
                  </Text>
                </Checkbox>
              </div>
              {storedSafelyChecked && (
                <Alert
                  type="info"
                  showIcon
                  icon={<CheckCircleOutlined />}
                  style={{ marginTop: 8, borderRadius: 6, fontSize: 12 }}
                  message="Ready to apply."
                />
              )}
            </>
          )}

          {pending.mode === 'sse-kms' && (
            <>
              <span style={{ fontSize: 11, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
                KMS key ARN or alias
              </span>
              <Input
                value={pending.kms_key_id ?? ''}
                onChange={(e) =>
                  setPending({ ...pending, kms_key_id: e.target.value })
                }
                placeholder="arn:aws:kms:us-east-1:123456789012:key/abcd-efgh"
                style={{
                  ...inputRadius,
                  marginTop: 4,
                  fontFamily: 'var(--font-mono)',
                  fontSize: 12,
                }}
              />
              <div style={{ marginTop: 8 }}>
                <Checkbox
                  checked={pending.bucket_key_enabled ?? true}
                  onChange={(e) =>
                    setPending({ ...pending, bucket_key_enabled: e.target.checked })
                  }
                >
                  <Text style={{ fontSize: 12 }}>
                    Enable S3 bucket keys (reduces KMS cost on bursty traffic)
                  </Text>
                </Checkbox>
              </div>
            </>
          )}

          {pending.mode === 'sse-s3' && (
            <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
              AWS will encrypt objects with its own AES256 keys. No additional configuration.
            </Text>
          )}

          <Space style={{ marginTop: 12 }}>
            <Button size="small" onClick={cancelEdit} disabled={applying}>
              Cancel
            </Button>
            <Button
              size="small"
              type="primary"
              loading={applying}
              disabled={!canApply() || applying}
              onClick={doApply}
            >
              Apply
            </Button>
          </Space>
        </div>
      )}
    </div>
  );
}
