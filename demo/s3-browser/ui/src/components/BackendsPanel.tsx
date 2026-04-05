import { useState, useEffect } from 'react';
import { Button, Input, Radio, Switch, Typography, Space, Alert, Spin } from 'antd';
import { PlusOutlined, DeleteOutlined, DatabaseOutlined, CloudOutlined, CheckCircleOutlined, ApiOutlined } from '@ant-design/icons';
import type { BackendInfo, CreateBackendRequest } from '../adminApi';
import { getBackends, createBackend, deleteBackend, testS3Connection } from '../adminApi';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

export default function BackendsPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const { cardStyle, labelStyle, inputRadius } = useCardStyles();

  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [defaultBackend, setDefaultBackend] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

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
      const data = await getBackends();
      setBackends(data.backends);
      setDefaultBackend(data.default_backend);
      setError(null);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(e instanceof Error ? e.message : 'Failed to load backends');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { refresh(); }, []);

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
    setFormName('');
    setFormType('filesystem');
    setFormPath('./data');
    setFormEndpoint('');
    setFormRegion('us-east-1');
    setFormForcePathStyle(true);
    setFormAccessKey('');
    setFormSecretKey('');
    setFormSetDefault(false);
  };

  if (loading) {
    return <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}><Spin /></div>;
  }

  return (
    <div style={{ maxWidth: 700, margin: '0 auto', padding: 'clamp(16px, 3vw, 24px)' }}>
      <Space direction="vertical" size={0} style={{ width: '100%' }}>

        {saveResult && (
          <Alert
            type={saveResult.ok ? 'success' : 'error'}
            message={saveResult.message}
            showIcon
            closable
            onClose={() => setSaveResult(null)}
            style={{ borderRadius: 8, marginBottom: 12 }}
          />
        )}

        {error && (
          <Alert type="error" message={error} showIcon style={{ borderRadius: 8, marginBottom: 12 }} />
        )}

        <div style={cardStyle}>
          <SectionHeader
            icon={<DatabaseOutlined />}
            title="Storage Backends"
            description={backends.length === 0
              ? 'No named backends configured. Using legacy single-backend mode.'
              : `${backends.length} backend${backends.length !== 1 ? 's' : ''} configured. Buckets route to the default unless overridden in Compression → Per-Bucket Policies.`
            }
          />

          {backends.map((b) => (
            <div key={b.name} style={{
              marginTop: 12,
              padding: '12px 14px',
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
                  <Button
                    size="small"
                    icon={<ApiOutlined />}
                    loading={testingBackend === b.name}
                    onClick={() => handleTestConnection(b)}
                    title="Test connection"
                  />
                )}
                <Button
                  size="small"
                  icon={<DeleteOutlined />}
                  danger
                  onClick={() => handleDelete(b.name)}
                  title="Remove backend"
                />
              </div>
              {testResult?.name === b.name && (
                <Alert
                  type={testResult.ok ? 'success' : 'error'}
                  message={testResult.message}
                  showIcon
                  style={{ marginTop: 8, borderRadius: 6 }}
                />
              )}
            </div>
          ))}

          {!showForm && (
            <Button
              icon={<PlusOutlined />}
              onClick={() => setShowForm(true)}
              style={{ marginTop: 12, borderRadius: 8, fontFamily: 'var(--font-ui)', fontWeight: 600 }}
              block
              type="dashed"
            >
              Add Backend
            </Button>
          )}
        </div>

        {showForm && (
          <div style={cardStyle}>
            <SectionHeader icon={<PlusOutlined />} title="New Backend" />
            <div style={{ marginTop: 16 }}>
              <span style={labelStyle}>Name</span>
              <Input
                value={formName}
                onChange={(e) => setFormName(e.target.value)}
                placeholder="e.g. local, hetzner, aws-prod"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
              />
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
                  <Input value={formEndpoint} onChange={(e) => setFormEndpoint(e.target.value)} placeholder="https://fsn1.your-objectstorage.com (leave empty for AWS)" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
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
              <Button
                type="primary"
                icon={<CheckCircleOutlined />}
                onClick={handleCreate}
                loading={saving}
                disabled={!formName.trim()}
                style={{ flex: 1, borderRadius: 8, fontWeight: 600 }}
              >
                Create Backend
              </Button>
              <Button onClick={() => { setShowForm(false); resetForm(); }} style={{ borderRadius: 8 }}>
                Cancel
              </Button>
            </div>
          </div>
        )}

      </Space>
    </div>
  );
}
