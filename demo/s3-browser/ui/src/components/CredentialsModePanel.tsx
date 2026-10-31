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
import { useState } from 'react';
import {
  Alert,
  Button,
  Input,
  Modal,
  Radio,
  Typography,
} from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import {
  ExclamationCircleOutlined,
  KeyOutlined,
  LockOutlined,
  SafetyOutlined,
  TeamOutlined,
} from '@ant-design/icons';
import { removeBootstrapCredentials, type IamMode } from '../adminApi';
import { ApiError } from '../errorHandling';
import { plainError, type PlainError } from '../plainError';
import PlainErrorAlert from './PlainErrorAlert';
import { ErrorState, LoadingState } from './StatePlaceholders';
import { confirmDialog } from '../confirmDialog';
import { docsUrlToInAppHref } from '../linkifyDocUrl';
import { qk } from '../queries/keys';
import { useColors } from '../ThemeContext';
import { useCardStyles, contentColumn, CONTENT_FORM, CARD_GAP, SPACE } from './shared-styles';
import { useSectionEditor } from '../useSectionEditor';
import { useAdminConfig, useEnvOverride } from '../queries/config';
import { envOverrideHelp } from '../envOverrides';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import ApplyDialog from './ApplyDialog';
import StickyDirtyBar from './StickyDirtyBar';
import PasswordChangeCard from './PasswordChangeCard';
import MaskedSecretInput from './MaskedSecretInput';

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

/**
 * The PUT body: an empty key field means "keep the current value" (the
 * same rule as the blank secret), never "set it to the empty string".
 * Removal is the explicit "Remove bootstrap credentials" action.
 */
function accessPayload(v: AccessSectionBody): AccessSectionBody {
  const out = { ...v };
  if (!out.access_key_id) delete out.access_key_id;
  if (!out.secret_access_key) delete out.secret_access_key;
  return out;
}

interface Props {
  onSessionExpired?: () => void;
}

