import { useState, useEffect } from 'react';
import { Button, Input, InputNumber, Select, Switch, Typography, Space, Alert, Spin, Tabs } from 'antd';
import { SaveOutlined, LockOutlined, WarningOutlined, DatabaseOutlined, ControlOutlined, SafetyOutlined, KeyOutlined, ApiOutlined, CloudOutlined, ArrowLeftOutlined } from '@ant-design/icons';
import type { AdminConfig, TestS3Response } from '../adminApi';
import { getAdminConfig, updateAdminConfig, changeAdminPassword, testS3Connection } from '../adminApi';
import { getEndpoint, setEndpoint, getRegion, setRegion, getCredentials, setCredentials, testConnection } from '../s3client';
import { useColors } from '../ThemeContext';

const { Title, Text } = Typography;

const LOG_LEVEL_PRESETS = [
  { label: 'Error', value: 'deltaglider_proxy=error,tower_http=error' },
  { label: 'Warn', value: 'deltaglider_proxy=warn,tower_http=warn' },
  { label: 'Info', value: 'deltaglider_proxy=info,tower_http=info' },
  { label: 'Debug', value: 'deltaglider_proxy=debug,tower_http=debug' },
  { label: 'Trace', value: 'deltaglider_proxy=trace,tower_http=trace' },
  { label: 'Custom', value: '__custom__' },
];

function normalizeLogFilter(filter: string): string {
  return filter.split(',').map((s) => s.trim()).filter(Boolean).sort().join(',');
}

function findMatchingPreset(logLevel: string): string | null {
  const normalized = normalizeLogFilter(logLevel);
  for (const preset of LOG_LEVEL_PRESETS) {
    if (preset.value === '__custom__') continue;
    if (normalizeLogFilter(preset.value) === normalized) return preset.value;
  }
  return null;
}

interface Props {
  onBack: () => void;
}

function SectionHeader({ icon, title }: { icon: React.ReactNode; title: string }) {
  const { ACCENT_BLUE, TEXT_PRIMARY } = useColors();
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 4 }}>
      <div style={{
        width: 28,
        height: 28,
        borderRadius: 7,
        background: `linear-gradient(135deg, ${ACCENT_BLUE}18, ${ACCENT_BLUE}08)`,
        border: `1px solid ${ACCENT_BLUE}22`,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        flexShrink: 0,
        fontSize: 14,
        color: ACCENT_BLUE,
      }}>
        {icon}
      </div>
      <Text strong style={{ fontFamily: "var(--font-ui)", fontSize: 15, color: TEXT_PRIMARY }}>{title}</Text>
    </div>
  );
}

/* -- Shared style helpers ------------------------------------------------- */

function useCardStyles() {
  const { BG_CARD, BORDER, TEXT_MUTED } = useColors();
  const cardStyle: React.CSSProperties = {
    background: BG_CARD,
    border: `1px solid ${BORDER}`,
    borderRadius: 12,
    padding: 'clamp(16px, 3vw, 24px)',
    marginBottom: 16,
  };
  const labelStyle: React.CSSProperties = {
    color: TEXT_MUTED,
    fontSize: 11,
    fontWeight: 600,
    letterSpacing: 0.5,
    textTransform: 'uppercase' as const,
    marginBottom: 6,
    display: 'block',
    fontFamily: "var(--font-ui)",
  };
  const inputRadius = { borderRadius: 8 };
  return { cardStyle, labelStyle, inputRadius };
}

/* -- BrowserConnectionCard ------------------------------------------------ */

