import { useEffect, useState } from 'react';
import { Button, Typography, Input, Alert, Switch, Divider, Spin, message } from 'antd';
import { PlusOutlined, SearchOutlined, CopyOutlined, SafetyOutlined, CheckCircleOutlined, CloseCircleOutlined, SyncOutlined } from '@ant-design/icons';
import {
  testAuthProvider, previewMapping, syncMemberships,
  type AuthProvider, type ProviderTestResult,
} from '../adminApi';
import { useAdminConfig } from '../queries/config';
import { useAuthProviders, useCreateAuthProvider, useUpdateAuthProvider, useDeleteAuthProvider } from '../queries/authProviders';
import { useGroupMappingRules, useCreateMappingRule, useUpdateMappingRule, useDeleteMappingRule } from '../queries/mappingRules';
import { useExternalIdentities } from '../queries/externalIdentities';
import { useGroups } from '../queries/groups';
import { useColors } from '../ThemeContext';
import { useFormLabelStyle, contentColumn, CONTENT_FORM } from './shared-styles';
import SectionHeader from './SectionHeader';
import IamSourceBanner from './IamSourceBanner';
import MappingRuleRow from './MappingRuleRow';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from '../queries/keys';

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
  const error = rawError ? (rawError instanceof Error ? rawError.message : 'Failed to load data') : '';

  // Bubble a 401 up so the login screen can take over. Effect, not render-body:
  // react-query keeps `error` populated across renders, so navigating in render
  // would fire a setState during render.
  useEffect(() => {
    if (rawError instanceof Error && rawError.message.includes('401')) {
      onSessionExpired?.();
    }
  }, [rawError, onSessionExpired]);

  const qc = useQueryClient();

  // IAM mode for the source-of-truth banner (cached react-query read).
  const { data: cfg } = useAdminConfig();
  const iamMode = cfg?.iam_mode;
  // Declarative IAM: providers + mapping rules are declared in YAML and the
  // admin API 403s every mutation. Render the config surfaces read-only (no
  // New Provider / Add Rule / Save Rules / provider Save+Delete). Non-mutating
  // diagnostics stay live: Test Connection, mapping Preview, and Sync Groups
  // (re-derives memberships from the declared rules — it doesn't edit them).
  const readOnly = iamMode === 'declarative';

  // Provider master-detail selection (form state lives in ProviderForm, keyed).
  const [selectedProviderId, setSelectedProviderId] = useState<number | null>(null);
  const [creating, setCreating] = useState(false);
  const selectedProvider = providers.find(p => p.id === selectedProviderId) ?? null;

  // Mapping rules dirty/saving state (local edits, batch-saved)
  const [pendingRules, setPendingRules] = useState<Record<number, Partial<typeof rules[number]>>>({});
  const [rulesSaving, setRulesSaving] = useState(false);
  const rulesDirty = Object.keys(pendingRules).length > 0;
  // The rule rows render the server snapshot with any pending local edits merged
  // on top (keyed by stable rule id — never array index).
  const mergedRules = rules.map(r => (pendingRules[r.id] ? { ...r, ...pendingRules[r.id] } : r));

  const createRuleMutation = useCreateMappingRule();
  const updateRuleMutation = useUpdateMappingRule();
  const deleteRuleMutation = useDeleteMappingRule();

  // Mapping preview
  const [previewEmail, setPreviewEmail] = useState('');
  const [previewGroups, setPreviewGroups] = useState<string[]>([]);

  // Sync
  const [syncing, setSyncing] = useState(false);

  const handleTest = async (id: number, setTestResult: (r: ProviderTestResult | null) => void, setTesting: (b: boolean) => void) => {
    setTesting(true);
    setTestResult(null);
    try {
      const result = await testAuthProvider(id);
      setTestResult(result);
    } catch (e) {
      setTestResult({ success: false, error: e instanceof Error ? e.message : 'Test failed' });
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
      message.error(e instanceof Error ? e.message : 'Mapping preview failed');
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
      message.error(e instanceof Error ? e.message : 'Sync failed');
    } finally {
      setSyncing(false);
    }
  };

  // Flush any pending local rule edits to the server before an action that
  // refetches (e.g. "Add Rule"), so the server snapshot doesn't overwrite them.
  const flushPendingRules = async () => {
    if (!rulesDirty) return;
    for (const rule of mergedRules) {
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

  const callbackUrl = `${window.location.origin}/_/api/admin/oauth/callback`;

  const label = useFormLabelStyle();

  if (loading) return <div style={{ display: 'flex', justifyContent: 'center', padding: 40 }}><Spin /></div>;
  if (error) return <Alert type="error" message={error} style={{ margin: 16 }} />;

  return (
    <div style={contentColumn(CONTENT_FORM)}>
      {/* IAM source-of-truth banner — OAuth providers + mapping rules
          live in the encrypted IAM DB, not YAML, in GUI mode. */}
      <IamSourceBanner iamMode={iamMode} resource="OAuth providers + mapping rules" />
      {/* Identity Providers */}
      <SectionHeader icon={<SafetyOutlined />} title="Identity Providers" />
      <div style={{ display: 'flex', gap: 16, marginBottom: 24, flexWrap: 'wrap' }}>
        {/* Left: provider list — stacks above the detail when the row wraps. */}
        <div style={{ width: 220, flexShrink: 0, flexGrow: 1, maxWidth: '100%' }}>
          {!readOnly && (
            <Button icon={<PlusOutlined />} block size="small" onClick={() => { setCreating(true); setSelectedProviderId(null); }} style={{ marginBottom: 8 }}>
              New Provider
            </Button>
          )}
          {providers.map(p => (
            <div
              key={p.id}
              onClick={() => { setSelectedProviderId(p.id); setCreating(false); }}
              style={{
                padding: '10px 12px', cursor: 'pointer', borderRadius: 8,
                border: `1px solid ${selectedProviderId === p.id ? colors.ACCENT_BLUE : colors.BORDER}`,
                background: selectedProviderId === p.id ? colors.ACCENT_BLUE + '18' : 'transparent',
                marginBottom: 6,
              }}
            >
              <div style={{ fontSize: 13, fontWeight: 600, color: colors.TEXT_PRIMARY, fontFamily: 'var(--font-ui)' }}>
                {p.display_name || p.name}
              </div>
              <div style={{ fontSize: 11, color: colors.TEXT_MUTED }}>
                {p.provider_type} {p.enabled ? '' : '(disabled)'}
              </div>
            </div>
          ))}
          {providers.length === 0 && !creating && (
            <Text type="secondary" style={{ fontSize: 12 }}>No providers configured</Text>
          )}
        </div>

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
          ) : (
            <div style={{ color: colors.TEXT_MUTED, fontSize: 13, padding: 20 }}>
              {readOnly
                ? 'Select a provider to view its configuration. Providers are declared in your YAML config.'
                : 'Select a provider or create a new one to configure external authentication.'}
            </div>
          )}
        </div>
      </div>

      <Divider style={{ margin: '16px 0' }} />

      {/* Allowed Users & Group Assignment */}
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 12 }}>
        <SectionHeader icon={<SafetyOutlined />} title="Allowed Users & Group Assignment" />
        {!readOnly && (
          <Button
            size="small"
            icon={<PlusOutlined />}
            loading={rulesSaving}
            onClick={async () => {
              if (groups.length === 0) { message.warning('Create a group first'); return; }
              // Disable the rule rows for the whole flush+create+refetch round-trip
              // (same `rulesSaving` gate the Save Rules button uses). Without this,
              // edits the operator types between flushPendingRules()'s setPendingRules({})
              // and the create-triggered refetch land in pendingRules only to be
              // clobbered by the incoming server snapshot — silently lost.
              setRulesSaving(true);
              try {
                // Flush any pending local edits before creating + refetching.
                // Otherwise the refetch overwrites in-memory rule edits with the
                // server snapshot and silently drops the operator's unsaved edits.
                await flushPendingRules();
                await createRuleMutation.mutateAsync({
                  // New rules start empty — the placeholder shows the syntax hint.
                  match_type: 'email_glob',
                  match_value: '',
                  group_id: groups[0].id,
                });
              } catch (e) {
                message.error(e instanceof Error ? e.message : 'Failed');
              } finally {
                setRulesSaving(false);
              }
            }}
          >
            Add Rule
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
        <div style={{ display: 'flex', flexDirection: 'column', gap: 6, marginBottom: 16 }}>
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
                await deleteRuleMutation.mutateAsync(rule.id);
                setPendingRules((prev) => {
                  const next = { ...prev };
                  delete next[rule.id];
                  return next;
                });
              }}
            />
          ))}
        </div>
      )}

      {rulesDirty && !readOnly && (
        <Button
          type="primary"
          loading={rulesSaving}
          onClick={async () => {
            setRulesSaving(true);
            try {
              await flushPendingRules();
              message.success('Mapping rules saved');
            } catch (e) {
              message.error(e instanceof Error ? e.message : 'Save failed');
            } finally {
              setRulesSaving(false);
            }
          }}
          style={{ borderRadius: 8, fontWeight: 600, marginBottom: 16 }}
          block
        >
          Save Rules
        </Button>
      )}

      {/* Preview */}
      <div style={{
        background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 8,
        padding: 16, marginBottom: 24,
      }}>
        <div style={{ ...label, marginBottom: 8 }}>Preview</div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <Input
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
          <div style={{ marginTop: 8, fontSize: 13, color: colors.TEXT_SECONDARY }}>
            Would be assigned to: <strong>{previewGroups.join(', ')}</strong>
          </div>
        )}
        {previewEmail && previewGroups.length === 0 && (
          <div style={{ marginTop: 8, fontSize: 13, color: colors.TEXT_MUTED }}>
            No matching groups for this email.
          </div>
        )}
      </div>

      <Divider style={{ margin: '16px 0' }} />

      {/* Login Activity */}
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 12 }}>
        <SectionHeader icon={<SafetyOutlined />} title={`Login Activity (${identities.length})`} />
        <Button size="small" icon={<SyncOutlined spin={syncing} />} onClick={handleSync} loading={syncing}>
          Sync Groups
        </Button>
      </div>

      {identities.length === 0 ? (
        <Text type="secondary" style={{ fontSize: 12 }}>No external users have logged in yet. Users matching the rules above will be auto-provisioned on first login.</Text>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          {identities.map(id => {
            const provider = providers.find(p => p.id === id.provider_id);
            return (
              <div key={id.id} style={{
                display: 'flex', alignItems: 'center', gap: 12, padding: '8px 12px',
                background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 6,
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
  /** Declarative IAM: disable inputs, hide Save/Delete. Test Connection and
   *  the Copy-callback-URL affordance stay live (non-mutating). */
  readOnly?: boolean;
  onSaved: () => void;
  onDeleted?: () => void;
  onTest: (
    id: number,
    setTestResult: (r: ProviderTestResult | null) => void,
    setTesting: (b: boolean) => void,
  ) => void;
}

function ProviderForm({ provider, callbackUrl, readOnly = false, onSaved, onDeleted, onTest }: ProviderFormProps) {
  const colors = useColors();
  const label = useFormLabelStyle();
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
  const [saving, setSaving] = useState(false);
  const [testResult, setTestResult] = useState<ProviderTestResult | null>(null);
  const [testing, setTesting] = useState(false);

  const handleSave = async () => {
    setSaving(true);
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
        });
        message.success('Provider created');
      } else {
        const patch: Record<string, unknown> = {
          name: formName,
          enabled: formEnabled,
          display_name: formDisplayName || undefined,
          client_id: formClientId || undefined,
          issuer_url: formIssuerUrl || undefined,
          scopes: formScopes,
        };
        if (formClientSecret) patch.client_secret = formClientSecret;
        await updateMutation.mutateAsync({ id: provider.id, patch });
        message.success('Provider updated');
      }
      onSaved();
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Save failed');
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async () => {
    if (!provider) return;
    if (!window.confirm('Delete this provider? External users linked to it will no longer be able to log in via this provider.')) return;
    try {
      await deleteMutation.mutateAsync(provider.id);
      message.success('Provider deleted');
      onDeleted?.();
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Delete failed');
    }
  };

  return (
    <div style={{ background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 10, padding: 20 }}>
      <div style={label}>Display Name</div>
      <Input value={formDisplayName} onChange={e => setFormDisplayName(e.target.value)} placeholder="Google Workspace" disabled={readOnly} style={{ marginBottom: 12 }} />

      <div style={label}>Provider Name (unique identifier)</div>
      <Input value={formName} onChange={e => setFormName(e.target.value)} placeholder="google-corp" disabled={readOnly} style={{ marginBottom: 12 }} />

      <div style={label}>Issuer URL</div>
      <Input value={formIssuerUrl} onChange={e => setFormIssuerUrl(e.target.value)} placeholder="https://accounts.google.com" disabled={readOnly} style={{ marginBottom: 12 }} />

      <div style={label}>Client ID</div>
      <Input value={formClientId} onChange={e => setFormClientId(e.target.value)} placeholder="123456.apps.googleusercontent.com" disabled={readOnly} style={{ marginBottom: 12 }} />

      {!readOnly && (
        <>
          <div style={label}>Client Secret</div>
          <Input.Password
            value={formClientSecret}
            onChange={e => setFormClientSecret(e.target.value)}
            placeholder={isEdit ? '(leave blank to keep existing)' : 'Client secret'}
            style={{ marginBottom: 12 }}
          />
        </>
      )}

      <div style={label}>Scopes</div>
      <Input value={formScopes} onChange={e => setFormScopes(e.target.value)} disabled={readOnly} style={{ marginBottom: 12 }} />

      {/* Callback URL */}
      <div style={label}>Callback URL (register this with your provider)</div>
      <div style={{
        display: 'flex', alignItems: 'center', gap: 8, marginBottom: 16,
        background: colors.BG_BASE, border: `1px solid ${colors.BORDER}`, borderRadius: 6, padding: '8px 12px',
      }}>
        <code style={{ fontSize: 12, color: colors.TEXT_SECONDARY, flex: 1, wordBreak: 'break-all' }}>
          {callbackUrl}
        </code>
        <Button
          size="small" icon={<CopyOutlined />}
          onClick={() => { navigator.clipboard.writeText(callbackUrl); message.success('Copied'); }}
        />
      </div>

      <div style={{ display: 'flex', alignItems: 'center', gap: 16, marginBottom: 16 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <Switch checked={formEnabled} onChange={setFormEnabled} size="small" disabled={readOnly} />
          <Text style={{ fontSize: 13 }}>Enabled</Text>
        </div>
      </div>

      {/* Test result */}
      {testResult && (
        <Alert
          type={testResult.success ? 'success' : 'error'}
          message={testResult.success
            ? `Connected. Issuer: ${testResult.issuer}`
            : `Failed: ${testResult.error}`}
          showIcon
          icon={testResult.success ? <CheckCircleOutlined /> : <CloseCircleOutlined />}
          style={{ marginBottom: 12, borderRadius: 8 }}
        />
      )}

      <div style={{ display: 'flex', gap: 8 }}>
        {!readOnly && (
          <Button
            type="primary"
            onClick={handleSave}
            loading={saving}
            disabled={!formName || !formClientId || !formIssuerUrl}
          >
            {isEdit ? 'Save' : 'Create'}
          </Button>
        )}
        {isEdit && (
          <Button onClick={() => onTest(provider.id, setTestResult, setTesting)} loading={testing}>
            Test Connection
          </Button>
        )}
        {isEdit && !readOnly && (
          <Button danger onClick={handleDelete}>Delete</Button>
        )}
      </div>
    </div>
  );
}