export default function CredentialsModePanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const colors = useColors();

  const {
    value: form,
    setValue: setForm,
    discard,
    isDirty,
    loading,
    loadError,
    refresh,
    applyOpen,
    applyResponse,
    applying,
    runApply,
    cancelApply,
    confirmApply,
  } = useSectionEditor<AccessSectionBody>({
    section: 'access',
    dirtyKey: 'access/credentials',
    initial: EMPTY_ACCESS,
    onSessionExpired,
    noun: 'access',
    toPayload: accessPayload,
  });
  const qc = useQueryClient();
  const [removing, setRemoving] = useState(false);
  const [removeError, setRemoveError] = useState<PlainError | null>(null);
  const [removeWarnings, setRemoveWarnings] = useState<string[]>([]);

  // The RUNNING auth state (env variables included), so the page never
  // implies auth is off while an env-provided key keeps SigV4 on.
  const { data: runtimeConfig } = useAdminConfig({ onSessionExpired });

  // Derive the auth-mode radio from the `authentication` string. The
  // server stores it as `Option<String>` — absent / `null` means
  // auto-detect; the literal `"none"` means open access.
  // DGP_AUTHENTICATION controls this field: the radio then shows the env
  // value read-only (an edit would be overridden by the environment).
  const authEnv = useEnvOverride('access.authentication');
  const authMode: AuthMode = (authEnv ? authEnv.value : form.authentication) === 'none' ? 'none' : 'auto';

  // ── Mutators ───────────────────────────────────────────
  // All write through `setForm((prev) => …)` so a burst of edits — or the
  // Modal.confirm onOk firing after the user edited another field while the
  // modal was open — always builds on the freshest state, never a stale
  // render snapshot (that snapshot staleness was the whole bug class).

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
              In <code>declarative</code> mode your YAML config becomes
              the source of truth for users, groups, and OAuth providers.
              The Users, Groups, and External authentication panels turn
              read-only — you manage everyone by editing your YAML config
              instead of clicking around this GUI.
            </p>
            <p style={{ marginTop: 8 }}>
              Each time you apply your config, DeltaGlider updates the
              user database to match it exactly — adding, changing, and
              removing users, groups, and providers as needed.
            </p>
            <p style={{ marginTop: 8 }}>
              Switch back to <code>gui</code> any time to edit users in
              the GUI again. This is just a form change for now — nothing
              takes effect until you click Apply.
            </p>
          </div>
        ),
        okText: 'Switch to Declarative',
        okButtonProps: { danger: true },
        cancelText: 'Cancel',
        onOk: () => setForm((prev) => ({ ...prev, iam_mode: 'declarative' })),
      });
      return;
    }
    setForm((prev) => ({ ...prev, iam_mode: next }));
  };

  const setAuthMode = (next: AuthMode) => {
    setForm((prev) => ({
      ...prev,
      // `undefined` means "absent from the body" which the server
      // interprets as auto-detect. `"none"` is the explicit
      // open-access opt-in.
      authentication: next === 'none' ? 'none' : undefined,
    }));
  };

  // The section GET redacts the access key id; the running config carries
  // it (it is not a secret). Show it, so an empty field never reads as
  // "no bootstrap key". Typing the same id back is no change.
  const runningKeyId = runtimeConfig?.access_key_id ?? undefined;
  // An env variable wins over the file: removing the file's pair would not
  // remove the key, so the action is not offered then.
  const accessKeyEnv = useEnvOverride('access.access_key_id');
  const accessKeyShown = form.access_key_id ?? runningKeyId ?? '';
  const setAccessKey = (v: string) =>
    setForm((prev) => ({ ...prev, access_key_id: v === runningKeyId ? undefined : v }));

  const removeBootstrap = async () => {
    const ok = await confirmDialog({
      title: 'Remove the bootstrap credentials?',
      content:
        'S3 clients that sign with this access key are refused from now on. IAM users keep working. ' +
        'The admin GUI password does not change.',
      okText: 'Remove',
      danger: true,
    });
    if (!ok) return;
    setRemoving(true);
    setRemoveError(null);
    setRemoveWarnings([]);
    try {
      const { warnings } = await removeBootstrapCredentials();
      setRemoveWarnings(warnings);
      await qc.invalidateQueries({ queryKey: qk.config() });
    } catch (e) {
      setRemoveError(
        e instanceof ApiError && (e.status === 404 || e.status === 405)
          ? { text: 'This proxy cannot remove the bootstrap credentials from the GUI. Remove access.access_key_id and access.secret_access_key from the YAML config instead.' }
          : plainError(e, 'The proxy could not remove the bootstrap credentials.'),
      );
    } finally {
      setRemoving(false);
    }
  };
  const setSecretKey = (v: string) => setForm((prev) => ({ ...prev, secret_access_key: v || undefined }));

  if (loadError) {
    return <ErrorState error={loadError} what="the access settings" onRetry={() => void refresh()} />;
  }
  if (loading) {
    return (
      <div style={contentColumn(CONTENT_FORM)}>
        <LoadingState variant="card" label="Loading the access settings" />
      </div>
    );
  }

  const declarativeActive = form.iam_mode === 'declarative';
  // The server refuses a gui → declarative flip whose YAML holds no IAM
  // (EMPTY_DECLARATIVE_FLIP, transition.rs): it would empty the user DB.
  // This page never sends users, so offer the flip only when the access
  // section already carries some.
  const yamlIam = form as Record<string, unknown>;
  const yamlHasIam = ['iam_users', 'iam_groups', 'auth_providers', 'group_mapping_rules'].some(
    (k) => Array.isArray(yamlIam[k]) && (yamlIam[k] as unknown[]).length > 0,
  );
  const declarativeBlocked = !declarativeActive && !yamlHasIam;

  return (
    <div
      style={{
        ...contentColumn(CONTENT_FORM),
        display: 'flex',
        flexDirection: 'column',
        gap: CARD_GAP,
      }}
    >
      {/* IAM mode card — the architectural choice */}
      <div style={cardStyle}>
        <SectionHeader
          icon={<TeamOutlined />}
          title="IAM mode"
          description="Where users, groups and providers are managed."
        />
        <div>
          <Radio.Group
            value={form.iam_mode ?? 'gui'}
            onChange={(e) => setIamMode(e.target.value as IamMode)}
            style={{ display: 'flex', flexDirection: 'column', gap: SPACE.sm }}
          >
            <Radio value="gui" style={{ alignItems: 'flex-start' }}>
              <div>
                <div style={{ fontWeight: 600 }}>GUI-managed (default)</div>
                <Text type="secondary" style={{ fontSize: 12, lineHeight: 1.5 }}>
                  You manage users on the Users page. The YAML config holds no users.
                </Text>
              </div>
            </Radio>
            <Radio value="declarative" disabled={declarativeBlocked} style={{ alignItems: 'flex-start' }}>
              <div>
                {/* A disabled radio greys its label to about 2.5:1; the title
                    keeps full contrast and the lock tag says it is off. */}
                <div style={{ fontWeight: 600, color: colors.TEXT_PRIMARY, display: 'flex', alignItems: 'center', gap: SPACE.xs }}>
                  Declarative (YAML-authoritative)
                  {declarativeBlocked && (
                    <span
                      data-testid="declarative-unavailable"
                      style={{
                        display: 'inline-flex', alignItems: 'center', gap: 4,
                        fontSize: 11, fontWeight: 600, padding: `0 ${SPACE.xs}px`, borderRadius: 4,
                        color: colors.TEXT_SECONDARY, border: `1px solid ${colors.BORDER}`,
                      }}
                    >
                      <LockOutlined aria-hidden /> Not available
                    </span>
                  )}
                </div>
                <Text type="secondary" style={{ fontSize: 12, lineHeight: 1.5 }}>
                  The YAML config lists the users, and each apply makes the user database match it.
                </Text>
              </div>
            </Radio>
            {declarativeBlocked && (
              // D64: the reason folds away; the "Not available" tag says enough at a glance.
              <details style={{ fontSize: 12, lineHeight: 1.5, marginLeft: SPACE.lg, color: colors.TEXT_MUTED }}>
                <summary style={{ cursor: 'pointer', width: 'fit-content' }}>Why is it not available?</summary>
                <div style={{ marginTop: SPACE.xxs }}>
                  The YAML config has no users, and a switch would empty the user database. Apply a YAML
                  document that lists the users in <code>access.iam_users</code> and sets{' '}
                  <code>iam_mode: declarative</code>. See{' '}
                  <a href={docsUrlToInAppHref('https://deltaglider.com/docs/how-to/manage-iam-as-code') ?? undefined}>
                    How to manage IAM as code
                  </a>
                  .
                </div>
              </details>
            )}
          </Radio.Group>
          {declarativeActive && (
            <Alert
              type="warning"
              showIcon
              icon={<LockOutlined />}
              style={{ marginTop: SPACE.sm }}
              title="Declarative mode is active after Apply"
              description={
                <span style={{ fontSize: 12 }}>
                  The Users, Groups and External authentication pages become read-only.
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
          description="Whether S3 clients must sign their requests."
        />
        {runtimeConfig && (
          <Alert
            type={runtimeConfig.auth_enabled ? 'success' : 'warning'}
            showIcon
            style={{ marginBottom: SPACE.md }}
            title={
              runtimeConfig.auth_enabled
                ? 'SigV4 authentication is on. Every S3 request must be signed.'
                : 'SigV4 authentication is off. Anyone who can reach the proxy can read and write.'
            }
          />
        )}
        <div>
          {authEnv && (
            <Text type="secondary" data-testid="auth-mode-env-note" style={{ fontSize: 12, display: 'block', marginBottom: SPACE.xs }}>
              {envOverrideHelp(authEnv)}
            </Text>
          )}
          <Radio.Group
            value={authMode}
            disabled={authEnv !== null}
            onChange={(e) => setAuthMode(e.target.value as AuthMode)}
            style={{ display: 'flex', flexDirection: 'column', gap: SPACE.sm }}
          >
            <Radio value="auto" style={{ alignItems: 'flex-start' }}>
              <div>
                <div style={{ fontWeight: 600 }}>Auto-detect (recommended)</div>
                <Text type="secondary" style={{ fontSize: 12 }}>
                  Signing is required when IAM users or bootstrap credentials exist.
                </Text>
              </div>
            </Radio>
            <Radio value="none" style={{ alignItems: 'flex-start' }}>
              <div>
                <div style={{ fontWeight: 600 }}>
                  Open access <span style={{ color: colors.ACCENT_AMBER, fontSize: 11 }}>(dev only)</span>
                </div>
                <Text type="secondary" style={{ fontSize: 12 }}>
                  No request is checked. In YAML:{' '}
                  <code style={{ fontFamily: 'var(--font-mono)' }}>authentication: &quot;none&quot;</code>.
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
          description="One S3 key pair that works before IAM users exist. It is not the admin password."
        />
        <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.sm }}>
          <FormField
            label="Access key ID"
            yamlPath="access.access_key_id"
            helpText="Clients use this key to sign S3 requests."
          >
            <Input
              value={accessKeyShown}
              onChange={(e) => setAccessKey(e.target.value)}
              placeholder="AKIAIOSFODNN7EXAMPLE"
              style={{ ...inputRadius }}
            />
          </FormField>
          <FormField
            label="Secret access key"
            yamlPath="access.secret_access_key"
            helpText={
              accessKeyShown
                ? 'The current secret is never shown. Leave the field empty to keep it, or enter a new secret to rotate it.'
                : 'Shared secret paired with the access key.'
            }
          >
            {/* Guard against browser password-manager heuristics — a
                hidden field with the access key as "username" keeps
                autocomplete from latching onto this field. */}
            <input
              type="text"
              autoComplete="username"
              value={accessKeyShown}
              readOnly
              aria-hidden="true"
              tabIndex={-1}
              style={{ display: 'none' }}
            />
            <MaskedSecretInput
              mode="blank-keeps"
              value={form.secret_access_key ?? ''}
              onChange={setSecretKey}
              placeholder={accessKeyShown ? 'Unchanged' : undefined}
              autoComplete="new-password"
              style={{ ...inputRadius }}
            />
          </FormField>
          {runningKeyId && !accessKeyEnv && (
            <div>
              <Button danger loading={removing} onClick={() => void removeBootstrap()}>
                Remove bootstrap credentials
              </Button>
            </div>
          )}
          {removeError && <PlainErrorAlert error={removeError} />}
          {removeWarnings.map((w) => (
            <Alert key={w} type="warning" showIcon title={w} />
          ))}
        </div>
      </div>

      {/* Admin GUI password change — separate endpoint, always
          available (distinct from S3 SigV4 auth). Its header says that it
          is not the SigV4 secret above. */}
      <PasswordChangeCard />

      <StickyDirtyBar
        visible={isDirty}
        applying={applying}
        onDiscard={discard}
        onApply={runApply}
        floating
      />

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
