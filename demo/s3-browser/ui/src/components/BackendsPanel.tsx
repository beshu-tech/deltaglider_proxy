import { useState, useEffect } from 'react';
import { Button, Input, Radio, Switch, Typography, Space, Alert, Spin, InputNumber } from 'antd';
import { PlusOutlined, DeleteOutlined, DatabaseOutlined, CloudOutlined, CheckCircleOutlined, ApiOutlined, FolderOutlined } from '@ant-design/icons';
import type { BackendInfo, CreateBackendRequest, AdminConfig } from '../adminApi';
import { getBackends, createBackend, deleteBackend, testS3Connection, getAdminConfig, updateAdminConfig, putSection } from '../adminApi';
import { listBuckets } from '../s3client';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import SimpleSelect from './SimpleSelect';
import SimpleAutoComplete from './SimpleAutoComplete';
import BackendEncryptionEditor, { type BackendEncryptionPatch } from './BackendEncryptionEditor';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

export default function BackendsPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const { cardStyle, labelStyle, inputRadius } = useCardStyles();

  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [defaultBackend, setDefaultBackend] = useState<string | null>(null);
  const [config, setConfig] = useState<AdminConfig | null>(null);
  const [availableBuckets, setAvailableBuckets] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Per-bucket policies (local edit state)
  const [bucketPolicies, setBucketPolicies] = useState<Array<{ name: string; compression: boolean; max_delta_ratio: number | null; backend: string; alias: string; public_prefixes: string[]; quota_bytes: number | null }>>([]);
  const [policyDirty, setPolicyDirty] = useState(false);
  const [policySaving, setPolicySaving] = useState(false);

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

  const refresh = async () => {
    try {
      const [data, cfg, buckets] = await Promise.all([
        getBackends(),
        getAdminConfig(),
        listBuckets().catch(() => []),
      ]);
      setBackends(data.backends);
      setDefaultBackend(data.default_backend);
      setConfig(cfg);
      setAvailableBuckets(buckets.map((b: { name: string }) => b.name));
      if (cfg?.bucket_policies) {
        setBucketPolicies(
          Object.entries(cfg.bucket_policies).map(([name, p]) => ({
            name,
            compression: p.compression ?? true,
            max_delta_ratio: p.max_delta_ratio ?? null,
            backend: p.backend ?? '',
            alias: p.alias ?? '',
            public_prefixes: p.public_prefixes ?? [],
            quota_bytes: p.quota_bytes ?? null,
          }))
        );
      }
      setError(null);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(e instanceof Error ? e.message : 'Failed to load');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { refresh(); }, []);

  const handleSavePolicies = async () => {
    setPolicySaving(true);
    try {
      const bp: Record<string, { compression?: boolean; max_delta_ratio?: number; backend?: string; alias?: string; public_prefixes?: string[] }> = {};
      for (const p of bucketPolicies) {
        if (!p.name) continue;
        // Preserve the `[""]` sentinel (entire-bucket-public shorthand
        // that the backend normalises to/from `public: true`). Only
        // strip genuinely-empty-by-accident rows by checking whether
        // the user left ANY prefix non-empty or stuck with the lone
        // empty-string sentinel.
        const prefixes = p.public_prefixes;
        const isEntireBucket = prefixes.length === 1 && prefixes[0] === '';
        const filtered = isEntireBucket ? [''] : prefixes.filter(s => s.length > 0);

        bp[p.name] = {
          compression: p.compression,
          ...(p.max_delta_ratio != null ? { max_delta_ratio: p.max_delta_ratio } : {}),
          ...(p.backend ? { backend: p.backend } : {}),
          ...(p.alias ? { alias: p.alias } : {}),
          ...(filtered.length > 0 ? { public_prefixes: filtered } : {}),
          ...(p.quota_bytes != null ? { quota_bytes: p.quota_bytes } : {}),
        };
      }
      await updateAdminConfig({ bucket_policies: bp });
      setPolicyDirty(false);
      setSaveResult({ ok: true, message: 'Bucket policies saved' });
    } catch (e) {
      setSaveResult({ ok: false, message: e instanceof Error ? e.message : 'Save failed' });
    } finally {
      setPolicySaving(false);
    }
  };

  const updatePolicy = (idx: number, patch: Partial<typeof bucketPolicies[number]>) => {
    const next = [...bucketPolicies];
    next[idx] = { ...next[idx], ...patch };
    setBucketPolicies(next);
    setPolicyDirty(true);
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
   * Apply a per-backend encryption change.
   *
   * Composes a `storage` section-PUT body that mutates ONLY the
   * target backend's `encryption` block and leaves every sibling
   * backend + every non-encryption field untouched. The server's
   * RFC 7396 merge-patch semantics guarantee siblings are preserved
   * — we just need to send the correct shape for the path we want
   * to replace.
   *
   * Path:
   *   * Singleton (synthetic "default" backend surfaced by the
   *     server when `backends` is empty) → `{ backend_encryption:
   *     <patch> }`.
   *   * Named entry (any other name) → `{ backends: [{name, encryption}] }`.
   *     Note the server replaces `backends` as a whole array on a
   *     section PUT; for per-entry edits we send the FULL list with
   *     only the target entry's `encryption` swapped.
   */
  const handleEncryptionApply = async (
    backendName: string,
    patch: BackendEncryptionPatch,
  ): Promise<void> => {
    // Translate the patch into the wire shape. null-clears for
    // `legacy_key` pass through; absent fields rely on the server's
    // three-state preservation to keep the previous value.
    const encBody: Record<string, unknown> = { mode: patch.mode };
    if (patch.key !== undefined) encBody.key = patch.key;
    if (patch.key_id !== undefined) encBody.key_id = patch.key_id;
    if (patch.kms_key_id !== undefined) encBody.kms_key_id = patch.kms_key_id;
    if (patch.bucket_key_enabled !== undefined) encBody.bucket_key_enabled = patch.bucket_key_enabled;
    if (patch.legacy_key !== undefined) encBody.legacy_key = patch.legacy_key;
    if (patch.legacy_key_id !== undefined) encBody.legacy_key_id = patch.legacy_key_id;

    // Build the section-PUT payload. The singleton ("default") path
    // and the named-entries path have different shapes on disk.
    let body: Record<string, unknown>;
    if (backendName === 'default' && backends.length === 1 && backends[0].name === 'default') {
      // Legacy singleton path — synthesise the singleton
      // `backend_encryption` block. The server handles the
      // preservation for us.
      body = { backend_encryption: encBody };
    } else {
      // Named-backend path: replace the whole list with the edited
      // encryption entry. The server's `preserve_backend_secrets`
      // keeps non-encryption fields intact (e.g. S3 creds); the
      // `preserve_backend_encryption_secrets` walker preserves
      // sibling fields inside the encryption block itself.
      const list = backends.map((b) => {
        const backendShape: Record<string, unknown> = {
          name: b.name,
          type: b.backend_type,
        };
        if (b.path) backendShape.path = b.path;
        if (b.endpoint) backendShape.endpoint = b.endpoint;
        if (b.region) backendShape.region = b.region;
        if (b.force_path_style !== null) backendShape.force_path_style = b.force_path_style;
        if (b.name === backendName) {
          backendShape.encryption = encBody;
        }
        return backendShape;
      });
      body = { backends: list };
    }

    try {
      const result = await putSection('storage', body);
      if (!result.ok) {
        setSaveResult({
          ok: false,
          message: result.error || 'Failed to apply encryption change',
        });
        throw new Error(result.error || 'Apply failed');
      }
      setSaveResult({
        ok: true,
        message: `Encryption updated on backend '${backendName}'`,
      });
      await refresh();
    } catch (e) {
      if (e instanceof Error && !saveResult?.message) {
        setSaveResult({ ok: false, message: e.message });
      }
      throw e;
    }
  };

  const globalCompressionOn = (config?.max_delta_ratio ?? 0.75) > 0;

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
            description={backends.length === 0
              ? 'No named backends. Using legacy single-backend mode.'
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
                  <div style={{ fontSize: 12, color: colors.TEXT_MUTED, fontFamily: 'var(--font-mono)' }}>
                    {b.backend_type === 'filesystem'
                      ? `filesystem: ${b.path}`
                      : `s3: ${b.endpoint || 'AWS'} (${b.region})`}
                  </div>
                </div>
                {b.backend_type === 's3' && (
                  <Button size="small" icon={<ApiOutlined />} loading={testingBackend === b.name} onClick={() => handleTestConnection(b)} title="Test connection" />
                )}
                <Button size="small" icon={<DeleteOutlined />} danger onClick={() => handleDelete(b.name)} title="Remove backend" />
              </div>
              {testResult?.name === b.name && (
                <Alert type={testResult.ok ? 'success' : 'error'} message={testResult.message} showIcon style={{ marginTop: 8, borderRadius: 6 }} />
              )}
              {/* Step 7: per-backend encryption subsection. Shows the
                 current mode, exposes a mode-change picker, and wraps
                 the proxy-AES key-generation flow lifted from the
                 EncryptionPanel. Apply sends a targeted storage
                 section PUT; siblings are preserved by merge-patch. */}
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
            <div style={{ marginTop: 16 }}>
              <span style={labelStyle}>Name</span>
              <Input value={formName} onChange={(e) => setFormName(e.target.value)} placeholder="e.g. local, hetzner, aws-prod" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
            </div>
            <div style={{ marginTop: 12 }}>
              <span style={labelStyle}>Type</span>
              <Radio.Group value={formType} onChange={(e) => setFormType(e.target.value)} style={{ display: 'flex', gap: 0 }}>
                <Radio.Button value="filesystem" style={{ fontSize: 13 }}>Filesystem</Radio.Button>
                <Radio.Button value="s3" style={{ fontSize: 13 }}>S3</Radio.Button>
              </Radio.Group>
            </div>
            {formType === 'filesystem' && (
              <div style={{ marginTop: 12 }}>
                <span style={labelStyle}>Data Directory</span>
                <Input value={formPath} onChange={(e) => setFormPath(e.target.value)} placeholder="./data" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
              </div>
            )}
            {formType === 's3' && (
              <>
                <div style={{ marginTop: 12 }}>
                  <span style={labelStyle}>Endpoint</span>
                  <Input value={formEndpoint} onChange={(e) => setFormEndpoint(e.target.value)} placeholder="https://fsn1.your-objectstorage.com" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                </div>
                <div style={{ marginTop: 8 }}>
                  <span style={labelStyle}>Region</span>
                  <Input value={formRegion} onChange={(e) => setFormRegion(e.target.value)} placeholder="us-east-1" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                </div>
                <div style={{ marginTop: 8, display: 'flex', alignItems: 'center', gap: 8 }}>
                  <Switch checked={formForcePathStyle} onChange={setFormForcePathStyle} size="small" />
                  <Text style={{ fontSize: 13, fontFamily: 'var(--font-ui)' }}>Force path-style URLs</Text>
                </div>
                <div style={{ marginTop: 12 }}>
                  <span style={labelStyle}>Access Key ID</span>
                  <Input value={formAccessKey} onChange={(e) => setFormAccessKey(e.target.value)} placeholder="AKIAIOSFODNN7EXAMPLE" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                </div>
                <div style={{ marginTop: 8 }}>
                  <span style={labelStyle}>Secret Access Key</span>
                  <Input.Password value={formSecretKey} onChange={(e) => setFormSecretKey(e.target.value)} placeholder="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLE" style={{ ...inputRadius }} />
                </div>
              </>
            )}
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

        {/* Per-Bucket Policies */}
        <div style={cardStyle}>
          <SectionHeader
            icon={<FolderOutlined />}
            title="Per-Bucket Policies"
            description="Override compression, backend routing, or aliasing for specific buckets."
          />

          {bucketPolicies.map((bp, idx) => (
            <div key={idx} style={{
              marginTop: idx === 0 ? 12 : 8, padding: '10px 12px',
              border: `1px solid ${bp.compression ? colors.BORDER : colors.ACCENT_AMBER + '66'}`,
              borderRadius: 8,
              background: bp.compression ? colors.BG_ELEVATED : colors.ACCENT_AMBER + '0a',
            }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 8 }}>
                {backends.length > 0 && (
                  <SimpleSelect
                    value={bp.backend}
                    onChange={(v) => updatePolicy(idx, { backend: v })}
                    placeholder="Backend"
                    allowClear
                    size="small"
                    style={{ width: 170 }}
                    options={backends.map(b => ({ value: b.name, label: b.name, sublabel: b.backend_type }))}
                  />
                )}
                <SimpleAutoComplete
                  value={bp.name}
                  onChange={(v) => updatePolicy(idx, { name: v.toLowerCase().replace(/[^a-z0-9.-]/g, '') })}
                  options={availableBuckets.filter(b => !bucketPolicies.some((p, i) => i !== idx && p.name === b))}
                  placeholder="Bucket name"
                  style={{ flex: 1 }}
                />
                <Button size="small" danger icon={<DeleteOutlined />} onClick={() => { setBucketPolicies(bucketPolicies.filter((_, i) => i !== idx)); setPolicyDirty(true); }} />
              </div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 14, flexWrap: 'wrap' }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                  <Switch checked={bp.compression} onChange={(v) => updatePolicy(idx, { compression: v })} size="small" />
                  <Text style={{ fontSize: 12, fontFamily: 'var(--font-ui)', color: bp.compression ? colors.TEXT_PRIMARY : colors.ACCENT_AMBER }}>
                    {bp.compression ? 'Compression' : 'No compression'}
                  </Text>
                </div>
                {bp.compression && (
                  <div style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
                    <Text style={{ fontSize: 11, color: colors.TEXT_MUTED }}>Threshold:</Text>
                    <InputNumber value={bp.max_delta_ratio ?? undefined} onChange={(v) => updatePolicy(idx, { max_delta_ratio: v ?? null })} min={0} max={1} step={0.05} placeholder="global" style={{ width: 80, ...inputRadius }} size="small" />
                  </div>
                )}
                <div style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
                  <Text style={{ fontSize: 11, color: colors.TEXT_MUTED }}>Alias:</Text>
                  <Input value={bp.alias} onChange={(e) => updatePolicy(idx, { alias: e.target.value })} placeholder="same as bucket" style={{ width: 130, ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 11 }} size="small" />
                </div>
                <div style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
                  <Text style={{ fontSize: 11, color: bp.quota_bytes != null ? colors.ACCENT_AMBER : colors.TEXT_MUTED }}>Quota:</Text>
                  <InputNumber
                    value={bp.quota_bytes != null ? Math.round(bp.quota_bytes / (1024 * 1024 * 1024)) : undefined}
                    onChange={(v) => updatePolicy(idx, { quota_bytes: v != null ? v * 1024 * 1024 * 1024 : null })}
                    min={0}
                    placeholder="unlimited"
                    style={{ width: 90, ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 11 }}
                    size="small"
                    addonAfter="GB"
                  />
                </div>
              </div>
              {/* Public Prefixes */}
              <div style={{ marginTop: 8, paddingTop: 8, borderTop: `1px solid ${colors.BORDER}` }}>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 4 }}>
                  <Text style={{ fontSize: 11, fontWeight: 600, color: bp.public_prefixes.length > 0 ? colors.ACCENT_AMBER : colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
                    Public Prefixes {bp.public_prefixes.length > 0 && `(${bp.public_prefixes.length})`}
                  </Text>
                  <Button type="text" size="small" icon={<PlusOutlined />} onClick={() => {
                    updatePolicy(idx, { public_prefixes: [...bp.public_prefixes, ''] });
                  }} style={{ fontSize: 10, color: colors.TEXT_MUTED, padding: '0 4px' }}>Add</Button>
                </div>
                {bp.public_prefixes.map((prefix, pi) => (
                  <div key={pi} style={{ display: 'flex', alignItems: 'center', gap: 4, marginBottom: 3 }}>
                    <Input
                      value={prefix}
                      onChange={(e) => {
                        const next = [...bp.public_prefixes];
                        next[pi] = e.target.value;
                        updatePolicy(idx, { public_prefixes: next });
                      }}
                      onBlur={(e) => {
                        // Auto-append trailing '/' if missing (UX nudge)
                        const v = e.target.value;
                        if (v && !v.endsWith('/')) {
                          const next = [...bp.public_prefixes];
                          next[pi] = v + '/';
                          updatePolicy(idx, { public_prefixes: next });
                        }
                      }}
                      placeholder="e.g. builds/"
                      style={{ flex: 1, ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 11 }}
                      size="small"
                    />
                    <Button type="text" size="small" danger onClick={() => {
                      const next = bp.public_prefixes.filter((_, i) => i !== pi);
                      updatePolicy(idx, { public_prefixes: next });
                    }} style={{ padding: '0 4px', minWidth: 0 }}>×</Button>
                  </div>
                ))}
                {bp.public_prefixes.length > 0 && (
                  <Text style={{ fontSize: 10, color: colors.ACCENT_AMBER, fontFamily: 'var(--font-ui)', display: 'block', marginTop: 2 }}>
                    Objects under these prefixes are publicly accessible without authentication.
                  </Text>
                )}
              </div>
            </div>
          ))}

          <Button
            icon={<PlusOutlined />}
            onClick={() => { setBucketPolicies([...bucketPolicies, { name: '', compression: true, max_delta_ratio: null, backend: '', alias: '', public_prefixes: [], quota_bytes: null }]); setPolicyDirty(true); }}
            style={{ marginTop: 12, borderRadius: 8, fontFamily: 'var(--font-ui)', fontWeight: 600 }}
            block type="dashed"
          >
            Add Bucket Policy
          </Button>

          {policyDirty && (
            <Button type="primary" onClick={handleSavePolicies} loading={policySaving} style={{ marginTop: 12, borderRadius: 8, fontWeight: 600 }} block>
              Save Bucket Policies
            </Button>
          )}
        </div>

      </Space>
    </div>
  );
}
