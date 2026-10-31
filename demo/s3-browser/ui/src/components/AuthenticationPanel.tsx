import { useId, useState } from 'react';
import { Button, Typography, Input, Alert, Switch, Divider, message } from 'antd';
import { PlusOutlined, SearchOutlined, CopyOutlined, SafetyOutlined, CheckCircleOutlined, CloseCircleOutlined, SyncOutlined, UserOutlined } from '@ant-design/icons';
import {
  testAuthProvider, previewMapping, syncMemberships,
  type AuthProvider, type ProviderTestForm, type ProviderTestResult,
} from '../adminApi';
import { useIamMode } from '../queries/config';
import { useAuthProviders, useCreateAuthProvider, useUpdateAuthProvider, useDeleteAuthProvider } from '../queries/authProviders';
import { useGroupMappingRules, useCreateMappingRule, useUpdateMappingRule, useDeleteMappingRule } from '../queries/mappingRules';
import { useExternalIdentities } from '../queries/externalIdentities';
import { useGroups } from '../queries/groups';
import { useColors } from '../ThemeContext';
import { useFormLabelStyle, useCardStyles, contentColumn, CONTENT_FORM, SPACE, CARD_GAP } from './shared-styles';
import PageActions from './PageActions';
import { EmptyState, ErrorState, LoadingState } from './StatePlaceholders';
import { v, useFieldValidation } from '../validation';
import { plainError } from '../plainError';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import IamSourceBanner from './IamSourceBanner';
import MappingRuleRow from './MappingRuleRow';
import MaskedSecretInput from './MaskedSecretInput';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from '../queries/keys';
import { ApiError } from '../errorHandling';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { IAM_DIRTY_KEYS, confirmDiscardEdits, useDirtyFlag, useFormBaseline } from '../useDirtyFlag';
import { useApplyHandler } from '../useDirtySection';
import { useCopyToClipboard } from '../useCopyToClipboard';
import { confirmDialog } from '../confirmDialog';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

