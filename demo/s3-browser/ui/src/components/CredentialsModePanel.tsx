/**
 * CredentialsModePanel — the "Credentials & mode" sub-page under
 * Configuration → Access (Wave 5 of the admin UI revamp plan).
 *
 * This is the FIRST thing on the Access section. It carries the
 * architecturally-important decisions for the whole section:
 *
 *   1. **IAM mode** (the architectural choice) — GUI (DB is the
 *      source of truth) vs Declarative (YAML is authoritative;
 *      admin-API IAM mutations return 403). Flipping to
 *      declarative is destructive — the operator loses the
 *      ability to add/edit/delete users through the GUI until the
 *      Phase 3c.3 reconciler ships. Requires confirmation.
 *
 *   2. **S3 authentication mode** — auto-detect from credentials
 *      (default) vs open-access (`"none"`, dev-only). Picks the
 *      SigV4 middleware's behaviour.
 *
 *   3. **Bootstrap SigV4 credentials** — the access_key_id +
 *      secret_access_key that exist before any IAM users are
 *      created. Still the fallback for legacy clients / scripts.
 *
 *   4. **Admin GUI password change** — delegated to the existing
 *      PasswordChangeCard (separate endpoint, separate security
 *      model; re-encrypts the config DB atomically).
 *
 * Uses the section-level API (`putSection('access', ...)`) for
 * persistence so the merge-patch + runtime-secret-preservation
 * plumbing applies. Dirty state + ApplyDialog + AdminPage's
 * sidebar-dot coordination all free from `useDirtySection`.
 */
import { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Input,
  Modal,
  Radio,
  Space,
  Typography,
  message,
} from 'antd';
import {
  ExclamationCircleOutlined,
  KeyOutlined,
  LockOutlined,
  SafetyOutlined,
  TeamOutlined,
} from '@ant-design/icons';
import type { IamMode, SectionApplyResponse } from '../adminApi';
import {
  getSection,
  putSection,
  validateSection,
} from '../adminApi';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import { useDirtySection } from '../useDirtySection';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import ApplyDialog from './ApplyDialog';
import PasswordChangeCard from './PasswordChangeCard';

const { Text } = Typography;

/** The AccessSection wire shape, matching the backend. Every field
 *  is optional — the server omits defaults on GET. */
interface AccessSectionBody {
  iam_mode?: IamMode;
  authentication?: string | null;
  access_key_id?: string | null;
  secret_access_key?: string | null;
}

/** Default form state — used when the server returns a bare `{}`. */
const EMPTY_ACCESS: AccessSectionBody = {
  iam_mode: 'gui',
  authentication: undefined,
  access_key_id: undefined,
  secret_access_key: undefined,
};

type AuthMode = 'auto' | 'none';

interface Props {
  onSessionExpired?: () => void;
}