function BrowserConnectionCard() {
  const { cardStyle, labelStyle, inputRadius } = useCardStyles();

  const [endpoint, setConnEndpoint] = useState(getEndpoint());
  const [region, setConnRegion] = useState(getRegion());
  const savedCreds = getCredentials();
  const [accessKey, setConnAccessKey] = useState(savedCreds.accessKeyId);
  const [secretKey, setConnSecretKey] = useState(savedCreds.secretAccessKey);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ ok: boolean; buckets?: string[]; error?: string } | null>(null);

  return (
    <form onSubmit={(e) => e.preventDefault()} style={cardStyle}>
      <SectionHeader icon={<CloudOutlined />} title="Browser Connection" />
      <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)", display: 'block', marginBottom: 12 }}>
        How this browser connects to the DeltaGlider proxy.
      </Text>

      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Proxy Endpoint</span>
        <Input
          value={endpoint}
          onChange={(e) => setConnEndpoint(e.target.value)}
          placeholder="http://localhost:9000"
          style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
        />
      </div>

      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Region</span>
        <Input
          value={region}
          onChange={(e) => setConnRegion(e.target.value)}
          placeholder="us-east-1"
          style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
        />
      </div>

      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Access Key ID</span>
        <Input
          value={accessKey}
          onChange={(e) => setConnAccessKey(e.target.value)}
          placeholder="minioadmin"
          autoComplete="username"
          style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
        />
      </div>

      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Secret Access Key</span>
        <Input.Password
          value={secretKey}
          onChange={(e) => setConnSecretKey(e.target.value)}
          placeholder="minioadmin"
          autoComplete="current-password"
          style={{ ...inputRadius }}
        />
      </div>

      <Space style={{ width: '100%', marginTop: 16 }} size={8}>
        <Button
          icon={<ApiOutlined />}
          loading={testing}
          onClick={async () => {
            setTesting(true);
            setTestResult(null);
            const result = await testConnection(endpoint, accessKey, secretKey, region);
            setTestResult(result);
            setTesting(false);
          }}
          style={{ borderRadius: 8, fontFamily: "var(--font-ui)", fontWeight: 600 }}
        >
          Test
        </Button>
        <Button
          type="primary"
          icon={<SaveOutlined />}
          onClick={() => {
            setEndpoint(endpoint);
            setRegion(region);
            setCredentials(accessKey, secretKey);
            setTestResult({ ok: true, buckets: [] });
            window.location.reload();
          }}
          style={{ borderRadius: 8, fontFamily: "var(--font-ui)", fontWeight: 600 }}
        >
          Save &amp; Reconnect
        </Button>
      </Space>

      {testResult && (
        <Alert
          style={{ marginTop: 12, borderRadius: 8 }}
          type={testResult.ok ? 'success' : 'error'}
          showIcon
          message={
            testResult.ok
              ? `Connected — ${testResult.buckets?.length ?? 0} bucket${(testResult.buckets?.length ?? 0) === 1 ? '' : 's'} found`
              : 'Connection failed'
          }
          description={testResult.ok ? testResult.buckets?.join(', ') : testResult.error}
        />
      )}
    </form>
  );
}

/* -- PasswordChangeCard --------------------------------------------------- */

function PasswordChangeCard() {
  const { cardStyle, inputRadius } = useCardStyles();

  const [currentPassword, setCurrentPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [changing, setChanging] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; error?: string } | null>(null);

  const handleSubmit = async () => {
    setChanging(true);
    setResult(null);
    const res = await changeAdminPassword(currentPassword, newPassword);
    setResult(res);
    if (res.ok) {
      setCurrentPassword('');
      setNewPassword('');
    }
    setChanging(false);
  };

  return (
    <form onSubmit={(e) => { e.preventDefault(); handleSubmit(); }} style={cardStyle}>
      <Space orientation="vertical" size="middle" style={{ width: '100%' }}>
        <SectionHeader icon={<LockOutlined />} title="Change Admin Password" />

        <input type="text" autoComplete="username" defaultValue="admin" aria-hidden="true" style={{ display: 'none' }} />
        <Input.Password
          placeholder="Current password"
          value={currentPassword}
          onChange={(e) => setCurrentPassword(e.target.value)}
          autoComplete="current-password"
          style={inputRadius}
        />
        <Input.Password
          placeholder="New password"
          value={newPassword}
          onChange={(e) => setNewPassword(e.target.value)}
          autoComplete="new-password"
          style={inputRadius}
        />

        {result && (
          <Alert
            type={result.ok ? 'success' : 'error'}
            message={result.ok ? 'Password changed successfully.' : (result.error || 'Failed')}
            showIcon
            style={{ borderRadius: 8 }}
          />
        )}

        <Button
          htmlType="submit"
          loading={changing}
          disabled={!currentPassword || !newPassword}
          block
          style={{ ...inputRadius, fontFamily: "var(--font-ui)", fontWeight: 600 }}
        >
          Change Password
        </Button>
      </Space>
    </form>
  );
}

