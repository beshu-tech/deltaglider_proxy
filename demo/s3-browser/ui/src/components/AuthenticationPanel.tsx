import { useState, useEffect, useCallback } from 'react';
import { Button, Typography, Input, Alert, Switch, Divider, Spin, message } from 'antd';
import SimpleSelect from './SimpleSelect';
import { PlusOutlined, DeleteOutlined, SearchOutlined, CopyOutlined, SafetyOutlined, CheckCircleOutlined, CloseCircleOutlined, SyncOutlined } from '@ant-design/icons';
import {
  getAdminConfig, getAuthProviders, createAuthProvider, updateAuthProvider, deleteAuthProvider, testAuthProvider,
  getMappingRules, createMappingRule, updateMappingRule, deleteMappingRule,
  previewMapping, getExternalIdentities, syncMemberships, getGroups,
  type AuthProvider, type IamMode, type MappingRule, type ExternalIdentity, type IamGroup, type ProviderTestResult,
} from '../adminApi';
import { useColors } from '../ThemeContext';
import IamSourceBanner from './IamSourceBanner';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

export default function AuthenticationPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const [providers, setProviders] = useState<AuthProvider[]>([]);
  const [rules, setRules] = useState<MappingRule[]>([]);
  const [identities, setIdentities] = useState<ExternalIdentity[]>([]);
  const [groups, setGroups] = useState<IamGroup[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [iamMode, setIamMode] = useState<IamMode | undefined>(undefined);

  // Load IAM mode once for the source-of-truth banner.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      const cfg = await getAdminConfig();
      if (!cancelled && cfg) setIamMode(cfg.iam_mode);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Provider form state
  const [selectedProvider, setSelectedProvider] = useState<AuthProvider | null>(null);
  const [creating, setCreating] = useState(false);
  const [formName, setFormName] = useState('');
  const [formDisplayName, setFormDisplayName] = useState('');
  const [formIssuerUrl, setFormIssuerUrl] = useState('');
  const [formClientId, setFormClientId] = useState('');
  const [formClientSecret, setFormClientSecret] = useState('');
  const [formScopes, setFormScopes] = useState('openid email profile');
  const [formEnabled, setFormEnabled] = useState(true);
  const [saving, setSaving] = useState(false);
  const [testResult, setTestResult] = useState<ProviderTestResult | null>(null);
  const [testing, setTesting] = useState(false);

  // Mapping rules dirty/saving state
  const [rulesDirty, setRulesDirty] = useState(false);
  const [rulesSaving, setRulesSaving] = useState(false);

  // Mapping preview
  const [previewEmail, setPreviewEmail] = useState('');
  const [previewGroups, setPreviewGroups] = useState<string[]>([]);

  // Sync
  const [syncing, setSyncing] = useState(false);

  const loadData = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const [p, r, ids, g] = await Promise.all([
        getAuthProviders(), getMappingRules(), getExternalIdentities(), getGroups(),
      ]);
      setProviders(p);
      setRules(r);
      setIdentities(ids);
      setGroups(g);
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'Failed to load data';
      if (msg.includes('401')) onSessionExpired?.();
      else setError(msg);
    } finally {
      setLoading(false);
    }
  }, [onSessionExpired]);

  useEffect(() => { loadData(); }, [loadData]);

  const selectProvider = (p: AuthProvider) => {
    setSelectedProvider(p);
    setCreating(false);
    setFormName(p.name);
    setFormDisplayName(p.display_name || '');
    setFormIssuerUrl(p.issuer_url || '');
    setFormClientId(p.client_id || '');
    setFormClientSecret('');
    setFormScopes(p.scopes || 'openid email profile');
    setFormEnabled(p.enabled);
    setTestResult(null);
  };

  const startCreate = () => {
    setSelectedProvider(null);
    setCreating(true);
    setFormName('');
    setFormDisplayName('');
    setFormIssuerUrl('https://accounts.google.com');
    setFormClientId('');
    setFormClientSecret('');
    setFormScopes('openid email profile');
    setFormEnabled(true);
    setTestResult(null);
  };

  const handleSave = async () => {
    setSaving(true);
    try {
      if (creating) {
        await createAuthProvider({
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
      } else if (selectedProvider) {
        const req: Record<string, unknown> = {
          name: formName,
          enabled: formEnabled,
          display_name: formDisplayName || undefined,
          client_id: formClientId || undefined,
          issuer_url: formIssuerUrl || undefined,
          scopes: formScopes,
        };
        if (formClientSecret) req.client_secret = formClientSecret;
        await updateAuthProvider(selectedProvider.id, req);
        message.success('Provider updated');
      }
      await loadData();
      setCreating(false);
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Save failed');
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (id: number) => {
    if (!window.confirm('Delete this provider? External users linked to it will no longer be able to log in via this provider.')) return;
    try {
      await deleteAuthProvider(id);
      message.success('Provider deleted');
      setSelectedProvider(null);
      setCreating(false);
      await loadData();
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Delete failed');
    }
  };

  const handleTest = async (id: number) => {
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
    } catch {
      setPreviewGroups([]);
    }
  };

  const handleSync = async () => {
    setSyncing(true);
    try {
      const result = await syncMemberships();
      message.success(`Synced: ${result.users_updated} users updated, ${result.memberships_changed} memberships changed`);
      await loadData();
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Sync failed');
    } finally {
      setSyncing(false);
    }
  };

  const callbackUrl = `${window.location.origin}/_/api/admin/oauth/callback`;

  const label = { fontSize: 11, fontWeight: 600, textTransform: 'uppercase' as const, letterSpacing: 0.5, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)', marginBottom: 4 };
  const section = { fontSize: 10, fontWeight: 700, textTransform: 'uppercase' as const, letterSpacing: 1.5, color: colors.ACCENT_BLUE, fontFamily: 'var(--font-mono)', marginBottom: 12 };

  if (loading) return <div style={{ display: 'flex', justifyContent: 'center', padding: 40 }}><Spin /></div>;
  if (error) return <Alert type="error" message={error} style={{ margin: 16 }} />;

  return (
    <div style={{ padding: 'clamp(16px, 3vw, 24px)', maxWidth: 960, margin: '0 auto' }}>
      {/* IAM source-of-truth banner — OAuth providers + mapping rules
          live in the encrypted IAM DB, not YAML, in GUI mode. */}
      <IamSourceBanner iamMode={iamMode} resource="OAuth providers + mapping rules" />
      {/* Identity Providers */}
      <div style={section}>Identity Providers</div>
      <div style={{ display: 'flex', gap: 16, marginBottom: 24 }}>
        {/* Left: provider list */}
        <div style={{ width: 220, flexShrink: 0 }}>
          <Button icon={<PlusOutlined />} block size="small" onClick={startCreate} style={{ marginBottom: 8 }}>
            New Provider
          </Button>
          {providers.map(p => (
            <div
              key={p.id}
              onClick={() => selectProvider(p)}
              style={{
                padding: '10px 12px', cursor: 'pointer', borderRadius: 8,
                border: `1px solid ${selectedProvider?.id === p.id ? colors.ACCENT_BLUE : colors.BORDER}`,
                background: selectedProvider?.id === p.id ? colors.ACCENT_BLUE + '18' : 'transparent',
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

        {/* Right: provider form */}
        <div style={{ flex: 1 }}>
          {(creating || selectedProvider) && (
            <div style={{ background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 10, padding: 20 }}>
              <div style={label}>Display Name</div>
              <Input value={formDisplayName} onChange={e => setFormDisplayName(e.target.value)} placeholder="Google Workspace" style={{ marginBottom: 12 }} />

              <div style={label}>Provider Name (unique identifier)</div>
              <Input value={formName} onChange={e => setFormName(e.target.value)} placeholder="google-corp" style={{ marginBottom: 12 }} />

              <div style={label}>Issuer URL</div>
              <Input value={formIssuerUrl} onChange={e => setFormIssuerUrl(e.target.value)} placeholder="https://accounts.google.com" style={{ marginBottom: 12 }} />

              <div style={label}>Client ID</div>
              <Input value={formClientId} onChange={e => setFormClientId(e.target.value)} placeholder="123456.apps.googleusercontent.com" style={{ marginBottom: 12 }} />

              <div style={label}>Client Secret</div>
              <Input.Password
                value={formClientSecret}
                onChange={e => setFormClientSecret(e.target.value)}
                placeholder={selectedProvider ? '(leave blank to keep existing)' : 'Client secret'}
                style={{ marginBottom: 12 }}
              />

              <div style={label}>Scopes</div>
              <Input value={formScopes} onChange={e => setFormScopes(e.target.value)} style={{ marginBottom: 12 }} />

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
                  <Switch checked={formEnabled} onChange={setFormEnabled} size="small" />
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
                <Button
                  type="primary"
                  onClick={handleSave}
                  loading={saving}
                  disabled={!formName || !formClientId || !formIssuerUrl}
                >
                  {creating ? 'Create' : 'Save'}
                </Button>
                {selectedProvider && (
                  <Button onClick={() => handleTest(selectedProvider.id)} loading={testing}>
                    Test Connection
                  </Button>
                )}
                {selectedProvider && (
                  <Button danger onClick={() => handleDelete(selectedProvider.id)}>Delete</Button>
                )}
              </div>
            </div>
          )}
          {!creating && !selectedProvider && (
            <div style={{ color: colors.TEXT_MUTED, fontSize: 13, padding: 20 }}>
              Select a provider or create a new one to configure external authentication.
            </div>
          )}
        </div>
      </div>

      <Divider style={{ margin: '16px 0' }} />

      {/* Allowed Users & Group Assignment */}
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 12 }}>
        <div style={section}>Allowed Users &amp; Group Assignment</div>
        <Button
          size="small"
          icon={<PlusOutlined />}
          onClick={async () => {
            if (groups.length === 0) { message.warning('Create a group first'); return; }
            try {
              await createMappingRule({
                match_type: 'email_glob',
                match_value: '*@example.com',
                group_id: groups[0].id,
              });
              await loadData();
            } catch (e) {
              message.error(e instanceof Error ? e.message : 'Failed');
            }
          }}
        >
          Add Rule
        </Button>
      </div>

      {rules.length === 0 ? (
        <Text type="secondary" style={{ fontSize: 12 }}>No rules configured. Add allowed email patterns (e.g. *@company.com) and assign them to groups.</Text>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 6, marginBottom: 16 }}>
          {rules.map((rule, idx) => (
            <MappingRuleRow
              key={rule.id}
              rule={rule}
              providers={providers}
              groups={groups}
              colors={colors}
              onUpdate={(req) => {
                // Local edit only — no API call. Save button appears below.
                const next = [...rules];
                next[idx] = { ...next[idx], ...req as Partial<typeof rule> };
                setRules(next);
                setRulesDirty(true);
              }}
              onDelete={async () => {
                await deleteMappingRule(rule.id);
                await loadData();
                setRulesDirty(false);
              }}
            />
          ))}
        </div>
      )}

      {rulesDirty && (
        <Button
          type="primary"
          loading={rulesSaving}
          onClick={async () => {
            setRulesSaving(true);
            try {
              for (const rule of rules) {
                await updateMappingRule(rule.id, {
                  match_type: rule.match_type,
                  match_field: rule.match_field,
                  match_value: rule.match_value,
                  group_id: rule.group_id,
                  provider_id: rule.provider_id,
                  priority: rule.priority,
                });
              }
              message.success('Mapping rules saved');
              setRulesDirty(false);
              await loadData();
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
            onChange={e => setPreviewEmail(e.target.value)}
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
        <div style={section}>
          Login Activity ({identities.length})
        </div>
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

// ── Inline Mapping Rule Row ──

interface MappingRuleRowProps {
  rule: MappingRule;
  providers: AuthProvider[];
  groups: IamGroup[];
  colors: ReturnType<typeof useColors>;
  onUpdate: (req: Record<string, unknown>) => void;
  onDelete: () => void;
}

const MATCH_TYPES = [
  { value: 'email_glob', label: 'Email pattern' },
  { value: 'email_domain', label: 'Email domain' },
  { value: 'email_exact', label: 'Email exact' },
  { value: 'email_regex', label: 'Email regex' },
  { value: 'claim_value', label: 'Claim value' },
];

function MappingRuleRow({ rule, providers, groups, colors, onUpdate, onDelete }: MappingRuleRowProps) {
  return (
    <div style={{
      display: 'flex', alignItems: 'center', gap: 8, padding: '8px 12px',
      background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 6,
      flexWrap: 'wrap',
    }}>
      <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, whiteSpace: 'nowrap' }}>When</Text>
      <SimpleSelect
        size="small"
        value={rule.match_type}
        onChange={v => onUpdate({ match_type: v })}
        options={MATCH_TYPES.map(t => ({ value: t.value, label: t.label }))}
        style={{ width: 140 }}
      />
      {rule.match_type === 'claim_value' && (
        <>
          <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>field</Text>
          <Input
            size="small"
            value={rule.match_field}
            onChange={e => onUpdate({ match_field: e.target.value })}
            style={{ width: 80 }}
          />
        </>
      )}
      <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>matches</Text>
      <Input
        size="small"
        value={rule.match_value}
        onChange={e => onUpdate({ match_value: e.target.value })}
        style={{ width: 180 }}
        placeholder={
          rule.match_type === 'email_glob' ? '*@company.com' :
          rule.match_type === 'email_domain' ? 'company.com' :
          rule.match_type === 'email_exact' ? 'alice@company.com' :
          'value'
        }
      />
      <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, whiteSpace: 'nowrap' }}>assign to</Text>
      <SimpleSelect
        size="small"
        value={String(rule.group_id)}
        onChange={v => onUpdate({ group_id: Number(v) })}
        options={groups.map(g => ({ value: String(g.id), label: g.name }))}
        style={{ width: 140 }}
      />
      <SimpleSelect
        size="small"
        value={String(rule.provider_id ?? 0)}
        onChange={v => onUpdate({ provider_id: Number(v) === 0 ? null : Number(v) })}
        options={[
          { value: '0', label: 'All providers' },
          ...providers.map(p => ({ value: String(p.id), label: p.display_name || p.name })),
        ]}
        style={{ width: 130 }}

      />
      <Button size="small" danger icon={<DeleteOutlined />} onClick={onDelete} />
    </div>
  );
}