export default function CredentialsModePanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const colors = useColors();

  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const {
    value: form,
    isDirty,
    setValue: setForm,
    discard,
    markApplied,
    resetWith,
  } = useDirtySection<AccessSectionBody>('access', EMPTY_ACCESS);

  // Apply-dialog state. Same pattern as AdmissionPanel — snapshot
  // the body at validate time (§F5 fix) so the diff and the
  // subsequent PUT refer to the same payload even if the operator
  // keeps interacting with the form underneath the modal.
  const [applyOpen, setApplyOpen] = useState(false);
  const [applyResponse, setApplyResponse] = useState<SectionApplyResponse | null>(null);
  const [pendingBody, setPendingBody] = useState<AccessSectionBody | null>(null);
  const [applying, setApplying] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setLoading(true);
      const body = await getSection<AccessSectionBody>('access');
      // Merge missing fields from EMPTY_ACCESS so the form controllers
      // always see a known shape. The server returns e.g. `{}` on a
      // fresh install when every access field is at its default.
      resetWith({ ...EMPTY_ACCESS, ...body });
      setError(null);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(
        `Failed to load access section: ${e instanceof Error ? e.message : 'unknown'}`
      );
    } finally {
      setLoading(false);
    }
  }, [resetWith, onSessionExpired]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Derive the auth-mode radio from the `authentication` string. The
  // server stores it as `Option<String>` — absent / `null` means
  // auto-detect; the literal `"none"` means open access.
  const authMode: AuthMode = form.authentication === 'none' ? 'none' : 'auto';

  // ── Mutators ───────────────────────────────────────────

  const setIamMode = (next: IamMode) => {
    if (next === form.iam_mode) return;
    if (next === 'declarative') {
      // Flip to declarative is destructive. Confirm.
      Modal.confirm({
        title: 'Switch to Declarative IAM mode?',
        icon: <ExclamationCircleOutlined />,
        width: 560,
        content: (
          <div style={{ fontSize: 13, lineHeight: 1.6 }}>
            <p>
              In <code>declarative</code> mode the YAML config is the
              source of truth for users, groups, and OAuth providers.
              The admin API returns <b>403</b> for every IAM mutation
              route — you can no longer create or edit users through
              this GUI.
            </p>
            <p style={{ marginTop: 8 }}>
              The reconciler that sync-diffs the encrypted IAM database
              to your YAML lands in Phase 3c.3. Until then,{' '}
              <b>declarative mode is effectively a lockout</b> — the
              DB stays as it was, but the GUI and admin API cannot
              edit it.
            </p>
            <p style={{ marginTop: 8 }}>
              Flip back to <code>gui</code> any time to restore mutation
              access. This is a local form change only — nothing
              persists until you click Apply.
            </p>
          </div>
        ),
        okText: 'Switch to Declarative',
        okButtonProps: { danger: true },
        cancelText: 'Cancel',
        onOk: () => setForm({ ...form, iam_mode: 'declarative' }),
      });
      return;
    }
    setForm({ ...form, iam_mode: next });
  };

  const setAuthMode = (next: AuthMode) => {
    setForm({
      ...form,
      // `undefined` means "absent from the body" which the server
      // interprets as auto-detect. `"none"` is the explicit
      // open-access opt-in.
      authentication: next === 'none' ? 'none' : undefined,
    });
  };

  const setAccessKey = (v: string) => setForm({ ...form, access_key_id: v || undefined });
  const setSecretKey = (v: string) => setForm({ ...form, secret_access_key: v || undefined });

  // ── Apply flow ─────────────────────────────────────────

  const runApply = async () => {
    // Snapshot the body at validate time. The apply dialog's diff
    // and the subsequent PUT both refer to THIS object, even if the
    // operator keeps typing underneath the modal (§F5).
    const snapshot: AccessSectionBody = {
      iam_mode: form.iam_mode,
      authentication: form.authentication,
      access_key_id: form.access_key_id,
      secret_access_key: form.secret_access_key,
    };
    try {
      const resp = await validateSection<AccessSectionBody>('access', snapshot);
      setApplyResponse(resp);
      setPendingBody(snapshot);
      setApplyOpen(true);
    } catch (e) {
      message.error(
        `Validate failed: ${e instanceof Error ? e.message : 'unknown'}`
      );
    }
  };

  const cancelApply = () => {
    setApplyOpen(false);
    setPendingBody(null);
  };

  const confirmApply = async () => {
    if (!pendingBody) return;
    setApplying(true);
    try {
      const resp = await putSection<AccessSectionBody>('access', pendingBody);
      if (!resp.ok) {
        message.error(resp.error || 'Apply failed');
        return;
      }
      message.success(
        resp.persisted_path
          ? `Applied + persisted to ${resp.persisted_path}`
          : 'Applied'
      );
      markApplied();
      setApplyOpen(false);
      setPendingBody(null);
      void refresh();
    } catch (e) {
      message.error(
        `Apply failed: ${e instanceof Error ? e.message : 'unknown'}`
      );
      setApplyOpen(false);
      setPendingBody(null);
      void refresh();
    } finally {
      setApplying(false);
    }
  };

  if (error) {
    return <Alert type="error" showIcon message="Failed to load" description={error} />;
  }
  if (loading) {
    return (
      <div style={{ padding: 48, textAlign: 'center' }}>
        <Text type="secondary">Loading access configuration...</Text>
      </div>
    );
  }

  const declarativeActive = form.iam_mode === 'declarative';

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
      {/* Dirty-state banner with Apply / Discard */}
      {isDirty && (
        <Alert
          type="warning"
          showIcon
          message="Unsaved changes to this section"
          description="Review the diff in the Apply dialog before persisting."
          action={
            <Space>
              <Button size="small" onClick={discard} disabled={applying}>
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

      {/* IAM mode card — the architectural choice */}
      <div style={cardStyle}>
        <SectionHeader
          icon={<TeamOutlined />}
          title="IAM mode"
          description="Who owns the user directory: this admin GUI + DB, or your YAML config?"
        />
        <div style={{ marginTop: 16 }}>
          <Radio.Group
            value={form.iam_mode ?? 'gui'}
            onChange={(e) => setIamMode(e.target.value as IamMode)}
            style={{ display: 'flex', flexDirection: 'column', gap: 10 }}
          >
            <Radio value="gui" style={{ alignItems: 'flex-start' }}>
              <div>
                <div style={{ fontWeight: 600 }}>GUI-managed (default)</div>
                <Text type="secondary" style={{ fontSize: 12, lineHeight: 1.5 }}>
                  The admin GUI and admin API write directly to the
                  encrypted IAM database. Runtime changes to YAML
                  <code style={{ margin: '0 4px' }}>access.users</code>
                  are ignored. Recommended for solo and GUI-first setups.
                </Text>
              </div>
            </Radio>
            <Radio value="declarative" style={{ alignItems: 'flex-start' }}>
              <div>
                <div style={{ fontWeight: 600 }}>Declarative (YAML-authoritative)</div>
                <Text type="secondary" style={{ fontSize: 12, lineHeight: 1.5 }}>
                  Your YAML document is the source of truth. Admin API
                  IAM mutation routes return 403. Recommended for GitOps
                  teams. <b>Heads up:</b> the reconciler that sync-diffs
                  DB ↔ YAML lands in Phase 3c.3 — today this is an
                  effective lockout until you flip back.
                </Text>
              </div>
            </Radio>
          </Radio.Group>
          {declarativeActive && (
            <Alert
              type="warning"
              showIcon
              icon={<LockOutlined />}
              style={{ marginTop: 12, borderRadius: 8 }}
              message="Declarative mode is active after Apply"
              description={
                <span style={{ fontSize: 12 }}>
                  The Users / Groups / External authentication panels
                  become read-only. Edit your YAML and Apply instead.
                </span>
              }
            />
          )}
        </div>
      </div>

      {/* S3 authentication mode */}
      <div style={cardStyle}>
        <SectionHeader
          icon={<SafetyOutlined />}
          title="S3 authentication mode"
          description="Whether clients must sign their requests with SigV4."
        />
        <div style={{ marginTop: 16 }}>
          <Radio.Group
            value={authMode}
            onChange={(e) => setAuthMode(e.target.value as AuthMode)}
            style={{ display: 'flex', flexDirection: 'column', gap: 10 }}
          >
            <Radio value="auto" style={{ alignItems: 'flex-start' }}>
              <div>
                <div style={{ fontWeight: 600 }}>Auto-detect (recommended)</div>
                <Text type="secondary" style={{ fontSize: 12 }}>
                  Authentication is required when credentials are set
                  (IAM users or bootstrap SigV4 pair). Leaving the
                  credentials empty turns auth off.
                </Text>
              </div>
            </Radio>
            <Radio value="none" style={{ alignItems: 'flex-start' }}>
              <div>
                <div style={{ fontWeight: 600 }}>
                  Open access <span style={{ color: colors.ACCENT_AMBER, fontSize: 11 }}>(dev only)</span>
                </div>
                <Text type="secondary" style={{ fontSize: 12 }}>
                  Explicitly disable SigV4 verification for every
                  request. Equivalent to{' '}
                  <code style={{ fontFamily: 'var(--font-mono)' }}>
                    authentication: &quot;none&quot;
                  </code>{' '}
                  in YAML.
                </Text>
              </div>
            </Radio>
          </Radio.Group>
        </div>
      </div>

      {/* Bootstrap SigV4 credentials */}
      <div style={cardStyle}>
        <SectionHeader
          icon={<KeyOutlined />}
          title="Bootstrap SigV4 credentials"
          description="Used before any IAM users exist, and by legacy scripted clients. Not the same as the admin GUI password."
        />
        <div style={{ marginTop: 16, display: 'flex', flexDirection: 'column', gap: 12 }}>
          <FormField
            label="Access key ID"
            yamlPath="access.access_key_id"
            helpText="Clients use this key to sign S3 requests."
          >
            <Input
              value={form.access_key_id ?? ''}
              onChange={(e) => setAccessKey(e.target.value)}
              placeholder="AKIAIOSFODNN7EXAMPLE"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
            />
          </FormField>
          <FormField
            label="Secret access key"
            yamlPath="access.secret_access_key"
            helpText={
              form.access_key_id
                ? 'Leave empty to keep the current secret unchanged. Paste a new value to rotate.'
                : 'Shared secret paired with the access key.'
            }
          >
            {/* Guard against browser password-manager heuristics — a
                hidden field with the access key as "username" keeps
                autocomplete from latching onto this field. */}
            <input
              type="text"
              autoComplete="username"
              value={form.access_key_id ?? ''}
              readOnly
              aria-hidden="true"
              tabIndex={-1}
              style={{ display: 'none' }}
            />
            <Input.Password
              value={form.secret_access_key ?? ''}
              onChange={(e) => setSecretKey(e.target.value)}
              placeholder="Leave empty to keep current"
              autoComplete="new-password"
              style={{ ...inputRadius }}
            />
          </FormField>
          <Text type="secondary" style={{ fontSize: 11, marginTop: -4 }}>
            Redacted on GET — the section API preserves the current
            secret when this field is empty on PUT. Set both fields
            together to rotate, or both to empty to clear.
          </Text>
        </div>
      </div>

      {/* Admin GUI password change — separate endpoint, always
          available (distinct from S3 SigV4 auth). The PasswordChangeCard
          already carries its own SectionHeader; we just prepend a
          one-line contextual note so operators know this is the GUI
          password, not the bootstrap SigV4 secret above. */}
      <div style={{ marginTop: 4 }}>
        <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: 8 }}>
          <b>Note:</b> The admin GUI password is SEPARATE from the S3
          SigV4 credentials above. It unlocks this GUI and encrypts
          the IAM database.
        </Text>
        <PasswordChangeCard />
      </div>

      {/* Apply dialog */}
      <ApplyDialog
        open={applyOpen}
        section="access"
        response={applyResponse}
        onApply={confirmApply}
        onCancel={cancelApply}
        loading={applying}
      />
    </div>
  );
}
