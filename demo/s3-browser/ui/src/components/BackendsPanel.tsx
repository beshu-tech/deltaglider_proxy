import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from '../queries/keys';
import { Button, Input, Radio, Switch, Typography, Space, Alert, Spin } from 'antd';
import { PlusOutlined, DeleteOutlined, DatabaseOutlined, CloudOutlined, CheckCircleOutlined, ApiOutlined, FolderOutlined } from '@ant-design/icons';
import type { BackendInfo, CreateBackendRequest } from '../adminApi';
import { createBackend, deleteBackend, testS3Connection, updateAdminConfig, putSection } from '../adminApi';
import { useAdminConfig } from '../queries/config';
import { useBackends } from '../queries/backends';
import { useColors } from '../ThemeContext';
import { useNavigation } from '../NavigationContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import BackendEncryptionEditor, { type BackendEncryptionPatch } from './BackendEncryptionEditor';
import { buildEncryptionSectionBody } from '../backendEncryptionPayload';
import MaskedSecretInput from './MaskedSecretInput';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

export default function BackendsPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const { cardStyle, inputRadius } = useCardStyles();
  // Query client lets mutations close the loop with `invalidateQueries`
  // instead of the local `refresh()` having to coordinate with siblings.
  const qc = useQueryClient();
  const { navigate } = useNavigation();

  // Backends + config are read from the shared cache; mutations below invalidate
  // `qk.backends.list()` + `qk.config()` (via refresh()) so all readers refresh.
  const backendsQuery = useBackends();
  const backends = backendsQuery.data?.backends ?? [];
  const defaultBackend = backendsQuery.data?.default_backend ?? null;
  const { data: config } = useAdminConfig();
  const loading = backendsQuery.isLoading;
  // Surface a load error (401 bubbles to the session-expired handler; anything
  // else renders in the Alert below).
  const loadError = backendsQuery.error;
  if (loadError && loadError instanceof Error && loadError.message.includes('401')) {
    onSessionExpired?.();
  }
  const error =
    loadError && !(loadError instanceof Error && loadError.message.includes('401'))
      ? loadError instanceof Error
        ? loadError.message
        : 'Failed to load'
      : null;

  // New backend form
  const [showForm, setShowForm] = useState(false);
  const [formName, setFormName] = useState('');
  const [formType, setFormType] = useState<'filesystem' | 's3'>('filesystem');
  const [formPath, setFormPath] = useState('./data');
  const [formEndpoint, setFormEndpoint] = useState('');
  const [formRegion, setFormRegion] = useState('us-east-1');
  const [formForcePathStyle, setFormForcePathStyle] = useState(true);
  const [formAccessKey, setFormAccessKey] = useState('');
  const [formSecretKey, setFormSecretKey] = useState('');
  const [formSetDefault, setFormSetDefault] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveResult, setSaveResult] = useState<{ ok: boolean; message: string } | null>(null);

  const [testingBackend, setTestingBackend] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<{ name: string; ok: boolean; message: string } | null>(null);

  // After a mutation, invalidate the shared cache so any panel reading
  // backends/config (this panel's own `useBackends()` + `useAdminConfig()`,
  // CredentialsModePanel, BucketsPanel, …) refetches the freshly-saved state.
  // The list READ itself is now `useBackends()` — no manual setState.
  const refresh = async () => {
    await Promise.all([
      qc.invalidateQueries({ queryKey: qk.backends.list() }),
      qc.invalidateQueries({ queryKey: qk.config() }),
    ]);
  };

  const handleCreate = async () => {
    setSaving(true);
    setSaveResult(null);
    const req: CreateBackendRequest = {
      name: formName.trim(),
      type: formType,
      set_default: formSetDefault || backends.length === 0,
    };
    if (formType === 'filesystem') {
      req.path = formPath;
    } else {
      req.endpoint = formEndpoint || undefined;
      req.region = formRegion;
      req.force_path_style = formForcePathStyle;
      if (formAccessKey) req.access_key_id = formAccessKey;
      if (formSecretKey) req.secret_access_key = formSecretKey;
    }
    try {
      const result = await createBackend(req);
      if (result.success) {
        setSaveResult({ ok: true, message: `Backend '${formName.trim()}' created` });
        setShowForm(false);
        resetForm();
        await refresh();
      } else {
        setSaveResult({ ok: false, message: result.error || 'Failed to create backend' });
      }
    } catch (e) {
      setSaveResult({ ok: false, message: e instanceof Error ? e.message : 'Network error' });
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (name: string) => {
    if (!window.confirm(`Delete backend "${name}"? This cannot be undone.`)) return;
    try {
      const result = await deleteBackend(name);
      if (result.success) {
        setSaveResult({ ok: true, message: `Backend '${name}' removed` });
        await refresh();
      } else {
        setSaveResult({ ok: false, message: result.error || 'Failed to delete' });
      }
    } catch (e) {
      setSaveResult({ ok: false, message: e instanceof Error ? e.message : 'Network error' });
    }
  };

  const handleTestConnection = async (b: BackendInfo) => {
    if (b.backend_type !== 's3') return;
    setTestingBackend(b.name);
    setTestResult(null);
    try {
      const result = await testS3Connection({
        endpoint: b.endpoint || undefined,
        region: b.region || undefined,
        force_path_style: b.force_path_style ?? true,
      });
      setTestResult({
        name: b.name,
        ok: result.success,
        message: result.success
          ? `Connected — ${result.buckets?.length ?? 0} bucket(s)`
          : result.error || 'Connection failed',
      });
    } catch {
      setTestResult({ name: b.name, ok: false, message: 'Network error' });
    } finally {
      setTestingBackend(null);
    }
  };

  const resetForm = () => {
    setFormName(''); setFormType('filesystem'); setFormPath('./data');
    setFormEndpoint(''); setFormRegion('us-east-1'); setFormForcePathStyle(true);
    setFormAccessKey(''); setFormSecretKey(''); setFormSetDefault(false);
  };

  /**
   * Apply a per-backend encryption change via a targeted `storage`
   * section PUT. The wire body (singleton vs named-list shape) is
   * composed by the pure `buildEncryptionSectionBody` builder; this
   * handler owns only the I/O + result-message bookkeeping around it.
   */
  const handleEncryptionApply = async (
    backendName: string,
    patch: BackendEncryptionPatch,
  ): Promise<void> => {
    // Compose the section-PUT body via the pure builder (singleton vs
    // named-list shape, encryption-block translation). Extracted to
    // `backendEncryptionPayload.ts` so the wire shape is unit-tested
    // byte-for-byte and can't silently drift.
    const body = buildEncryptionSectionBody(backendName, patch, backends);

    // Tracks whether the try-block already set a precise result message this
    // run. Using a local (not the closed-over `saveResult` state, which is the
    // stale render snapshot) is what keeps the catch from either suppressing a
    // genuine error — because a PREVIOUS success message is still in state — or
    // clobbering the precise message just set with a generic one.
    let resultSet = false;
    try {
      const result = await putSection('storage', body);
      if (!result.ok) {
        setSaveResult({
          ok: false,
          message: result.error || 'Failed to apply encryption change',
        });
        resultSet = true;
        throw new Error(result.error || 'Apply failed');
      }
      setSaveResult({
        ok: true,
        message: `Encryption updated on backend '${backendName}'`,
      });
      await refresh();
    } catch (e) {
      if (e instanceof Error && !resultSet) {
        setSaveResult({ ok: false, message: e.message });
      }
      throw e;
    }
  };

  const globalCompressionOn = (config?.max_delta_ratio ?? 0.75) > 0;
  const bucketPolicyEntries = Object.entries(config?.bucket_policies || {});
  const routedPolicies = bucketPolicyEntries.filter(([, policy]) => Boolean(policy.backend));
  const publicPolicies = bucketPolicyEntries.filter(
    ([, policy]) => policy.public === true || (policy.public_prefixes && policy.public_prefixes.length > 0)
  );
  const quotaPolicies = bucketPolicyEntries.filter(([, policy]) => policy.quota_bytes != null);

  if (loading) {
    return <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}><Spin /></div>;
  }

  return (
    <div style={{ maxWidth: 700, margin: '0 auto', padding: 'clamp(16px, 3vw, 24px)' }}>
      <Space direction="vertical" size={0} style={{ width: '100%' }}>

        {saveResult && (
          <Alert type={saveResult.ok ? 'success' : 'error'} message={saveResult.message} showIcon closable onClose={() => setSaveResult(null)} style={{ borderRadius: 8, marginBottom: 12 }} />
        )}
        {error && (
          <Alert type="error" message={error} showIcon style={{ borderRadius: 8, marginBottom: 12 }} />
        )}

        {/* Default compression policy */}
        <div style={cardStyle}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
            <Switch
              checked={globalCompressionOn}
              onChange={async (on) => {
                try {
                  await updateAdminConfig({ max_delta_ratio: on ? 0.75 : 0 });
                  await refresh();
                } catch { /* non-blocking: user sees the toggle revert */ }
              }}
            />
            <div>
              <Text style={{ fontSize: 14, fontWeight: 700, fontFamily: 'var(--font-ui)', color: colors.TEXT_PRIMARY }}>
                Default compression: <span style={{ color: globalCompressionOn ? colors.ACCENT_GREEN : colors.ACCENT_AMBER }}>{globalCompressionOn ? 'ON' : 'OFF'}</span>
              </Text>
              <Text type="secondary" style={{ fontSize: 12, fontFamily: 'var(--font-ui)', display: 'block', marginTop: 2, lineHeight: 1.6 }}>
                {globalCompressionOn
                  ? 'New buckets compress by default. Versioned binaries are stored as xdelta3 deltas (30-70% savings). GETs reconstruct transparently. Already-compressed formats (images, video) are skipped automatically.'
                  : 'New buckets store files as-is by default. You can still enable compression for individual buckets below.'}
                {' '}Per-bucket overrides always take precedence.
              </Text>
            </div>
          </div>
        </div>

        {/* Storage Backends */}
        <div style={cardStyle}>
          <SectionHeader
            icon={<DatabaseOutlined />}
            title="Storage Backends"
            description={
              backends.length === 0
                ? 'No backend configured.'
                : backends.every((b) => b.is_synthesized)
                  ? 'Running on the legacy singleton backend (shown below). Add a named backend to migrate to the multi-backend shape; the singleton stays active until you clear `storage.backend` in YAML.'
                  : `${backends.length} backend${backends.length !== 1 ? 's' : ''} configured.`
            }
          />

          {backends.map((b) => (
            <div key={b.name} style={{
              marginTop: 12, padding: '12px 14px',
              border: `1px solid ${b.name === defaultBackend ? colors.ACCENT_BLUE + '66' : colors.BORDER}`,
              borderRadius: 8,
              background: b.name === defaultBackend ? colors.ACCENT_BLUE + '08' : colors.BG_ELEVATED,
            }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                {b.backend_type === 'filesystem'
                  ? <DatabaseOutlined style={{ fontSize: 16, color: colors.ACCENT_BLUE }} />
                  : <CloudOutlined style={{ fontSize: 16, color: colors.ACCENT_BLUE }} />}
                <div style={{ flex: 1 }}>
                  <Text strong style={{ fontFamily: 'var(--font-ui)', fontSize: 14 }}>{b.name}</Text>
                  {b.name === defaultBackend && (
                    <span style={{ fontSize: 10, color: colors.ACCENT_BLUE, marginLeft: 8, fontWeight: 600 }}>DEFAULT</span>
                  )}
                  {b.is_synthesized && (
                    <span
                      style={{ fontSize: 10, color: colors.ACCENT_AMBER, marginLeft: 8, fontWeight: 600 }}
                      title="Virtual projection of the legacy singleton `storage.backend` in YAML. Not a real named backend; cannot be deleted. Add a named backend to migrate."
                    >
                      LEGACY SINGLETON
                    </span>
                  )}
                  <div style={{ fontSize: 12, color: colors.TEXT_MUTED, fontFamily: 'var(--font-mono)' }}>
                    {b.backend_type === 'filesystem'
                      ? `filesystem: ${b.path}`
                      : `s3: ${b.endpoint || 'AWS'} (${b.region})`}
                  </div>
                </div>
                {b.backend_type === 's3' && (
                  <Button size="small" icon={<ApiOutlined />} loading={testingBackend === b.name} onClick={() => handleTestConnection(b)} title="Test connection" />
                )}
                {!b.is_synthesized && (
                  <Button size="small" icon={<DeleteOutlined />} danger onClick={() => handleDelete(b.name)} title="Remove backend" />
                )}
              </div>
              {testResult?.name === b.name && (
                <Alert type={testResult.ok ? 'success' : 'error'} message={testResult.message} showIcon style={{ marginTop: 8, borderRadius: 6 }} />
              )}
              {/* Per-backend encryption subsection: shows the current
                 mode, exposes a mode-change picker, and wraps the
                 proxy-AES key-generation flow. Apply sends a targeted
                 storage section PUT; siblings preserved by merge-patch. */}
              <BackendEncryptionEditor
                backendName={b.name}
                current={b.encryption}
                onApply={(patch) => handleEncryptionApply(b.name, patch)}
              />
            </div>
          ))}

          {!showForm && (
            <Button icon={<PlusOutlined />} onClick={() => setShowForm(true)} style={{ marginTop: 12, borderRadius: 8, fontFamily: 'var(--font-ui)', fontWeight: 600 }} block type="dashed">
              Add Backend
            </Button>
          )}
        </div>

        {/* New Backend Form */}
        {showForm && (
          <div style={cardStyle}>
            <SectionHeader icon={<PlusOutlined />} title="New Backend" />
            <div>
              <FormField label="Name" yamlPath="storage.backends[].name">
                <Input value={formName} onChange={(e) => setFormName(e.target.value)} placeholder="e.g. local, hetzner, aws-prod" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
              </FormField>
              <FormField label="Type" yamlPath="storage.backends[].type">
                <Radio.Group value={formType} onChange={(e) => setFormType(e.target.value)} style={{ display: 'flex', gap: 0 }}>
                  <Radio.Button value="filesystem" style={{ fontSize: 13 }}>Filesystem</Radio.Button>
                  <Radio.Button value="s3" style={{ fontSize: 13 }}>S3</Radio.Button>
                </Radio.Group>
              </FormField>
              {formType === 'filesystem' && (
                <FormField label="Data Directory" yamlPath="storage.backends[].path">
                  <Input value={formPath} onChange={(e) => setFormPath(e.target.value)} placeholder="./data" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                </FormField>
              )}
              {formType === 's3' && (
                <>
                  <FormField label="Endpoint" yamlPath="storage.backends[].endpoint">
                    <Input value={formEndpoint} onChange={(e) => setFormEndpoint(e.target.value)} placeholder="https://fsn1.your-objectstorage.com" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                  </FormField>
                  <FormField label="Region" yamlPath="storage.backends[].region">
                    <Input value={formRegion} onChange={(e) => setFormRegion(e.target.value)} placeholder="us-east-1" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                  </FormField>
                  <div style={{ marginBottom: 20, display: 'flex', alignItems: 'center', gap: 8 }}>
                    <Switch checked={formForcePathStyle} onChange={setFormForcePathStyle} size="small" />
                    <Text style={{ fontSize: 13, fontFamily: 'var(--font-ui)' }}>Force path-style URLs</Text>
                  </div>
                  <FormField label="Access Key ID" yamlPath="storage.backends[].access_key_id">
                    <Input value={formAccessKey} onChange={(e) => setFormAccessKey(e.target.value)} placeholder="AKIAIOSFODNN7EXAMPLE" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                  </FormField>
                  <FormField label="Secret Access Key" yamlPath="storage.backends[].secret_access_key">
                    <MaskedSecretInput
                      mode="blank-keeps"
                      value={formSecretKey}
                      onChange={setFormSecretKey}
                      placeholder="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLE"
                      style={{ ...inputRadius }}
                    />
                  </FormField>
                </>
              )}
            </div>
            <div style={{ marginTop: 12, display: 'flex', alignItems: 'center', gap: 8 }}>
              <Switch checked={formSetDefault} onChange={setFormSetDefault} size="small" />
              <Text style={{ fontSize: 13, fontFamily: 'var(--font-ui)' }}>Set as default backend</Text>
            </div>
            <div style={{ marginTop: 16, display: 'flex', gap: 8 }}>
              <Button type="primary" icon={<CheckCircleOutlined />} onClick={handleCreate} loading={saving} disabled={!formName.trim()} style={{ flex: 1, borderRadius: 8, fontWeight: 600 }}>
                Create Backend
              </Button>
              <Button onClick={() => { setShowForm(false); resetForm(); }} style={{ borderRadius: 8 }}>Cancel</Button>
            </div>
          </div>
        )}

        {/* Bucket policy summary — Buckets owns the editor. */}
        <div style={cardStyle}>
          <SectionHeader
            icon={<FolderOutlined />}
            title="Bucket policy routing"
            description="Per-bucket routing, compression, aliases, quotas, and public read live in the Buckets page."
          />
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, minmax(0, 1fr))', gap: 8 }}>
            {[
              ['Policies', bucketPolicyEntries.length, 'custom bucket rows'],
              ['Routed', routedPolicies.length, 'non-default backend'],
              ['Public', publicPolicies.length, 'anonymous read'],
              ['Quotas', quotaPolicies.length, 'quota limits'],
            ].map(([label, value, hint]) => (
              <div
                key={label}
                style={{
                  border: `1px solid ${colors.BORDER}`,
                  borderRadius: 8,
                  padding: '10px 12px',
                  background: colors.BG_ELEVATED,
                }}
              >
                <div style={{ fontSize: 18, fontWeight: 700, color: colors.TEXT_PRIMARY }}>{value}</div>
                <div style={{ fontSize: 11, fontWeight: 700, color: colors.TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 0.6 }}>{label}</div>
                <div style={{ fontSize: 11, color: colors.TEXT_MUTED, marginTop: 2 }}>{hint}</div>
              </div>
            ))}
          </div>
          <Button
            type="primary"
            ghost
            icon={<FolderOutlined />}
            onClick={() => navigate('admin/configuration/storage/buckets')}
            style={{ marginTop: 12, borderRadius: 8, fontWeight: 600 }}
            block
          >
            Open bucket policies
          </Button>
        </div>

      </Space>
    </div>
  );
}
