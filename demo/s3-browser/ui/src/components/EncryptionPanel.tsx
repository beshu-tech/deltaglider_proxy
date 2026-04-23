/**
 * EncryptionPanel — at-rest encryption configuration.
 *
 * Lives under Configuration → Storage → Encryption (it's a global
 * storage-level concern, not per-bucket). The operator flows are:
 *
 *   1. Enable from scratch:
 *      - Click "Generate new key" — the UI generates 32 random bytes
 *        client-side via `crypto.getRandomValues` and shows the hex
 *        ONCE. The key never round-trips through the server pre-
 *        apply — the backend only sees it when the operator clicks
 *        Apply.
 *      - Operator copies the key to off-box storage.
 *      - Checks "I have stored this key safely" — this gates Apply.
 *      - Clicks Apply → the key lands in `advanced.encryption_key`
 *        via the section-PUT endpoint.
 *
 *   2. Rotate when already enabled: same flow + an amber banner
 *      warning that rotation makes old-key objects unreadable (no
 *      multi-key tracking in this release — Phase 3d/e territory).
 *
 *   3. Disable: clears `encryption_key` to null, future writes
 *      bypass the wrapper. Existing encrypted objects become
 *      unreadable until encryption is re-enabled with the same key.
 *
 * Status is derived from `GET /config/section/advanced` — the server
 * redacts the actual key (returns `null`) but exposes a dedicated
 * `encryption_enabled` boolean; we trust that. The panel NEVER
 * shows any part of the key post-generation.
 *
 * Uses `useSectionEditor` for fetch/dirty/apply plumbing, matching
 * the shape of every other section panel (CredentialsModePanel,
 * advancedPanels, etc.). The key-generation + "stored safely"
 * checkbox state is local-only (not persisted), since the key gets
 * wiped after Apply succeeds.
 */
import { useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Checkbox,
  Input,
  Space,
  Typography,
  message,
} from 'antd';
import {
  LockOutlined,
  SafetyOutlined,
  WarningOutlined,
  CheckCircleOutlined,
  ReloadOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import { useSectionEditor } from '../useSectionEditor';
import { getAdminConfig } from '../adminApi';
import SectionHeader from './SectionHeader';
import ApplyDialog from './ApplyDialog';

const { Text } = Typography;

/**
 * Subset of AdvancedSectionBody this panel owns. The section PUT path
 * uses merge-patch, so sibling fields (listen_addr, log_level, etc.)
 * are preserved untouched.
 */
interface AdvancedEncryptionBody {
  encryption_key?: string | null;
}

/**
 * Generate a fresh 32-byte (256-bit) AES key as 64 hex chars using
 * the browser's CSPRNG. Never sent to the server before the operator
 * has confirmed they've stored it; never logged; never persisted in
 * React state beyond this panel's lifetime.
 */
function generateAesKeyHex(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

interface Props {
  onSessionExpired?: () => void;
}

export default function EncryptionPanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const colors = useColors();

  // Section editor handles the fetch → dirty → apply lifecycle.
  // `value.encryption_key` reflects what the operator is about to
  // apply; we set it to the generated hex (or null for disable) and
  // leave the rest of the advanced section alone via merge-patch.
  const {
    setValue: setForm,
    discard,
    isDirty,
    loading,
    error,
    applyOpen,
    applyResponse,
    applying,
    runApply,
    cancelApply,
    confirmApply,
  } = useSectionEditor<AdvancedEncryptionBody>({
    section: 'advanced',
    initial: { encryption_key: undefined },
    onSessionExpired,
    noun: 'encryption',
    // The server returns `encryption_key: null` on GET (redacted) when
    // encryption is enabled — map that to `undefined` so the form
    // doesn't look dirty on mount. We track server-side enablement via
    // a separate indicator below.
    pick: () => ({ encryption_key: undefined }),
  });

  // Client-only state for the key-generation flow.
  const [pendingKey, setPendingKey] = useState<string | null>(null);
  const [storedSafelyChecked, setStoredSafelyChecked] = useState(false);
  const [mode, setMode] = useState<'idle' | 'generating' | 'disabling'>('idle');

  // Server-side encryption status, fetched from GET /api/admin/config.
  // The `encryption_enabled` field is a boolean-only indicator — no
  // key material ever crosses the wire. `null` = still loading.
  const [serverEnabled, setServerEnabled] = useState<boolean | null>(null);
  // Bump to force a re-fetch after Apply succeeds.
  const [refreshTick, setRefreshTick] = useState(0);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const cfg = await getAdminConfig();
        if (!cancelled) {
          setServerEnabled(cfg?.encryption_enabled ?? false);
        }
      } catch {
        if (!cancelled) setServerEnabled(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [refreshTick]);

  // ── Actions ──

  /** Start the enable/rotate flow: generate a key, show it ONCE. */
  const startGenerate = () => {
    setPendingKey(generateAesKeyHex());
    setStoredSafelyChecked(false);
    setMode('generating');
    // Don't set the form value yet — we wait for "stored safely" confirmation.
  };

  const cancelGenerate = () => {
    setPendingKey(null);
    setStoredSafelyChecked(false);
    setMode('idle');
    setForm({ encryption_key: undefined });
  };

  /** Operator confirmed they saved the key → stage it for Apply. */
  const commitGeneratedKeyToForm = () => {
    if (!pendingKey) return;
    setForm({ encryption_key: pendingKey });
  };

  /** Start the disable flow: stage `encryption_key: null`. */
  const startDisable = () => {
    setMode('disabling');
    setForm({ encryption_key: null });
  };

  const cancelDisable = () => {
    setMode('idle');
    setForm({ encryption_key: undefined });
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

  // Wrap the hook's confirmApply to also clear local key-gen state
  // on success.
  const confirmAndClear = async () => {
    await confirmApply();
    setPendingKey(null);
    setStoredSafelyChecked(false);
    setMode('idle');
    // Re-read server state after successful apply.
    setRefreshTick((n) => n + 1);
  };

  if (error) {
    return <Alert type="error" showIcon message="Failed to load" description={error} />;
  }

  if (loading) {
    return (
      <div style={{ padding: 48, textAlign: 'center' }}>
        <Text type="secondary">Loading encryption configuration...</Text>
      </div>
    );
  }

  const enabled = serverEnabled === true;

  return (
    <div
      style={{
        maxWidth: 740,
        margin: '0 auto',
        padding: 'clamp(16px, 3vw, 24px)',
        display: 'flex',
        flexDirection: 'column',
        gap: 16,
      }}
    >
      {/* Unsaved-changes banner (appears when a key has been generated and
         confirmed, or when disable has been staged). */}
      {isDirty && (
        <Alert
          type="warning"
          showIcon
          message="Unsaved encryption change"
          description="Review the diff in the Apply dialog before persisting."
          action={
            <Space>
              <Button size="small" onClick={() => { discard(); cancelGenerate(); cancelDisable(); }} disabled={applying}>
                Discard
              </Button>
              <Button
                type="primary"
                size="small"
                onClick={runApply}
                disabled={applying}
                loading={applying}
              >
                Apply
              </Button>
            </Space>
          }
        />
      )}

      {/* Red loss-of-key warning — ALWAYS shown. */}
      <Alert
        type="error"
        showIcon
        icon={<WarningOutlined />}
        message="If you lose this key, encrypted objects are unrecoverable."
        description="Store the key off-box (password manager, vault, HSM). DeltaGlider does not back up the key for you — the encrypted config export is encrypted with the same key and won't help."
        style={{ borderRadius: 8 }}
      />

      {/* Status card */}
      <div style={cardStyle}>
        <SectionHeader
          icon={<LockOutlined />}
          title="Encryption at rest"
          description="AES-256-GCM applied to every object before it's written to the storage backend. Delta compression happens first, so compression ratios stay intact."
        />
        <div
          style={{
            marginTop: 16,
            display: 'flex',
            alignItems: 'center',
            gap: 12,
          }}
        >
          <div
            style={{
              width: 10,
              height: 10,
              borderRadius: '50%',
              background: enabled ? colors.ACCENT_GREEN : colors.TEXT_MUTED,
            }}
          />
          <Text style={{ fontWeight: 600, fontFamily: 'var(--font-ui)' }}>
            {enabled ? 'ENABLED (AES-256-GCM)' : 'DISABLED'}
          </Text>
        </div>
        <div style={{ marginTop: 20, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          {!enabled && mode === 'idle' && (
            <Button
              type="primary"
              icon={<SafetyOutlined />}
              onClick={startGenerate}
            >
              Enable encryption
            </Button>
          )}
          {enabled && mode === 'idle' && (
            <>
              <Button
                icon={<ReloadOutlined />}
                onClick={startGenerate}
              >
                Rotate key
              </Button>
              <Button danger onClick={startDisable}>
                Disable encryption
              </Button>
            </>
          )}
        </div>
      </div>

      {/* Rotation-specific warning */}
      {enabled && mode === 'generating' && (
        <Alert
          type="warning"
          showIcon
          message="Rotating the key makes existing encrypted objects UNREADABLE until the old key is restored."
          description="DeltaGlider does not track which key encrypted which object — this release does not support multi-key rotation. If you rotate, keep the OLD key alongside the new one."
          style={{ borderRadius: 8 }}
        />
      )}

      {/* Enablement info banner */}
      {!enabled && mode === 'generating' && (
        <Alert
          type="info"
          showIcon
          message="Enabling encryption does NOT re-encrypt existing objects."
          description="Only new writes from this moment forward are encrypted. Objects written before enabling stay in plaintext on disk. Flip encryption on BEFORE storing any sensitive data."
          style={{ borderRadius: 8 }}
        />
      )}

      {/* Key-generation card */}
      {mode === 'generating' && pendingKey && (
        <div style={cardStyle}>
          <SectionHeader
            icon={<SafetyOutlined />}
            title={enabled ? 'New key' : 'Generated key'}
            description="This 256-bit key was generated in your browser with crypto.getRandomValues. It is shown ONCE and never round-trips through the server until you click Apply. Save it somewhere safe BEFORE proceeding."
          />
          <div style={{ marginTop: 16, display: 'flex', flexDirection: 'column', gap: 12 }}>
            <Input.TextArea
              value={pendingKey}
              readOnly
              autoSize={{ minRows: 2, maxRows: 2 }}
              style={{
                ...inputRadius,
                fontFamily: 'var(--font-mono)',
                fontSize: 12,
                letterSpacing: 0.5,
              }}
            />
            <Space>
              <Button onClick={copyKey}>Copy to clipboard</Button>
              <Button onClick={startGenerate}>Re-generate</Button>
            </Space>
            <Checkbox
              checked={storedSafelyChecked}
              onChange={(e) => {
                const v = e.target.checked;
                setStoredSafelyChecked(v);
                if (v) {
                  commitGeneratedKeyToForm();
                } else {
                  // Back out of staging if they uncheck.
                  setForm({ encryption_key: undefined });
                }
              }}
            >
              <Text style={{ fontSize: 13 }}>
                I have stored this key safely. I understand that losing it makes
                encrypted objects unrecoverable.
              </Text>
            </Checkbox>
            {storedSafelyChecked && !isDirty && (
              <Alert
                type="info"
                showIcon
                icon={<CheckCircleOutlined />}
                message="Ready to apply. Click Apply above to persist the key."
                style={{ borderRadius: 8 }}
              />
            )}
          </div>
        </div>
      )}

      {/* Disable confirmation card */}
      {mode === 'disabling' && (
        <div style={cardStyle}>
          <SectionHeader
            icon={<WarningOutlined />}
            title="Disable encryption"
            description="Click Apply above to clear the encryption key. Future writes will be stored in plaintext. Existing encrypted objects will remain encrypted on disk — re-enable with the SAME key later to read them again."
          />
          <div style={{ marginTop: 16, display: 'flex', gap: 8 }}>
            <Button onClick={cancelDisable}>Cancel</Button>
          </div>
        </div>
      )}

      <ApplyDialog
        open={applyOpen}
        section="advanced"
        response={applyResponse}
        onApply={() => {
          void confirmAndClear();
        }}
        onCancel={cancelApply}
        loading={applying}
      />
    </div>
  );
}