export default function AuthenticationPanel({ onSessionExpired }: Props) {
  const colors = useColors();

  // All four IAM-DB resources come from the shared query cache. Mutations
  // (provider + mapping-rule hooks) invalidate the relevant list key on
  // success, so there's no manual loadData()-after-every-mutation reload.
  const providersQuery = useAuthProviders();
  const rulesQuery = useGroupMappingRules();
  const identitiesQuery = useExternalIdentities();
  const groupsQuery = useGroups();

  const providers = providersQuery.data ?? [];
  const rules = rulesQuery.data ?? [];
  const identities = identitiesQuery.data ?? [];
  const groups = groupsQuery.data ?? [];

  const loading =
    providersQuery.isLoading || rulesQuery.isLoading || identitiesQuery.isLoading || groupsQuery.isLoading;
  const rawError =
    providersQuery.error ?? rulesQuery.error ?? identitiesQuery.error ?? groupsQuery.error;
  const retryLoad = () => {
    for (const q of [providersQuery, rulesQuery, identitiesQuery, groupsQuery]) if (q.error) void q.refetch();
  };

  // Bubble a 401 up so the login screen can take over. Effect, not render-body:
  // react-query keeps `error` populated across renders, so navigating in render
  // would fire a setState during render.
  useSessionExpiredOn(rawError, onSessionExpired);

  const qc = useQueryClient();

  // IAM mode for the source-of-truth banner (cached react-query read).
  const { iamMode, readOnly, loadError: iamLoadError } = useIamMode(onSessionExpired);
  // Declarative IAM: providers + mapping rules are declared in YAML and the
  // admin API 403s every mutation. Render the config surfaces read-only (no
  // New provider / Add rule / Save rules / provider Save+Delete). Non-mutating
  // diagnostics stay live: Test connection, mapping Preview, and Sync groups
  // (re-derives memberships from the declared rules — it doesn't edit them).
  // An unknown mode (config still loading, or failed) is read-only too.

  // Provider master-detail selection (form state lives in ProviderForm, keyed).
  const [selectedProviderId, setSelectedProviderId] = useState<number | null>(null);
  const [creating, setCreating] = useState(false);
  const selectedProvider = providers.find(p => p.id === selectedProviderId) ?? null;

  // Mapping rules dirty/saving state (local edits, batch-saved)
  const [pendingRules, setPendingRules] = useState<Record<number, Partial<typeof rules[number]>>>({});
  const [rulesSaving, setRulesSaving] = useState(false);
  // "Add rule" makes a local draft (negative id); Save rules creates it.
  // Creating at once left an empty rule on the server when the operator
  // left the page without saving.
  const [draftRules, setDraftRules] = useState<typeof rules>([]);
  const rulesDirty = Object.keys(pendingRules).length > 0 || draftRules.length > 0;
  useDirtyFlag(IAM_DIRTY_KEYS.mappingRules, rulesDirty && !readOnly);
  // The rule rows render the server snapshot with any pending local edits merged
  // on top (keyed by stable rule id — never array index).
  const mergedRules = [...rules, ...draftRules].map(r => (pendingRules[r.id] ? { ...r, ...pendingRules[r.id] } : r));

  const createRuleMutation = useCreateMappingRule();
  const updateRuleMutation = useUpdateMappingRule();
  const deleteRuleMutation = useDeleteMappingRule();

  // Mapping preview
  const [previewEmail, setPreviewEmail] = useState('');
  const [previewGroups, setPreviewGroups] = useState<string[]>([]);

  // Sync
  const [syncing, setSyncing] = useState(false);

  const handleTest = async (
    id: number | null,
    form: ProviderTestForm,
    setTestResult: (r: ProviderTestResult | null) => void,
    setTesting: (b: boolean) => void,
  ) => {
    setTesting(true);
    setTestResult(null);
    try {
      const result = await testAuthProvider(id, form);
      setTestResult(result);
    } catch (e) {
      setTestResult({ success: false, error: plainError(e, 'The proxy could not run the test.').text });
    } finally {
      setTesting(false);
    }
  };

  const handlePreview = async () => {
    if (!previewEmail) return;
    try {
      const result = await previewMapping(previewEmail);
      setPreviewGroups(result.group_names);
    } catch (e) {
      // Don't masquerade a request failure as "no matching rules" (an empty
      // group list) — surface it so the operator can tell the two apart.
      setPreviewGroups([]);
      message.error(plainError(e, 'The proxy could not run the preview.').text);
    }
  };

  const handleSync = async () => {
    setSyncing(true);
    try {
      const result = await syncMemberships();
      message.success(`Synced: ${result.users_updated} users updated, ${result.memberships_changed} memberships changed`);
      // Re-derived memberships affect identities, users, and groups.
      qc.invalidateQueries({ queryKey: qk.externalIdentities.list() });
      qc.invalidateQueries({ queryKey: qk.users.list() });
      qc.invalidateQueries({ queryKey: qk.groups.list() });
    } catch (e) {
      message.error(plainError(e, 'The proxy could not sync the groups.').text);
    } finally {
      setSyncing(false);
    }
  };

  // Flush any pending local rule edits to the server before an action that
  // refetches (e.g. "Add rule"), so the server snapshot doesn't overwrite them.
  const flushPendingRules = async () => {
    if (!rulesDirty) return;
    for (const rule of mergedRules) {
      if (rule.id < 0) {
        await createRuleMutation.mutateAsync({
          match_type: rule.match_type,
          match_field: rule.match_field,
          match_value: rule.match_value,
          group_id: rule.group_id,
          provider_id: rule.provider_id,
          priority: rule.priority,
        });
        setDraftRules((prev) => prev.filter((d) => d.id !== rule.id));
        continue;
      }
      if (!pendingRules[rule.id]) continue;
      await updateRuleMutation.mutateAsync({
        id: rule.id,
        patch: {
          match_type: rule.match_type,
          match_field: rule.match_field,
          match_value: rule.match_value,
          group_id: rule.group_id,
          provider_id: rule.provider_id,
          priority: rule.priority,
        },
      });
    }
    setPendingRules({});
  };

  const saveRules = async () => {
    if (mergedRules.some((r) => r.id < 0 && !r.match_value.trim())) {
      message.error('Type a pattern in every new rule, or remove the rule.');
      return;
    }
    setRulesSaving(true);
    try {
      await flushPendingRules();
      message.success('Mapping rules saved');
    } catch (e) {
      message.error(plainError(e, 'The proxy could not save the rules.').text);
    } finally {
      setRulesSaving(false);
    }
  };
  useApplyHandler(IAM_DIRTY_KEYS.mappingRules, () => { if (!rulesSaving) void saveRules(); }, rulesDirty && !readOnly);

  const callbackUrl = `${window.location.origin}/_/api/admin/oauth/callback`;

  const label = useFormLabelStyle();

  const { cardStyle } = useCardStyles();

  if (loading) {
    return (
      <div style={contentColumn(CONTENT_FORM)}>
        <LoadingState variant="form" label="Loading the external authentication settings" />
      </div>
    );
  }
  if (rawError) {
    return <ErrorState error={rawError} what="the external authentication settings" onRetry={retryLoad} />;
  }

  // D36: the page's create action is ONE primary in the page header.
  const newProvider = !readOnly && (
    <PageActions>
      <Button
        type="primary"
        icon={<PlusOutlined />}
        onClick={async () => {
          if (creating) return;
          if (!(await confirmDiscardEdits(IAM_DIRTY_KEYS.providers))) return;
          setCreating(true);
          setSelectedProviderId(null);
        }}
      >
        New provider
      </Button>
    </PageActions>
  );

  // Nothing configured yet: ONE empty state with ONE next step. The rule
  // editor, the preview, and "Sync groups" all need a provider first, so
  // showing them here only added controls that could not do anything.
  const nothingConfigured =
    providers.length === 0 && !creating && rules.length === 0 && identities.length === 0;
  if (nothingConfigured) {
    return (
      <div style={contentColumn(CONTENT_FORM)}>
        <IamSourceBanner iamMode={iamMode} loadError={iamLoadError} resource="OAuth providers + mapping rules" />
        {newProvider}
        <EmptyState
          icon={<SafetyOutlined />}
          title="No identity providers yet"
          hint={
            readOnly
              ? 'Your YAML config declares no providers. Add them under access.auth_providers and apply.'
              : 'Add an OAuth or OIDC provider, for example Google, so people can sign in with it. Click New provider at the top right.'
          }
        />
      </div>
    );
  }

  return (
    <div style={contentColumn(CONTENT_FORM)}>
      {/* IAM source-of-truth banner — OAuth providers + mapping rules
          live in the encrypted IAM DB, not YAML, in GUI mode. */}
      <IamSourceBanner iamMode={iamMode} loadError={iamLoadError} resource="OAuth providers + mapping rules" />
      {newProvider}
      {/* Identity providers */}
      <SectionHeader icon={<SafetyOutlined />} title="Identity providers" />
      {providers.length === 0 && !creating && (
        <Text type="secondary" style={{ display: 'block', fontSize: 12.5, marginBottom: SPACE.lg }}>
          No provider is configured.{readOnly ? '' : ' Click New provider at the top right.'}
        </Text>
      )}
      <div style={{ display: 'flex', gap: SPACE.md, marginBottom: SPACE.lg, flexWrap: 'wrap' }}>
        {/* Left: provider list — only when there is a list to show; the
            first provider's form takes the full width (no empty column).
            Stacks above the detail when the row wraps. */}
        {providers.length > 0 && (
        <div role="list" aria-label="Identity providers" style={{ width: 220, flexShrink: 0, flexGrow: 1, maxWidth: '100%' }}>
          {providers.map(p => (
            <button
              type="button"
              role="listitem"
              key={p.id}
              aria-current={selectedProviderId === p.id && !creating ? 'true' : undefined}
              onClick={async () => {
                if (p.id === selectedProviderId && !creating) return;
                if (!(await confirmDiscardEdits(IAM_DIRTY_KEYS.providers))) return;
                setSelectedProviderId(p.id); setCreating(false);
              }}
              style={{
                display: 'block', width: '100%', textAlign: 'left', font: 'inherit',
                padding: `${SPACE.xs}px ${SPACE.sm}px`, cursor: 'pointer', borderRadius: 8,
                border: `1px solid ${selectedProviderId === p.id ? colors.ACCENT_BLUE : colors.BORDER}`,
                background: selectedProviderId === p.id ? colors.ACCENT_BLUE + '18' : 'transparent',
                marginBottom: SPACE.xs,
              }}
            >
              <div style={{ fontSize: 13, fontWeight: 600, color: colors.TEXT_PRIMARY, fontFamily: 'var(--font-ui)' }}>
                {p.display_name || p.name}
              </div>
              <div style={{ fontSize: 11, color: colors.TEXT_MUTED }}>
                {p.provider_type} {p.enabled ? '' : '(disabled)'}
              </div>
            </button>
          ))}
        </div>
        )}

        {/* Right: provider form — keyed remount resets state per selection
            (key={provider.id} for edit, key="new" for create), so there's no
            imperative prop→state mirror. */}
        <div style={{ flex: '1 1 280px', minWidth: 0 }}>
          {creating ? (
            <ProviderForm
              key="new"
              provider={null}
              callbackUrl={callbackUrl}
              onSaved={() => setCreating(false)}
              onCancel={() => { void confirmDiscardEdits(IAM_DIRTY_KEYS.providers).then((ok) => { if (ok) setCreating(false); }); }}
              onTest={handleTest}
            />
          ) : selectedProvider ? (
            <ProviderForm
              key={selectedProvider.id}
              provider={selectedProvider}
              callbackUrl={callbackUrl}
              readOnly={readOnly}
              onSaved={() => { /* stays on the edit view; cache invalidation refreshes data */ }}
              onDeleted={() => setSelectedProviderId(null)}
              onTest={handleTest}
            />
          ) : providers.length === 0 ? null : (
            <div style={{ color: colors.TEXT_MUTED, fontSize: 13, padding: SPACE.md }}>
              {readOnly
                ? 'Select a provider to view its configuration. Providers are declared in your YAML config.'
                : 'Select a provider to edit it.'}
            </div>
          )}
        </div>
      </div>

      <Divider style={{ margin: `${SPACE.lg}px 0` }} />

      {/* Allowed users and group assignment */}
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: SPACE.sm }}>
        <SectionHeader icon={<SafetyOutlined />} title="Allowed users and group assignment" />
        {!readOnly && (
          <Button
            size="small"
            icon={<PlusOutlined />}
            disabled={rulesSaving}
            onClick={() => {
              if (groups.length === 0) { message.warning('Create a group first'); return; }
              // A local draft, saved with Save rules. It starts empty; the
              // placeholder shows the syntax hint.
              setDraftRules((prev) => [
                ...prev,
                {
                  id: -(prev.length + 1) - Date.now(),
                  provider_id: null,
                  priority: 0,
                  match_type: 'email_glob',
                  match_field: 'email',
                  match_value: '',
                  group_id: groups[0].id,
                  created_at: '',
                },
              ]);
            }}
          >
            Add rule
          </Button>
        )}
      </div>

      {mergedRules.length === 0 ? (
        <Text type="secondary" style={{ fontSize: 12 }}>
          {readOnly
            ? 'No mapping rules. Declare them under access.group_mapping_rules in your YAML config.'
            : 'No rules configured. Add allowed email patterns (e.g. *@company.com) and assign them to groups.'}
        </Text>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.xs, marginBottom: SPACE.md }}>
          {mergedRules.map((rule) => (
            <MappingRuleRow
              key={rule.id}
              rule={rule}
              providers={providers}
              groups={groups}
              colors={colors}
              disabled={rulesSaving || readOnly}
              onUpdate={(req) => {
                // Local edit only — no API call. Save button appears below.
                // Key the pending edit by the row's stable id, NOT the array
                // index (the documented admin-editor bug class).
                setPendingRules((prev) => ({
                  ...prev,
                  [rule.id]: { ...prev[rule.id], ...(req as Partial<typeof rule>) },
                }));
              }}
              onDelete={async () => {
                // MappingRuleRow's menu confirms before this runs.
                if (rule.id < 0) {
                  setDraftRules((prev) => prev.filter((d) => d.id !== rule.id));
                  return;
                }
                try {
                  await deleteRuleMutation.mutateAsync(rule.id);
                  setPendingRules((prev) => {
                    const next = { ...prev };
                    delete next[rule.id];
                    return next;
                  });
                } catch (e) {
                  message.error(plainError(e, 'The proxy could not delete the rule.').text);
                }
              }}
            />
          ))}
        </div>
      )}

      {rulesDirty && !readOnly && (
        <Button
          type="primary"
          loading={rulesSaving}
          onClick={saveRules}
          style={{ fontWeight: 600, marginBottom: SPACE.md }}
          block
        >
          Save rules
        </Button>
      )}

      {/* Preview */}
      <div style={{ ...cardStyle, padding: SPACE.md, marginBottom: CARD_GAP }}>
        <label htmlFor="mapping-preview-email" style={{ ...label, display: 'block', marginBottom: SPACE.xs }}>Preview</label>
        <div style={{ display: 'flex', gap: SPACE.xs, alignItems: 'center' }}>
          <Input
            id="mapping-preview-email"
            prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
            placeholder="Test email address..."
            value={previewEmail}
            onChange={e => {
              setPreviewEmail(e.target.value);
              // Clear the previous result so a stale match can't sit next to a
              // newly-typed email until the operator hits Check again.
              setPreviewGroups([]);
            }}
            onPressEnter={handlePreview}
            style={{ flex: 1 }}
          />
          <Button onClick={handlePreview} disabled={!previewEmail}>Check</Button>
        </div>
        {previewGroups.length > 0 && (
          <div style={{ marginTop: SPACE.xs, fontSize: 13, color: colors.TEXT_SECONDARY }}>
            Would be assigned to: <strong>{previewGroups.join(', ')}</strong>
          </div>
        )}
        {previewEmail && previewGroups.length === 0 && (
          <div style={{ marginTop: SPACE.xs, fontSize: 13, color: colors.TEXT_MUTED }}>
            No matching groups for this email.
          </div>
        )}
      </div>

      <Divider style={{ margin: `${SPACE.lg}px 0` }} />

      {/* Login activity */}
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: SPACE.sm }}>
        <SectionHeader icon={<SafetyOutlined />} title={`Login activity (${identities.length})`} />
        {/* Sync writes group memberships to the DB, which declarative mode
            refuses (403); the YAML owns IAM there. */}
        {!readOnly && (
          <Button size="small" icon={<SyncOutlined spin={syncing} />} onClick={handleSync} loading={syncing}>
            Sync groups
          </Button>
        )}
      </div>

      {identities.length === 0 ? (
        <EmptyState
          compact
          icon={<UserOutlined />}
          title="No external sign-ins yet"
          hint="A person who matches a rule above gets a user at the first sign-in."
        />
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.xxs }}>
          {identities.map(id => {
            const provider = providers.find(p => p.id === id.provider_id);
            return (
              <div key={id.id} style={{
                display: 'flex', alignItems: 'center', gap: SPACE.sm, padding: `${SPACE.xs}px ${SPACE.sm}px`,
                background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 8,
              }}>
                <SafetyOutlined style={{ color: colors.ACCENT_BLUE, fontSize: 14 }} />
                <div style={{ flex: 1 }}>
                  <div style={{ fontSize: 13, fontWeight: 500, color: colors.TEXT_PRIMARY }}>{id.email || id.external_sub}</div>
                  <div style={{ fontSize: 11, color: colors.TEXT_MUTED }}>
                    via {provider?.display_name || provider?.name || 'unknown'}
                    {id.last_login && ` · Last login: ${new Date(id.last_login + 'Z').toLocaleDateString()}`}
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

// === Provider Edit Form ===
//
// Master-detail form keyed by provider id (edit) / "new" (create) at the render
// site, so a keyed remount resets all fields from the lazy useState
// initializers below. No imperative `selectProvider()` setFormX() mirror.

interface ProviderFormProps {
  provider: AuthProvider | null; // null = create mode
  callbackUrl: string;
  /** Declarative IAM: disable inputs, hide Save/Delete. Test connection and
   *  the Copy-callback-URL affordance stay live (non-mutating). */
  readOnly?: boolean;
  onSaved: () => void;
  onDeleted?: () => void;
  /** Create mode: leave the form without saving. */
  onCancel?: () => void;
  /** Test the form as it is now (unsaved edits included); `id` null =
   *  a provider that is not saved yet. */
  onTest: (
    id: number | null,
    form: ProviderTestForm,
    setTestResult: (r: ProviderTestResult | null) => void,
    setTesting: (b: boolean) => void,
  ) => void;
}

function ProviderForm({ provider, callbackUrl, readOnly = false, onSaved, onDeleted, onCancel, onTest }: ProviderFormProps) {
  const colors = useColors();
  const { cardStyle } = useCardStyles();
  const fid = useId();
  const isEdit = provider !== null;

  const createMutation = useCreateAuthProvider();
  const updateMutation = useUpdateAuthProvider();
  const deleteMutation = useDeleteAuthProvider();

  const [formName, setFormName] = useState(() => provider?.name ?? '');
  const [formDisplayName, setFormDisplayName] = useState(() => provider?.display_name ?? '');
  const [formIssuerUrl, setFormIssuerUrl] = useState(() => provider?.issuer_url ?? 'https://accounts.google.com');
  const [formClientId, setFormClientId] = useState(() => provider?.client_id ?? '');
  // Secret is never hydrated from the server; blank means "keep existing" on edit.
  const [formClientSecret, setFormClientSecret] = useState('');
  const [formScopes, setFormScopes] = useState(() => provider?.scopes ?? 'openid email profile');
  const [formEnabled, setFormEnabled] = useState(() => provider?.enabled ?? true);
  // Network policy (extra_config.allow_local / ca_cert_path), read by the
  // server at save time and on every discovery request.
  const [formAllowLocal, setFormAllowLocal] = useState(() => provider?.extra_config?.allow_local === true);
  const [formCaCertPath, setFormCaCertPath] = useState(() => {
    const v = provider?.extra_config?.ca_cert_path;
    return typeof v === 'string' ? v : '';
  });
  // The server's save-time refusal (422), shown in the form, not as a toast.
  const [saveRefusal, setSaveRefusal] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testResult, setTestResult] = useState<ProviderTestResult | null>(null);
  const [testing, setTesting] = useState(false);
  const { copy } = useCopyToClipboard();

  const { isDirty, markClean } = useFormBaseline({
    formName, formDisplayName, formIssuerUrl, formClientId, formClientSecret, formScopes, formEnabled,
    formAllowLocal, formCaCertPath,
  });
  useDirtyFlag(IAM_DIRTY_KEYS.providers, isDirty && !readOnly);
  // Field errors show when a field loses focus (D10) or after Create/Save,
  // then follow each keystroke. The buttons stay enabled: a disabled button
  // does not say why.
  const form = useFieldValidation();
  const nameField = form.text('name', formName, setFormName, v.required('the provider name'));
  const issuerValidator = v.all(v.required('the issuer URL'), v.url());
  const issuerField = form.text('issuer', formIssuerUrl, setFormIssuerUrl, issuerValidator);
  const clientIdValidator = v.required('the client ID');
  const clientIdField = form.text('clientId', formClientId, setFormClientId, clientIdValidator);

  const handleSave = async () => {
    if (!form.submit()) return;
    setSaving(true);
    setSaveRefusal(null);
    const extraConfig = withNetworkPolicy(provider?.extra_config, formAllowLocal, formCaCertPath);
    try {
      if (!isEdit) {
        await createMutation.mutateAsync({
          name: formName,
          provider_type: 'oidc',
          enabled: formEnabled,
          display_name: formDisplayName || undefined,
          client_id: formClientId || undefined,
          client_secret: formClientSecret || undefined,
          issuer_url: formIssuerUrl || undefined,
          scopes: formScopes,
          extra_config: extraConfig,
        });
        message.success('Provider created');
      } else {
        const patch: Record<string, unknown> = {
          name: formName,
          enabled: formEnabled,
          // "" clears it on the server; `undefined` would drop the key, and
          // an absent key means "keep the old name".
          display_name: formDisplayName,
          client_id: formClientId || undefined,
          issuer_url: formIssuerUrl || undefined,
          scopes: formScopes,
          extra_config: extraConfig,
        };
        if (formClientSecret) patch.client_secret = formClientSecret;
        await updateMutation.mutateAsync({ id: provider.id, patch });
        message.success('Provider updated');
      }
      markClean();
      onSaved();
    } catch (e) {
      if (e instanceof ApiError && e.status === 422 && e.detail) setSaveRefusal(e.detail);
      else message.error(plainError(e, 'The proxy could not save the provider.').text);
    } finally {
      setSaving(false);
    }
  };

  useApplyHandler(IAM_DIRTY_KEYS.providers, () => { if (!saving) void handleSave(); }, isDirty && !readOnly);

  const handleDelete = async () => {
    if (!provider) return;
    if (!(await confirmDialog({
      title: 'Delete this provider?',
      content: 'External users linked to it can no longer sign in through this provider.',
      okText: 'Delete',
      danger: true,
    }))) return;
    try {
      await deleteMutation.mutateAsync(provider.id);
      message.success('Provider deleted');
      onDeleted?.();
    } catch (e) {
      message.error(plainError(e, 'The proxy could not delete the provider.').text);
    }
  };

  return (
    <div style={{ ...cardStyle, padding: SPACE.md }}>
      <FormField label="Display name" helpText="The sign-in button shows this name." style={{ marginBottom: 16 }}>
        <Input value={formDisplayName} onChange={e => setFormDisplayName(e.target.value)} placeholder="Google Workspace" disabled={readOnly} />
      </FormField>

      <FormField label="Provider name" helpText="A unique identifier, for example google-corp." error={nameField.error} style={{ marginBottom: 16 }}>
        <Input {...nameField.inputProps} placeholder="google-corp" disabled={readOnly} />
      </FormField>

      <FormField label="Issuer URL" error={issuerField.error} style={{ marginBottom: 16 }}>
        <Input {...issuerField.inputProps} placeholder="https://accounts.google.com" disabled={readOnly} />
      </FormField>

      <FormField label="Client ID" error={clientIdField.error} style={{ marginBottom: 16 }}>
        <Input {...clientIdField.inputProps} placeholder="123456.apps.googleusercontent.com" disabled={readOnly} />
      </FormField>

      {!readOnly && (
        <FormField
          label="Client secret"
          helpText={isEdit ? 'Leave empty to keep the current secret.' : undefined}
          style={{ marginBottom: 16 }}
        >
          <MaskedSecretInput
            id={`${fid}-secret`}
            mode={isEdit ? 'blank-keeps' : 'new'}
            value={formClientSecret}
            onChange={setFormClientSecret}
            placeholder={isEdit ? 'Unchanged' : 'Client secret'}
          />
        </FormField>
      )}

      <FormField label="Scopes" style={{ marginBottom: 16 }}>
        <Input value={formScopes} onChange={e => setFormScopes(e.target.value)} disabled={readOnly} />
      </FormField>

      {/* Callback URL */}
      {/* Same label look as the FormField labels above. */}
      <div style={{ fontSize: 13.5, fontWeight: 600, color: colors.TEXT_PRIMARY, marginBottom: SPACE.xxs }}>
        Callback URL
      </div>
      <div style={{
        display: 'flex', alignItems: 'center', gap: SPACE.xs, marginBottom: SPACE.xxs,
        background: colors.BG_BASE, border: `1px solid ${colors.BORDER}`, borderRadius: 8, padding: `${SPACE.xs}px ${SPACE.sm}px`,
      }}>
        {/* Break only after a "/" (a <wbr> each), never inside a word. */}
        <code style={{ fontSize: 12, color: colors.TEXT_SECONDARY, flex: 1, overflowWrap: 'anywhere' }}>
          {callbackUrl.split('/').map((part, i, all) => (
            <span key={i}>{part}{i < all.length - 1 && <>/<wbr /></>}</span>
          ))}
        </code>
        <Button
          size="small" icon={<CopyOutlined />}
          title="Copy callback URL"
          aria-label="Copy callback URL"
          onClick={() => { void copy(callbackUrl, { successMessage: 'Callback URL copied' }); }}
        />
      </div>
      <div style={{ fontSize: 12.5, color: colors.TEXT_MUTED, marginBottom: 16, lineHeight: 1.4 }}>
        Register this URL with your identity provider.
      </div>

      {/* Network policy */}
      <div style={{ fontSize: 13.5, fontWeight: 600, color: colors.TEXT_PRIMARY, marginBottom: SPACE.xxs }}>Network</div>
      <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs, marginBottom: SPACE.xxs }}>
        <Switch
          checked={formAllowLocal}
          onChange={setFormAllowLocal}
          size="small"
          disabled={readOnly}
          aria-label="Allow http:// and private addresses"
        />
        <Text style={{ fontSize: 13 }}>Allow http:// and private addresses</Text>
      </div>
      <div style={{ fontSize: 12.5, color: colors.TEXT_MUTED, marginBottom: 16, lineHeight: 1.4 }}>
        Turn this on only for an identity provider on your own network. The proxy then sends
        requests to any address that the issuer and its discovery document name, so a wrong
        issuer URL can reach internal services. Cloud metadata addresses stay refused.
      </div>
      <FormField
        label="CA certificate file"
        helpText="Optional. A PEM file on the proxy host, for an identity provider whose certificate a private CA signs. The proxy trusts it in addition to the public roots."
        style={{ marginBottom: 16 }}
      >
        <Input
          value={formCaCertPath}
          onChange={e => setFormCaCertPath(e.target.value)}
          placeholder="/etc/deltaglider/idp-ca.pem"
          disabled={readOnly}
        />
      </FormField>

      {saveRefusal && (
        <Alert
          type="error"
          showIcon
          title="The provider was not saved"
          description={saveRefusal}
          style={{ marginBottom: SPACE.sm }}
        />
      )}

      <div style={{ display: 'flex', alignItems: 'center', gap: 16, marginBottom: 16 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <Switch checked={formEnabled} onChange={setFormEnabled} size="small" disabled={readOnly} aria-label="Enabled" />
          <Text style={{ fontSize: 13 }}>Enabled</Text>
        </div>
      </div>

      {/* Test result */}
      {testResult && (
        <Alert
          type={testResult.success ? 'success' : 'error'}
          title={testResult.success
            ? `Connected. Issuer: ${testResult.issuer}`
            : `Failed: ${testResult.error}`}
          showIcon
          icon={testResult.success ? <CheckCircleOutlined /> : <CloseCircleOutlined />}
          style={{ marginBottom: SPACE.sm }}
        />
      )}

      <div style={{ display: 'flex', gap: SPACE.xs, flexWrap: 'wrap' }}>
        {!readOnly && (
          <Button
            type="primary"
            onClick={handleSave}
            loading={saving}
          >
            {isEdit ? 'Save' : 'Create'}
          </Button>
        )}
        <Button
          onClick={() => {
            // A test needs the issuer and the client ID only: show their errors.
            issuerField.inputProps.onBlur();
            clientIdField.inputProps.onBlur();
            if (issuerValidator(formIssuerUrl) || clientIdValidator(formClientId)) return;
            onTest(provider?.id ?? null, {
            client_id: formClientId,
            client_secret: formClientSecret,
            issuer_url: formIssuerUrl,
            scopes: formScopes,
            extra_config: withNetworkPolicy(provider?.extra_config, formAllowLocal, formCaCertPath),
          }, setTestResult, setTesting);
          }}
          loading={testing}
          title="Test the values in this form. Nothing is saved."
        >
          Test connection
        </Button>
        {isEdit && !readOnly && (
          <Button danger onClick={handleDelete}>Delete</Button>
        )}
        {!isEdit && onCancel && (
          <Button onClick={onCancel}>Cancel</Button>
        )}
      </div>
    </div>
  );
}

/**
 * `extra_config` with the form's network policy on top of the stored keys.
 * An off switch and an empty path remove their key, so the stored config
 * stays minimal (and matches a YAML declaration that omits them).
 */
function withNetworkPolicy(
  base: Record<string, unknown> | undefined,
  allowLocal: boolean,
  caCertPath: string,
): Record<string, unknown> {
  const next: Record<string, unknown> = { ...(base ?? {}) };
  delete next.allow_local;
  delete next.ca_cert_path;
  if (allowLocal) next.allow_local = true;
  if (caCertPath.trim()) next.ca_cert_path = caCertPath.trim();
  return next;
}
