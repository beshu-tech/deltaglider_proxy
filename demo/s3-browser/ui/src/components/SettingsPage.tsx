import { useState, useEffect } from 'react';
import { Button, Input, InputNumber, Radio, Select, Switch, Typography, Space, Alert, Spin, Tabs } from 'antd';
import { SaveOutlined, LockOutlined, WarningOutlined, DatabaseOutlined, ControlOutlined, SafetyOutlined, KeyOutlined, ApiOutlined, CloudOutlined, ArrowLeftOutlined } from '@ant-design/icons';
import type { AdminConfig, TestS3Response } from '../adminApi';
import { getAdminConfig, updateAdminConfig, testS3Connection } from '../adminApi';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import BrowserConnectionCard from './BrowserConnectionCard';
import PasswordChangeCard from './PasswordChangeCard';


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
  onSessionExpired?: () => void;
  /** When set, renders only the content for this tab (no tabs bar, no header). Used by AdminPage. */
  embeddedTab?: string;
}

/* -- SettingsPage (main) -------------------------------------------------- */

export default function SettingsPage({ onBack, onSessionExpired, embeddedTab }: Props) {
  const colors = useColors();
  const { cardStyle, labelStyle, inputRadius } = useCardStyles();

  const [config, setConfig] = useState<AdminConfig | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [saveResult, setSaveResult] = useState<{ warnings: string[]; requires_restart: boolean } | null>(null);

  const [maxDeltaRatio, setMaxDeltaRatio] = useState<number>(0.5);
  const [maxObjectSizeMb, setMaxObjectSizeMb] = useState<number>(100);
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
  const [showAdvancedSecurity, setShowAdvancedSecurity] = useState(false);

  useEffect(() => {
    getAdminConfig().then((cfg) => {
      if (cfg) {
        setConfig(cfg);
        setMaxDeltaRatio(cfg.max_delta_ratio);
        setMaxObjectSizeMb(Math.round(cfg.max_object_size / (1024 * 1024)));
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
      } else {
        // Config load failed (401) — session expired or invalid.
        // Signal parent to reset admin state so AdminGate re-appears.
        onSessionExpired?.();
      }
      setLoading(false);
    });
  }, []);

  const handleSave = async () => {
    setSaving(true);
    setSaveResult(null);
    const payload: Record<string, unknown> = {
      max_delta_ratio: maxDeltaRatio,
      max_object_size: maxObjectSizeMb * 1024 * 1024,
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
        <Alert type="error" message="Session expired. Please log in again." showIcon />
        <Space style={{ marginTop: 16 }}>
          <Button type="primary" onClick={() => onSessionExpired?.()}>Log in again</Button>
          <Button onClick={onBack}>Back to browser</Button>
        </Space>
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

  /* -- Helper: read-only display field ------------------------------------ */
  const readOnlyField = (label: string, value: string | number | boolean | undefined, description?: string, badge?: string) => (
    <div style={{ marginTop: 16 }}>
      <span style={labelStyle}>
        {label}
        {badge && <span style={{ fontSize: 10, color: colors.ACCENT_AMBER, marginLeft: 8, fontWeight: 400 }}>{badge}</span>}
      </span>
      <Input value={String(value ?? '—')} readOnly style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13, opacity: 0.7 }} />
      {description && <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>{description}</Text>}
    </div>
  );

  /* -- Save button + result shared across editable tabs ------------------- */
  const saveSection = (
    <>
      {saveResult && (
        <>
          {saveResult.requires_restart && (
            <Alert type="warning" icon={<WarningOutlined />} message="Restart Required" description={saveResult.warnings.join('. ')} showIcon style={{ borderRadius: 8, marginBottom: 16 }} />
          )}
          {saveResult.warnings.length > 0 && !saveResult.requires_restart && (
            <Alert type="info" message={saveResult.warnings.join('. ')} showIcon style={{ borderRadius: 8, marginBottom: 16 }} />
          )}
          {saveResult.warnings.length === 0 && (
            <Alert type="success" message="Configuration saved." showIcon style={{ borderRadius: 8, marginBottom: 16 }} />
          )}
        </>
      )}
      <Button type="primary" icon={<SaveOutlined />} loading={saving} onClick={handleSave} block size="large"
        style={{ borderRadius: 10, height: 48, fontWeight: 700, fontFamily: "var(--font-ui)", fontSize: 15 }}>
        Save Configuration
      </Button>
    </>
  );

  /* -- Tab: Compression --------------------------------------------------- */
  const compressionTab = (
    <div style={tabPane}><form onSubmit={(e) => { e.preventDefault(); handleSave(); }}><Space direction="vertical" size={0} style={{ width: '100%' }}>
      <div style={cardStyle}>
        <SectionHeader icon={<SafetyOutlined />} title="Delta Encoding" description="Controls when files are stored as deltas vs full copies" />
        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Max Delta Ratio</span>
          <InputNumber value={maxDeltaRatio} onChange={(v) => v !== null && setMaxDeltaRatio(v)} min={0} max={1} step={0.05} style={{ width: '100%', ...inputRadius }} />
          <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>
            Store as delta only if compressed size is less than this fraction of the original. E.g. 0.75 means deltas must save at least 25% space.
          </Text>
        </div>
        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Max Object Size (MB)</span>
          <InputNumber value={maxObjectSizeMb} onChange={(v) => v !== null && setMaxObjectSizeMb(v)} min={1} step={10} style={{ width: '100%', ...inputRadius }} />
          <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>
            Files larger than this are always stored as-is (xdelta3 memory constraint).
          </Text>
        </div>
      </div>

      <div style={cardStyle}>
        <SectionHeader icon={<DatabaseOutlined />} title="Cache" description="In-memory caches that speed up reads. Larger = faster but more RAM." />
        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Reference Cache Size (MB) <span style={{ fontSize: 10, color: colors.ACCENT_AMBER }}>restart required</span></span>
          <InputNumber value={cacheSizeMb} onChange={(v) => v !== null && setCacheSizeMb(v)} min={0} step={100} style={{ width: '100%', ...inputRadius }} />
          <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>
            Cache for delta baselines. Each active deltaspace needs its reference in cache for fast reconstruction. Recommend 1024+ MB for production.
          </Text>
        </div>
        {readOnlyField('Metadata Cache (MB)', config?.metadata_cache_mb, 'Cache for object metadata (HEAD/LIST). Eliminates redundant S3 HEAD calls.', 'restart required')}
      </div>

      <div style={cardStyle}>
        <SectionHeader icon={<ControlOutlined />} title="Advanced Compression" description="Codec subprocess settings — usually auto-configured." />
        {readOnlyField('Codec Concurrency', config?.codec_concurrency, 'Max parallel xdelta3 encode/decode operations. Auto-detected from CPU cores.', 'restart required')}
        {readOnlyField('Codec Timeout (seconds)', config?.codec_timeout_secs, 'Kill xdelta3 subprocess if it takes longer than this. Prevents hung processes.', 'restart required')}
      </div>

      {saveSection}
    </Space></form></div>
  );

  /* -- Tab: Limits -------------------------------------------------------- */
  const limitsTab = (
    <div style={tabPane}><Space direction="vertical" size={0} style={{ width: '100%' }}>
      <div style={cardStyle}>
        <SectionHeader icon={<SafetyOutlined />} title="Request Limits" description="Protect the server from overload and abuse. All require restart to change." />
        {readOnlyField('Request Timeout (seconds)', config?.request_timeout_secs, 'Maximum time for any single request. Returns HTTP 504 Gateway Timeout when exceeded.', 'restart required')}
        {readOnlyField('Max Concurrent Requests', config?.max_concurrent_requests, 'Maximum in-flight HTTP requests. Additional requests queue until a slot opens.', 'restart required')}
        {readOnlyField('Max Multipart Uploads', config?.max_multipart_uploads, 'Maximum concurrent multipart uploads. Each holds part data in memory.', 'restart required')}
      </div>
    </Space></div>
  );

  /* -- Tab: Security ------------------------------------------------------ */
  const securityTab = (
    <div style={tabPane}><form onSubmit={(e) => { e.preventDefault(); handleSave(); }}><Space direction="vertical" size={0} style={{ width: '100%' }}>
      <div style={cardStyle}>
        <SectionHeader icon={<KeyOutlined />} title="Authentication" description="Controls how S3 clients authenticate with the proxy" />
        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Access Key ID</span>
          <Input value={accessKeyId} onChange={(e) => setAccessKeyId(e.target.value)} placeholder="AKIAIOSFODNN7EXAMPLE" style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }} />
          <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>Clients use this key to sign S3 requests. Leave empty to disable auth.</Text>
        </div>
        <div style={{ marginTop: 12 }}>
          <span style={labelStyle}>Secret Access Key</span>
          <input type="text" autoComplete="username" value={accessKeyId} readOnly aria-hidden="true" style={{ display: 'none' }} />
          <Input.Password value={secretAccessKey} onChange={(e) => setSecretAccessKey(e.target.value)} placeholder="Leave empty to keep current" autoComplete="off" style={{ ...inputRadius }} />
          <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>Shared secret for SigV4 signature verification.</Text>
        </div>
      </div>

      <PasswordChangeCard />

      <div style={{ ...cardStyle, cursor: 'pointer' }} onClick={() => setShowAdvancedSecurity(!showAdvancedSecurity)}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <SectionHeader icon={<LockOutlined />} title="Advanced Security" description="Rate limiting, session, and protocol settings" />
          <Switch checked={showAdvancedSecurity} onChange={setShowAdvancedSecurity} size="small" />
        </div>
      </div>

      {showAdvancedSecurity && (
        <>
          <div style={cardStyle}>
            <SectionHeader icon={<SafetyOutlined />} title="Session & Headers" />
            {readOnlyField('Trust Proxy Headers', config?.trust_proxy_headers ? 'Enabled' : 'Disabled', 'Trust X-Forwarded-For/X-Real-IP for rate limiting and IAM conditions. Disable if exposed directly to the internet.', 'restart required')}
            {readOnlyField('Session TTL (hours)', config?.session_ttl_hours, 'Admin session expiry. Lower = more secure, higher = less frequent re-login.', 'restart required')}
            {readOnlyField('Clock Skew Tolerance (seconds)', config?.clock_skew_seconds, 'Maximum allowed time difference between client and server clocks for SigV4 signatures. 300 = 5 minutes, matches AWS S3.', 'restart required')}
            {readOnlyField('Secure Cookies', config?.secure_cookies ? 'Enabled' : 'Disabled', 'Require HTTPS for admin session cookies. Disable only for local development.', 'restart required')}
            {readOnlyField('Debug Headers', config?.debug_headers ? 'Enabled' : 'Disabled', 'Expose x-amz-storage-type and x-deltaglider-cache headers. Disable in production.', 'restart required')}
          </div>

          <div style={cardStyle}>
            <SectionHeader icon={<LockOutlined />} title="Rate Limiting" description="Brute-force protection for authentication endpoints" />
            {readOnlyField('Max Attempts', config?.rate_limit_max_attempts, 'Failed auth attempts before IP lockout.', 'restart required')}
            {readOnlyField('Window (seconds)', config?.rate_limit_window_secs, 'Rolling time window for counting failures.', 'restart required')}
            {readOnlyField('Lockout Duration (seconds)', config?.rate_limit_lockout_secs, 'How long a locked-out IP is blocked.', 'restart required')}
            {readOnlyField('Replay Window (seconds)', config?.replay_window_secs, 'Duplicate SigV4 signature rejection window. Lower = fewer false positives.', 'restart required')}
          </div>
        </>
      )}

      {saveSection}
    </Space></form></div>
  );

  /* -- Tab: Logging ------------------------------------------------------- */
  const loggingTab = (
    <div style={tabPane}><form onSubmit={(e) => { e.preventDefault(); handleSave(); }}><Space direction="vertical" size={0} style={{ width: '100%' }}>
      <div style={cardStyle}>
        <SectionHeader icon={<ControlOutlined />} title="Log Level" description="Controls verbosity of proxy logs. Changes take effect immediately." />
        <div style={{ marginTop: 16 }}>
          <Radio.Group
            value={logLevelCustom ? '__custom__' : logLevel}
            onChange={(e) => {
              const val = e.target.value;
              if (val === '__custom__') { setLogLevelCustom(true); } else { setLogLevelCustom(false); setLogLevel(val); }
            }}
            style={{ display: 'flex', flexWrap: 'wrap', gap: 0 }}
          >
            {LOG_LEVEL_PRESETS.map(p => (
              <Radio.Button key={p.value} value={p.value} style={{ fontSize: 13 }}>{p.label}</Radio.Button>
            ))}
          </Radio.Group>
        </div>
        {logLevelCustom && (
          <div style={{ marginTop: 12 }}>
            <span style={labelStyle}>Custom Filter (RUST_LOG syntax)</span>
            <Input value={logLevel} onChange={(e) => setLogLevel(e.target.value)} placeholder="deltaglider_proxy=debug,tower_http=info" style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }} />
            <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>Comma-separated tracing directives, e.g. &quot;deltaglider_proxy=debug,tower_http=warn&quot;</Text>
          </div>
        )}
      </div>
      {saveSection}
    </Space></form></div>
  );

  // When embedded in AdminPage, render just the requested tab content
  if (embeddedTab) {
    const tabMap: Record<string, React.ReactNode> = {
      connection: connectionTab,
      backend: backendTab,
      compression: compressionTab,
      limits: limitsTab,
      security: securityTab,
      logging: loggingTab,
      // Legacy alias
      proxy: compressionTab,
    };
    return (
      <div style={{ maxWidth: 640, margin: '0 auto', padding: 'clamp(16px, 3vw, 24px)' }}>
        {tabMap[embeddedTab] ?? null}
      </div>
    );
  }

  /* -- Tab items ---------------------------------------------------------- */

  const tabItems = [
    { key: 'backend', label: <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}><DatabaseOutlined aria-hidden="true" /><span>Backend</span></span>, children: backendTab },
    { key: 'compression', label: <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}><ControlOutlined aria-hidden="true" /><span>Compression</span></span>, children: compressionTab },
    { key: 'limits', label: <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}><CloudOutlined aria-hidden="true" /><span>Limits</span></span>, children: limitsTab },
    { key: 'security', label: <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}><LockOutlined aria-hidden="true" /><span>Security</span></span>, children: securityTab },
    { key: 'logging', label: <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}><ApiOutlined aria-hidden="true" /><span>Logging</span></span>, children: loggingTab },
  ];

  return (
    <div className="animate-fade-in" style={{ maxWidth: 800, width: '100%', margin: '0 auto', padding: 'clamp(16px, 3vw, 24px) clamp(12px, 2vw, 16px)' }}>
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
