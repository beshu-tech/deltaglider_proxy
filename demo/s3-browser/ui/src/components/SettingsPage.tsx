import { useState, useEffect } from 'react';
import { Button, Input, Radio, Switch, Typography, Space, Alert, Spin } from 'antd';
import { SaveOutlined, LockOutlined, WarningOutlined, DatabaseOutlined, ControlOutlined, SafetyOutlined, KeyOutlined, ApiOutlined } from '@ant-design/icons';
import type { AdminConfig, TestS3Response } from '../adminApi';
import { getAdminConfig, updateAdminConfig, testS3Connection } from '../adminApi';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import PasswordChangeCard from './PasswordChangeCard';


const { Text } = Typography;

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
  onSessionExpired?: () => void;
  /** Which tab to render. Used by AdminPage. */
  embeddedTab: string;
}

/* -- SettingsPage (main) -------------------------------------------------- */

export default function SettingsPage({ onSessionExpired, embeddedTab }: Props) {
  const colors = useColors();
  const { cardStyle, labelStyle, inputRadius } = useCardStyles();

  const [config, setConfig] = useState<AdminConfig | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [saveResult, setSaveResult] = useState<{ warnings: string[]; requires_restart: boolean } | null>(null);

  const [accessKeyId, setAccessKeyId] = useState('');
  const [secretAccessKey, setSecretAccessKey] = useState('');

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

  // Taint detection: fields whose runtime value differs from the
  // on-disk config file (YAML, or legacy TOML — whichever extension
  // the server was started with).
  const [taintedFields, setTaintedFields] = useState<Set<string>>(new Set());

  /** Render an amber "modified" indicator next to a field label when tainted. */
  const taintBadge = (field: string) =>
    taintedFields.has(field)
      ? <span title="Value differs from config file on disk" style={{ fontSize: 10, color: colors.ACCENT_AMBER, marginLeft: 6, fontWeight: 400, cursor: 'help' }}>modified</span>
      : null;

  useEffect(() => {
    getAdminConfig().then((cfg) => {
      if (cfg) {
        setConfig(cfg);
        setAccessKeyId(cfg.access_key_id || '');
        setBackendType(cfg.backend_type);
        setOriginalBackendType(cfg.backend_type);
        setBackendPath(cfg.backend_path || './data');
        setBackendEndpoint(cfg.backend_endpoint || '');
        setBackendRegion(cfg.backend_region || 'us-east-1');
        setBackendForcePathStyle(cfg.backend_force_path_style ?? true);
        // Tainted fields
        setTaintedFields(new Set(cfg.tainted_fields || []));
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
    try {
      const payload: Record<string, unknown> = {
        access_key_id: accessKeyId || null,
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
      // Re-fetch config to update taint status — save persists to the
      // on-disk config file (YAML or legacy TOML, matching the
      // extension the server was started with), so taint should clear.
      const refreshed = await getAdminConfig();
      if (refreshed) setTaintedFields(new Set(refreshed.tainted_fields || []));
      setOriginalBackendType(backendType);
      setBeAccessKeyId('');
      setBeSecretAccessKey('');
    } catch (e) {
      setSaveResult({ warnings: [e instanceof Error ? e.message : 'Failed to save configuration'], requires_restart: false });
    } finally {
      setSaving(false);
    }
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
        <div style={{ marginTop: 16 }}>
          <Button type="primary" onClick={() => onSessionExpired?.()}>Log in again</Button>
        </div>
      </div>
    );
  }

  /* Stable wrapper — fixed width prevents horizontal jump between tabs;
     minHeight prevents vertical collapse on short tabs (Security). */
  const tabPane: React.CSSProperties = { width: '100%', minWidth: 0, minHeight: 420 };

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
        <span style={labelStyle}>Backend Type {taintBadge('backend_type')}</span>
        <Radio.Group
          value={backendType}
          onChange={(e) => setBackendType(e.target.value)}
          style={{ display: 'flex', gap: 0 }}
        >
          <Radio.Button value="filesystem" style={{ fontSize: 13 }}>Filesystem</Radio.Button>
          <Radio.Button value="s3" style={{ fontSize: 13 }}>S3</Radio.Button>
        </Radio.Group>
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
          <span style={labelStyle}>Data Directory {taintBadge('backend_path')}</span>
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
            <span style={labelStyle}>S3 Endpoint {taintBadge('backend_endpoint')}</span>
            <Input
              value={backendEndpoint}
              onChange={(e) => setBackendEndpoint(e.target.value)}
              placeholder="http://localhost:9000 (leave empty for AWS default)"
              style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
            />
          </div>
          <div style={{ marginTop: 12 }}>
            <span style={labelStyle}>S3 Region {taintBadge('backend_region')}</span>
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

      {saveSection}
    </form></div>
  );

  /* -- Tab: Proxy --------------------------------------------------------- */

  /* -- Helper: read-only display field ------------------------------------ */
  //
  // `configSource` describes WHERE this field can be set. The three
  // accurate shapes:
  //   * `{ env: "DGP_X=..." }`              -- env-var-only (the server
  //                                            reads env directly; no
  //                                            YAML key exists).
  //   * `{ yaml: "advanced.x", env: "DGP_X=..." }`
  //                                         -- YAML field with env
  //                                            override. Render both.
  //   * undefined                           -- derived / informational;
  //                                            no source surface.
  //
  // Historically this helper carried a fabricated `toml: ...` line for
  // fields that were env-only; the label claimed `foo = 300` in
  // deltaglider_proxy.toml was a valid setting when the server never
  // read it from the file at all. Fixed by switching to accurate
  // source discrimination.
  const readOnlyField = (
    label: string,
    value: string | number | boolean | undefined,
    description?: string,
    badge?: string,
    configSource?: { yaml?: string; env: string }
  ) => (
    <div style={{ marginTop: 16 }}>
      <span style={labelStyle}>
        {label}
        {badge && <span style={{ fontSize: 10, color: colors.ACCENT_AMBER, marginLeft: 8, fontWeight: 400 }}>{badge}</span>}
      </span>
      <Input value={String(value ?? '—')} readOnly style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13, opacity: 0.7 }} />
      {description && <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)" }}>{description}</Text>}
      {configSource && (
        <div style={{ marginTop: 4, padding: '6px 10px', background: colors.BG_ELEVATED, border: `1px solid ${colors.BORDER}`, borderRadius: 6, fontSize: 11, fontFamily: 'var(--font-mono)', lineHeight: 1.6, color: colors.TEXT_MUTED }}>
          {configSource.yaml && (
            <>
              <span style={{ color: colors.TEXT_SECONDARY }}>YAML:</span> {configSource.yaml}<br />
            </>
          )}
          <span style={{ color: colors.TEXT_SECONDARY }}>ENV:</span>&nbsp; {configSource.env}
          {!configSource.yaml && (
            <>
              <br />
              <span style={{ fontStyle: 'italic', fontSize: 10 }}>
                environment-variable only — no YAML/config-file field.
              </span>
            </>
          )}
        </div>
      )}
    </div>
  );

  /* -- Tab: Compression — removed, merged into BackendsPanel -------------- */

  /* -- Tab: Limits -------------------------------------------------------- */
  const limitsTab = (
    <div style={tabPane}><Space direction="vertical" size={0} style={{ width: '100%' }}>
      <div style={cardStyle}>
        <SectionHeader icon={<SafetyOutlined />} title="Request Limits" description="Protect the server from overload and abuse. All require restart to change." />
        {readOnlyField('Request Timeout (seconds)', config?.request_timeout_secs, 'Maximum time for any single request. Returns HTTP 504 Gateway Timeout when exceeded.', 'restart required', { env: 'DGP_REQUEST_TIMEOUT_SECS=300' })}
        {readOnlyField('Max Concurrent Requests', config?.max_concurrent_requests, 'Maximum in-flight HTTP requests. Additional requests queue until a slot opens.', 'restart required', { env: 'DGP_MAX_CONCURRENT_REQUESTS=1024' })}
        {readOnlyField('Max Multipart Uploads', config?.max_multipart_uploads, 'Maximum concurrent multipart uploads. Each holds part data in memory.', 'restart required', { env: 'DGP_MAX_MULTIPART_UPLOADS=1000' })}
      </div>
    </Space></div>
  );

  /* -- Tab: Security ------------------------------------------------------ */
  const securityTab = (
    <div style={tabPane}><form onSubmit={(e) => { e.preventDefault(); handleSave(); }}><Space direction="vertical" size={0} style={{ width: '100%' }}>
      <div style={cardStyle}>
        <SectionHeader icon={<KeyOutlined />} title="Authentication" description="Controls how S3 clients authenticate with the proxy" />
        <div style={{ marginTop: 16 }}>
          <span style={labelStyle}>Access Key ID {taintBadge('access_key_id')}</span>
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
            {readOnlyField('Trust Proxy Headers', config?.trust_proxy_headers ? 'Enabled' : 'Disabled', 'Trust X-Forwarded-For/X-Real-IP for rate limiting and IAM conditions. Disable if exposed directly to the internet.', 'restart required', { env: 'DGP_TRUST_PROXY_HEADERS=true' })}
            {readOnlyField('Session TTL (hours)', config?.session_ttl_hours, 'Admin session expiry. Lower = more secure, higher = less frequent re-login.', 'restart required', { env: 'DGP_SESSION_TTL_HOURS=4' })}
            {readOnlyField('Clock Skew Tolerance (seconds)', config?.clock_skew_seconds, 'Maximum allowed time difference between client and server clocks for SigV4 signatures. 300 = 5 minutes, matches AWS S3.', 'restart required', { env: 'DGP_CLOCK_SKEW_SECONDS=300' })}
            {readOnlyField('Secure Cookies', config?.secure_cookies ? 'Enabled' : 'Disabled', 'Require HTTPS for admin session cookies. Disable only for local development.', 'restart required', { env: 'DGP_SECURE_COOKIES=true' })}
            {readOnlyField('Debug Headers', config?.debug_headers ? 'Enabled' : 'Disabled', 'Expose x-amz-storage-type and x-deltaglider-cache headers. Disable in production.', 'restart required', { env: 'DGP_DEBUG_HEADERS=false' })}
          </div>

          <div style={cardStyle}>
            <SectionHeader icon={<LockOutlined />} title="Rate Limiting" description="Brute-force protection for authentication endpoints" />
            {readOnlyField('Max Attempts', config?.rate_limit_max_attempts, 'Failed auth attempts before IP lockout.', 'restart required', { env: 'DGP_RATE_LIMIT_MAX_ATTEMPTS=100' })}
            {readOnlyField('Window (seconds)', config?.rate_limit_window_secs, 'Rolling time window for counting failures.', 'restart required', { env: 'DGP_RATE_LIMIT_WINDOW_SECS=300' })}
            {readOnlyField('Lockout Duration (seconds)', config?.rate_limit_lockout_secs, 'How long a locked-out IP is blocked.', 'restart required', { env: 'DGP_RATE_LIMIT_LOCKOUT_SECS=600' })}
            {readOnlyField('Replay Window (seconds)', config?.replay_window_secs, 'Duplicate SigV4 signature rejection window. Lower = fewer false positives.', 'restart required', { env: 'DGP_REPLAY_WINDOW_SECS=2' })}
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
        <SectionHeader icon={<ControlOutlined />} title={<>Log Level {taintBadge('log_level')}</>} description="Controls verbosity of proxy logs. Changes take effect immediately." />
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
  const tabMap: Record<string, React.ReactNode> = {
    backend: backendTab,
    limits: limitsTab,
    security: securityTab,
    logging: loggingTab,
  };
  return (
    <div style={{ maxWidth: 640, margin: '0 auto', padding: 'clamp(16px, 3vw, 24px)' }}>
      {tabMap[embeddedTab] ?? null}
    </div>
  );
}