/* -- SettingsPage (main) -------------------------------------------------- */

export default function SettingsPage({ onBack }: Props) {
  const colors = useColors();
  const { cardStyle, labelStyle, inputRadius } = useCardStyles();

  const [config, setConfig] = useState<AdminConfig | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [saveResult, setSaveResult] = useState<{ warnings: string[]; requires_restart: boolean } | null>(null);

  const [maxDeltaRatio, setMaxDeltaRatio] = useState<number>(0.5);
  const [maxObjectSize, setMaxObjectSize] = useState<number>(104857600);
  const [accessKeyId, setAccessKeyId] = useState('');
  const [secretAccessKey, setSecretAccessKey] = useState('');
  const [cacheSizeMb, setCacheSizeMb] = useState<number>(100);

  const [logLevel, setLogLevel] = useState('');
  const [logLevelCustom, setLogLevelCustom] = useState(false);

  const [backendType, setBackendType] = useState<string>('filesystem');
  const [backendEndpoint, setBackendEndpoint] = useState('');
  const [backendRegion, setBackendRegion] = useState('us-east-1');
  const [backendPath, setBackendPath] = useState('./data');
  const [backendForcePathStyle, setBackendForcePathStyle] = useState(true);
  const [originalBackendType, setOriginalBackendType] = useState<string>('filesystem');

  const [beAccessKeyId, setBeAccessKeyId] = useState('');
  const [beSecretAccessKey, setBeSecretAccessKey] = useState('');

  const [testingS3, setTestingS3] = useState(false);
  const [testS3Result, setTestS3Result] = useState<TestS3Response | null>(null);

  useEffect(() => {
    getAdminConfig().then((cfg) => {
      if (cfg) {
        setConfig(cfg);
        setMaxDeltaRatio(cfg.max_delta_ratio);
        setMaxObjectSize(cfg.max_object_size);
        setAccessKeyId(cfg.access_key_id || '');
        setCacheSizeMb(cfg.cache_size_mb);
        setBackendType(cfg.backend_type);
        setOriginalBackendType(cfg.backend_type);
        setBackendPath(cfg.backend_path || './data');
        setBackendEndpoint(cfg.backend_endpoint || '');
        setBackendRegion(cfg.backend_region || 'us-east-1');
        setBackendForcePathStyle(cfg.backend_force_path_style ?? true);
        const matchedPreset = findMatchingPreset(cfg.log_level || '');
        if (matchedPreset) {
          setLogLevel(matchedPreset);
          setLogLevelCustom(false);
        } else {
          setLogLevel(cfg.log_level || 'deltaglider_proxy=debug,tower_http=debug');
          setLogLevelCustom(true);
        }
      }
      setLoading(false);
    });
  }, []);

  const handleSave = async () => {
    setSaving(true);
    setSaveResult(null);
    const payload: Record<string, unknown> = {
      max_delta_ratio: maxDeltaRatio,
      max_object_size: maxObjectSize,
      access_key_id: accessKeyId || null,
      cache_size_mb: cacheSizeMb,
      log_level: logLevel,
      backend_type: backendType,
    };
    if (backendType === 'filesystem') {
      payload.backend_path = backendPath;
    } else {
      payload.backend_endpoint = backendEndpoint || null;
      payload.backend_region = backendRegion;
      payload.backend_force_path_style = backendForcePathStyle;
    }
    if (secretAccessKey) payload.secret_access_key = secretAccessKey;
    if (beAccessKeyId) payload.backend_access_key_id = beAccessKeyId;
    if (beSecretAccessKey) payload.backend_secret_access_key = beSecretAccessKey;
    const result = await updateAdminConfig(payload);
    setSaveResult({ warnings: result.warnings, requires_restart: result.requires_restart });
    setOriginalBackendType(backendType);
    setBeAccessKeyId('');
    setBeSecretAccessKey('');
    setSaving(false);
  };

  if (loading) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}>
        <Spin description="Loading configuration..." />
      </div>
    );
  }

  if (!config) {
    return (
      <div style={{ padding: 24 }}>
        <Alert type="error" message="Failed to load configuration. Are you logged in?" showIcon />
        <Button onClick={onBack} style={{ marginTop: 16 }}>Back</Button>
      </div>
    );
  }

  /* Stable wrapper — fixed width prevents horizontal jump between tabs;
     minHeight prevents vertical collapse on short tabs (Security). */
  const tabPane: React.CSSProperties = { width: '100%', minWidth: 0, minHeight: 420 };

  /* -- Tab: Connection ---------------------------------------------------- */

  const connectionTab = <div style={tabPane}><BrowserConnectionCard /></div>;

  /* -- Tab: Backend ------------------------------------------------------- */

  const backendTab = (
    <div style={tabPane}><form onSubmit={(e) => { e.preventDefault(); handleSave(); }} style={cardStyle}>
      <SectionHeader icon={<DatabaseOutlined />} title="Backend Configuration" />
      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Listen Address</span>
        <Text style={{ fontFamily: "var(--font-mono)", fontSize: 13 }}>{config.listen_addr}</Text>
        <br />
        <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>
          Changes to listen address require a restart.
        </Text>
      </div>

      <div style={{ marginTop: 16 }}>
        <span style={labelStyle}>Backend Type</span>
        <Select
          value={backendType}
          onChange={(val) => setBackendType(val)}
          style={{ width: '100%', ...inputRadius }}
          options={[
            { label: 'Filesystem', value: 'filesystem' },
            { label: 'S3', value: 's3' },
          ]}
        />
      </div>

      {backendType !== originalBackendType && (
        <Alert
          style={{ marginTop: 12, borderRadius: 8 }}
          type="warning"
          icon={<WarningOutlined />}
          showIcon
          message="Backend type change"
          description="Changing the backend type will NOT migrate data from the previous backend. The new backend will start empty."
        />
      )}

      {backendType === 'filesystem' && (
        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Data Directory</span>
          <Input
            value={backendPath}
            onChange={(e) => setBackendPath(e.target.value)}
            placeholder="./data"
            style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
          />
        </div>
      )}

      {backendType === 's3' && (
        <>
          <div style={{ marginTop: 16 }}>
            <span style={labelStyle}>S3 Endpoint</span>
            <Input
              value={backendEndpoint}
              onChange={(e) => setBackendEndpoint(e.target.value)}
              placeholder="http://localhost:9000 (leave empty for AWS default)"
              style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
            />
          </div>
          <div style={{ marginTop: 12 }}>
            <span style={labelStyle}>S3 Region</span>
            <Input
              value={backendRegion}
              onChange={(e) => setBackendRegion(e.target.value)}
              placeholder="us-east-1"
              style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
            />
          </div>
          <div style={{ marginTop: 12, display: 'flex', alignItems: 'center', gap: 8 }}>
            <Switch
              checked={backendForcePathStyle}
              onChange={(checked) => setBackendForcePathStyle(checked)}
              size="small"
            />
            <Text style={{ fontFamily: "var(--font-ui)" }}>Force path-style URLs</Text>
            <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>(required for MinIO, LocalStack)</Text>
          </div>

          <div style={{ borderTop: `1px solid ${colors.BORDER}`, margin: '16px 0 12px' }} />

          <div>
            <span style={labelStyle}>Backend Access Key ID</span>
            <Input
              value={beAccessKeyId}
              onChange={(e) => setBeAccessKeyId(e.target.value)}
              placeholder={config.backend_has_credentials ? 'Leave empty to keep current' : 'Enter access key ID'}
              style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
            />
          </div>

          <div style={{ marginTop: 12 }}>
            <span style={labelStyle}>Backend Secret Access Key</span>
            <input type="text" autoComplete="username" value={beAccessKeyId} readOnly aria-hidden="true" style={{ display: 'none' }} />
            <Input.Password
              value={beSecretAccessKey}
              onChange={(e) => setBeSecretAccessKey(e.target.value)}
              placeholder={config.backend_has_credentials ? 'Leave empty to keep current' : 'Enter secret access key'}
              autoComplete="off"
              style={{ ...inputRadius }}
            />
          </div>

          <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)", display: 'block', marginTop: 8 }}>
            Changing backend credentials will rebuild the storage connection. In-flight requests may fail.
          </Text>

          <Button
            icon={<ApiOutlined />}
            loading={testingS3}
            onClick={async () => {
              setTestingS3(true);
              setTestS3Result(null);
              const result = await testS3Connection({
                endpoint: backendEndpoint || undefined,
                region: backendRegion || undefined,
                force_path_style: backendForcePathStyle,
                access_key_id: beAccessKeyId || undefined,
                secret_access_key: beSecretAccessKey || undefined,
              });
              setTestS3Result(result);
              setTestingS3(false);
            }}
            style={{ marginTop: 12, borderRadius: 8, fontFamily: "var(--font-ui)", fontWeight: 600 }}
            block
          >
            Test Connection
          </Button>

          {testS3Result && (
            <Alert
              style={{ marginTop: 12, borderRadius: 8 }}
              type={testS3Result.success ? 'success' : 'error'}
              showIcon
              message={
                testS3Result.success
                  ? `Connection successful — ${testS3Result.buckets?.length ?? 0} bucket${(testS3Result.buckets?.length ?? 0) === 1 ? '' : 's'} found`
                  : `Connection failed (${testS3Result.error_kind || 'unknown'})`
              }
              description={
                testS3Result.success
                  ? (testS3Result.buckets && testS3Result.buckets.length > 0
                      ? testS3Result.buckets.join(', ')
                      : undefined)
                  : testS3Result.error
              }
            />
          )}
        </>
      )}

      {saveResult && (
        <div style={{ marginTop: 16 }}>
          {saveResult.requires_restart && (
            <Alert
              type="warning"
              icon={<WarningOutlined />}
              message="Restart Required"
              description={saveResult.warnings.join('. ')}
              showIcon
              style={{ borderRadius: 8 }}
            />
          )}
          {saveResult.warnings.length > 0 && !saveResult.requires_restart && (
            <Alert type="info" message={saveResult.warnings.join('. ')} showIcon style={{ borderRadius: 8 }} />
          )}
          {saveResult.warnings.length === 0 && (
            <Alert type="success" message="Configuration saved." showIcon style={{ borderRadius: 8 }} />
          )}
        </div>
      )}

      <Button
        type="primary"
        icon={<SaveOutlined />}
        loading={saving}
        onClick={handleSave}
        block
        size="large"
        style={{
          borderRadius: 10,
          height: 48,
          fontWeight: 700,
          fontFamily: "var(--font-ui)",
          fontSize: 15,
          marginTop: 20,
        }}
      >
        Save Configuration
      </Button>
    </form></div>
  );

  /* -- Tab: Proxy --------------------------------------------------------- */

  const proxyTab = (
    <div style={tabPane}><form onSubmit={(e) => { e.preventDefault(); handleSave(); }}><Space orientation="vertical" size={0} style={{ width: '100%' }}>
      <div style={cardStyle}>
        <SectionHeader icon={<SafetyOutlined />} title="Proxy Tuning" />

        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Max Delta Ratio</span>
          <InputNumber
            value={maxDeltaRatio}
            onChange={(v) => v !== null && setMaxDeltaRatio(v)}
            min={0}
            max={1}
            step={0.05}
            style={{ width: '100%', ...inputRadius }}
          />
          <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>
            Store as delta only if delta_size/original_size is below this ratio.
          </Text>
        </div>

        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Max Object Size (bytes)</span>
          <InputNumber
            value={maxObjectSize}
            onChange={(v) => v !== null && setMaxObjectSize(v)}
            min={1024}
            step={1048576}
            style={{ width: '100%', ...inputRadius }}
            formatter={(v) => `${v}`.replace(/\B(?=(\d{3})+(?!\d))/g, ',')}
          />
        </div>

        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Cache Size (MB)</span>
          <InputNumber
            value={cacheSizeMb}
            onChange={(v) => v !== null && setCacheSizeMb(v)}
            min={1}
            step={10}
            style={{ width: '100%', ...inputRadius }}
          />
          <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>
            Reference cache size. Changes require restart.
          </Text>
        </div>
      </div>

      <div style={cardStyle}>
        <SectionHeader icon={<ControlOutlined />} title="Logging" />

        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Log Level</span>
          <Select
            value={logLevelCustom ? '__custom__' : logLevel}
            onChange={(val) => {
              if (val === '__custom__') {
                setLogLevelCustom(true);
              } else {
                setLogLevelCustom(false);
                setLogLevel(val);
              }
            }}
            style={{ width: '100%' }}
            options={LOG_LEVEL_PRESETS}
          />
        </div>

        {logLevelCustom && (
          <div style={{ marginTop: 12 }}>
            <span style={labelStyle}>Custom Filter (RUST_LOG syntax)</span>
            <Input
              value={logLevel}
              onChange={(e) => setLogLevel(e.target.value)}
              placeholder="deltaglider_proxy=debug,tower_http=info"
              style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
            />
            <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>
              Comma-separated tracing directives, e.g. "deltaglider_proxy=debug,tower_http=warn"
            </Text>
          </div>
        )}
      </div>

      <div style={cardStyle}>
        <SectionHeader icon={<KeyOutlined />} title="Proxy Authentication (SigV4)" />

        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Access Key ID</span>
          <Input
            value={accessKeyId}
            onChange={(e) => setAccessKeyId(e.target.value)}
            placeholder="Leave empty to disable auth"
            style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
          />
        </div>

        <div style={{ marginTop: 12 }}>
          <span style={labelStyle}>Secret Access Key</span>
          <input type="text" autoComplete="username" value={accessKeyId} readOnly aria-hidden="true" style={{ display: 'none' }} />
          <Input.Password
            value={secretAccessKey}
            onChange={(e) => setSecretAccessKey(e.target.value)}
            placeholder="Leave empty to keep current"
            autoComplete="off"
            style={{ ...inputRadius }}
          />
        </div>
      </div>

      {saveResult && (
        <>
          {saveResult.requires_restart && (
            <Alert
              type="warning"
              icon={<WarningOutlined />}
              message="Restart Required"
              description={saveResult.warnings.join('. ')}
              showIcon
              style={{ borderRadius: 8, marginBottom: 16 }}
            />
          )}
          {saveResult.warnings.length > 0 && !saveResult.requires_restart && (
            <Alert type="info" message={saveResult.warnings.join('. ')} showIcon style={{ borderRadius: 8, marginBottom: 16 }} />
          )}
          {saveResult.warnings.length === 0 && (
            <Alert type="success" message="Configuration saved." showIcon style={{ borderRadius: 8, marginBottom: 16 }} />
          )}
        </>
      )}

      <Button
        type="primary"
        icon={<SaveOutlined />}
        loading={saving}
        onClick={handleSave}
        block
        size="large"
        style={{
          borderRadius: 10,
          height: 48,
          fontWeight: 700,
          fontFamily: "var(--font-ui)",
          fontSize: 15,
        }}
      >
        Save Configuration
      </Button>
    </Space></form></div>
  );

  /* -- Tab: Security ------------------------------------------------------ */

  const securityTab = <div style={tabPane}><PasswordChangeCard /></div>;

  /* -- Tab items ---------------------------------------------------------- */

  const tabItems = [
    {
      key: 'connection',
      label: (
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
          <CloudOutlined aria-hidden="true" />
          <span>Connection</span>
        </span>
      ),
      children: connectionTab,
    },
    {
      key: 'backend',
      label: (
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
          <DatabaseOutlined aria-hidden="true" />
          <span>Backend</span>
        </span>
      ),
      children: backendTab,
    },
    {
      key: 'proxy',
      label: (
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
          <ControlOutlined aria-hidden="true" />
          <span>Proxy</span>
        </span>
      ),
      children: proxyTab,
    },
    {
      key: 'security',
      label: (
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
          <LockOutlined aria-hidden="true" />
          <span>Security</span>
        </span>
      ),
      children: securityTab,
    },
  ];

  return (
    <div className="animate-fade-in" style={{ maxWidth: 640, width: '100%', margin: '0 auto', padding: 'clamp(16px, 3vw, 24px) clamp(12px, 2vw, 16px)' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 16 }}>
        <Title level={4} style={{ margin: 0, fontFamily: "var(--font-ui)", fontWeight: 700 }}>Settings</Title>
        <Button onClick={onBack} size="small" icon={<ArrowLeftOutlined />} style={inputRadius}>Back</Button>
      </div>

      <Tabs
        items={tabItems}
        defaultActiveKey="connection"
        size="middle"
        tabBarStyle={{
          fontFamily: "var(--font-ui)",
          fontWeight: 600,
          marginBottom: 20,
        }}
        style={{ minHeight: 300, width: '100%' }}
      />
    </div>
  );
}
