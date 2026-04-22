import { useState, useEffect } from 'react';
import { Button, Input, Typography, Space, Alert, Spin, message } from 'antd';
import { ApiOutlined, WarningOutlined, CheckCircleOutlined, CopyOutlined } from '@ant-design/icons';
import { testConnection, setEndpoint, setCredentials, setBucket, initFromSession } from '../s3client';
import { adminLogin, loginAs, whoami, recoverDb } from '../adminApi';
import type { ExternalProviderInfo } from '../adminApi';
import OAuthProviderList from './OAuthProviderList';
import { detectDefaultEndpoint } from '../utils';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

interface Props {
  onConnect: () => void;
  showError?: boolean;
}

export default function ConnectPage({ onConnect, showError }: Props) {
  const { BORDER, TEXT_MUTED, TEXT_FAINT, TEXT_PRIMARY, TEXT_SECONDARY, ACCENT_BLUE } = useColors();
  const [accessKey, setAccessKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [adminPassword, setAdminPassword] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [authMode, setAuthMode] = useState<'bootstrap' | 'iam' | 'open' | null>(null);
  const [externalProviders, setExternalProviders] = useState<ExternalProviderInfo[]>([]);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [detecting, setDetecting] = useState(true);
  // Recovery wizard state — persist success in sessionStorage so refresh doesn't reset
  const [showRecovery, setShowRecovery] = useState(false);
  const [recoveryPassword, setRecoveryPassword] = useState('');
  const [recoveryLoading, setRecoveryLoading] = useState(false);
  const [recoveryError, setRecoveryError] = useState('');
  const [recoveredHash, setRecoveredHash] = useState<{ hash: string; base64: string } | null>(() => {
    try {
      const saved = sessionStorage.getItem('dg-recovered-hash');
      return saved ? JSON.parse(saved) : null;
    } catch { return null; }
  });
  const [messageApi, contextHolder] = message.useMessage();

  // Detect auth mode on mount — auto-connect in open mode, show recovery wizard if mismatch
  useEffect(() => {
    whoami()
      .then(async (info) => {
        setAuthMode(info.mode as 'bootstrap' | 'iam' | 'open');
        setExternalProviders(info.external_providers || []);
        if (info.config_db_mismatch) {
          setShowRecovery(true);
          setDetecting(false);
          return;
        }
        // In open access mode, auto-connect with the proxy's own endpoint (no credentials needed)
        if (info.mode === 'open') {
          const endpoint = detectDefaultEndpoint().replace(/\/+$/, '');
          setEndpoint(endpoint);
          setCredentials('anonymous', 'anonymous');
          const result = await testConnection(endpoint, 'anonymous', 'anonymous').catch(() => ({ ok: false } as const));
          if (result.ok && 'buckets' in result && result.buckets && result.buckets.length > 0) {
            setBucket(result.buckets[0]);
            onConnect();
            return;
          }
          // S3 backend unreachable in open mode — fall through to show connect page
          // so user can see the error and retry
          setDetecting(false);
          setError('Open access mode but S3 backend is unreachable. Check server configuration.');
          return;
        }
        setDetecting(false);
      })
      .catch(() => setDetecting(false));
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleConnect = async () => {
    setLoading(true);
    setError('');
    try {
      if (authMode === 'bootstrap') {
        // Bootstrap mode: login with password, session auto-provides S3 creds
        if (!adminPassword.trim()) {
          setError('Bootstrap password is required');
          setLoading(false);
          return;
        }
        const adminResult = await adminLogin(adminPassword);
        if (!adminResult.ok) {
          setError(`Login failed: ${adminResult.error || 'Invalid password'}`);
          setLoading(false);
          return;
        }

        // Check for config DB mismatch after successful login
        const info = await whoami();
        if (info.config_db_mismatch) {
          setShowRecovery(true);
          setLoading(false);
          return;
        }

        const restored = await initFromSession();
        if (restored) {
          onConnect();
          return;
        }
        // Session didn't provide creds — shouldn't happen in bootstrap, but fall through
        setError('Login succeeded but no S3 credentials available. Check server config.');
        setLoading(false);
        return;
      }

      // IAM mode: connect with S3 credentials
      const cleanEndpoint = detectDefaultEndpoint().replace(/\/+$/, '');
      if (!accessKey.trim() || !secretKey.trim()) {
        setError('Access Key and Secret Key are required');
        setLoading(false);
        return;
      }
      const result = await testConnection(cleanEndpoint, accessKey, secretKey);
      if (!result.ok) {
        setError(`Connection failed: ${result.error || 'Invalid credentials'}`);
        setLoading(false);
        return;
      }

      setEndpoint(cleanEndpoint);
      setCredentials(accessKey, secretKey);
      if (result.buckets && result.buckets.length > 0) {
        setBucket(result.buckets[0]);
      }

      // Try to establish admin session (best-effort — only succeeds for admin users)
      loginAs(accessKey, secretKey).catch(() => {});

      onConnect();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Connection failed');
    } finally {
      setLoading(false);
    }
  };

  const handleRecover = async () => {
    if (!recoveryPassword.trim()) return;
    setRecoveryLoading(true);
    setRecoveryError('');
    try {
      const result = await recoverDb(recoveryPassword);
      if (result.success && result.correct_hash) {
        const recovered = {
          hash: result.correct_hash,
          base64: result.correct_hash_base64 || '',
        };
        setRecoveredHash(recovered);
        try { sessionStorage.setItem('dg-recovered-hash', JSON.stringify(recovered)); } catch { /* Safari private mode — fine to skip */ }
      } else {
        setRecoveryError(result.error || 'Password does not match');
      }
    } catch (e) {
      setRecoveryError(e instanceof Error ? e.message : 'Recovery failed');
    } finally {
      setRecoveryLoading(false);
    }
  };

  const copyToClipboard = (text: string, label: string) => {
    navigator.clipboard.writeText(text).then(() => {
      messageApi.success(`${label} copied to clipboard`);
    });
  };

  const isBootstrap = authMode === 'bootstrap';
  const canSubmit = isBootstrap ? adminPassword.trim() : (accessKey.trim() && secretKey.trim());

  const inputStyle = {
    background: 'var(--input-bg)',
    borderColor: BORDER,
    borderRadius: 10,
    height: 44,
    fontFamily: "var(--font-mono)" as const,
    fontSize: 13,
  };

  if (detecting) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: '100vh' }}>
        <Spin size="large" />
      </div>
    );
  }

  // Recovery wizard
  if (showRecovery) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: '100vh', padding: 24 }}>
        {contextHolder}
        <div className="glass-card animate-fade-in" style={{ borderRadius: 14, padding: 'clamp(28px, 4vw, 40px)', width: '100%', maxWidth: 520 }}>
          <Space direction="vertical" size="large" style={{ width: '100%' }}>
            {recoveredHash ? (
              <>
                <div>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 12 }}>
                    <CheckCircleOutlined style={{ fontSize: 28, color: 'var(--accent-success)', flexShrink: 0 }} />
                    <div style={{ fontSize: 20, fontWeight: 700, color: TEXT_PRIMARY, fontFamily: "var(--font-ui)" }}>
                      Database Recovered
                    </div>
                  </div>
                  <div style={{ color: TEXT_SECONDARY, fontSize: 14, fontFamily: "var(--font-ui)", lineHeight: 1.7 }}>
                    Update your configuration with the hash below, then restart the server.
                  </div>
                </div>
                <div style={{ background: 'var(--input-bg)', borderRadius: 10, padding: 16 }}>
                  <div style={{ marginBottom: 12 }}>
                    <label style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Hash</label>
                    <div style={{ display: 'flex', gap: 8, marginTop: 4 }}>
                      <Input value={recoveredHash.hash} readOnly style={{ ...inputStyle, flex: 1, fontSize: 11 }} />
                      <Button icon={<CopyOutlined />} onClick={() => copyToClipboard(recoveredHash.hash, 'Hash')} />
                    </div>
                  </div>
                  <div>
                    <label style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Base64 (for Docker / env vars)</label>
                    <div style={{ display: 'flex', gap: 8, marginTop: 4 }}>
                      <Input value={recoveredHash.base64} readOnly style={{ ...inputStyle, flex: 1, fontSize: 11 }} />
                      <Button icon={<CopyOutlined />} onClick={() => copyToClipboard(recoveredHash.base64, 'Base64 hash')} />
                    </div>
                  </div>
                </div>
                <Alert type="info" showIcon message={
                  <span style={{ fontFamily: "var(--font-ui)", fontSize: 12 }}>
                    Set <code style={{ fontFamily: "var(--font-mono)" }}>DGP_BOOTSTRAP_PASSWORD_HASH</code> in your environment
                    or <code style={{ fontFamily: "var(--font-mono)" }}>advanced.bootstrap_password_hash</code> in your YAML config, then restart.
                  </span>
                } />
              </>
            ) : (
              <>
                <div>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 12 }}>
                    <WarningOutlined style={{ fontSize: 28, color: 'var(--accent-warning)', flexShrink: 0 }} />
                    <div style={{ fontSize: 20, fontWeight: 700, color: TEXT_PRIMARY, fontFamily: "var(--font-ui)" }}>
                      Config Database Locked
                    </div>
                  </div>
                  <div style={{ color: TEXT_SECONDARY, fontSize: 14, lineHeight: 1.7, fontFamily: "var(--font-ui)" }}>
                    The bootstrap password hash in your configuration does not match the
                    encryption key of the existing IAM database. S3 API access is blocked until resolved.
                  </div>
                  <div style={{ color: TEXT_MUTED, fontSize: 13, marginTop: 12, lineHeight: 1.7, fontFamily: "var(--font-ui)" }}>
                    Paste the original <code style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: ACCENT_BLUE }}>DGP_BOOTSTRAP_PASSWORD_HASH</code> value
                    below. Check your previous deployment config, environment variables,
                    or <code style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: ACCENT_BLUE }}>.deltaglider_bootstrap_hash</code> file.
                  </div>
                </div>
                {recoveryError && <Alert type="error" message={recoveryError} showIcon />}
                <div>
                  <label style={{ fontSize: 13, fontWeight: 600, color: TEXT_SECONDARY, fontFamily: "var(--font-ui)", marginBottom: 6, display: 'block' }}>
                    Original Bootstrap Password Hash
                  </label>
                  <Input.TextArea
                    value={recoveryPassword}
                    onChange={(e) => setRecoveryPassword(e.target.value)}
                    placeholder="$2b$12$... or base64-encoded hash"
                    autoFocus
                    rows={2}
                    style={{ ...inputStyle, height: 'auto', fontSize: 13 }}
                  />
                </div>
                <Button
                  type="primary"
                  block
                  size="large"
                  loading={recoveryLoading}
                  disabled={!recoveryPassword.trim()}
                  onClick={handleRecover}
                  style={{ height: 48, borderRadius: 10, fontWeight: 700, fontFamily: "var(--font-ui)", fontSize: 15, letterSpacing: '0.02em', marginTop: 4 }}
                >
                  Try Hash
                </Button>
              </>
            )}
          </Space>
        </div>
      </div>
    );
  }

  return (
    <div style={{
      display: 'flex',
      justifyContent: 'center',
      alignItems: 'center',
      minHeight: '100vh',
      padding: 24,
    }}>
      <div
        className="glass-card animate-fade-in"
        style={{
          borderRadius: 14,
          padding: 'clamp(28px, 4vw, 40px)',
          width: '100%',
          maxWidth: 440,
        }}
      >
        <Space direction="vertical" size="large" style={{ width: '100%' }}>
          <div style={{ textAlign: 'center' }}>
            <div style={{
              width: 56,
              height: 56,
              borderRadius: 14,
              background: `linear-gradient(135deg, ${ACCENT_BLUE}22, ${ACCENT_BLUE}08)`,
              border: `1px solid ${ACCENT_BLUE}33`,
              display: 'inline-flex',
              alignItems: 'center',
              justifyContent: 'center',
              marginBottom: 16,
            }}>
              <ApiOutlined style={{ fontSize: 24, color: ACCENT_BLUE }} />
            </div>
            <div style={{ fontSize: 18, fontWeight: 800, letterSpacing: 3, color: TEXT_PRIMARY, lineHeight: 1.2, fontFamily: "var(--font-ui)" }}>
              DELTAGLIDER
            </div>
            <div style={{ fontSize: 12, fontWeight: 600, letterSpacing: 2.5, color: ACCENT_BLUE, textTransform: 'uppercase', marginTop: 3, fontFamily: "var(--font-mono)" }}>
              Proxy
            </div>
            <Text style={{ color: TEXT_MUTED, fontSize: 13, display: 'block', marginTop: 12 }}>
              {externalProviders.length > 0
                ? 'Sign in to continue.'
                : isBootstrap
                  ? 'Enter the bootstrap password to sign in.'
                  : 'Sign in with your S3 credentials.'}
            </Text>
          </div>

          {showError && !error && (
            <Alert type="warning" message="Stored credentials are invalid or the endpoint is unreachable." showIcon />
          )}
          {error && <Alert type="error" message={error} showIcon />}

          {/* OAuth provider buttons — shown prominently when available */}
          {externalProviders.length > 0 && (
            <OAuthProviderList
              providers={externalProviders}
              nextUrl={window.location.pathname}
              height={48}
              fontSize={15}
            />
          )}

          {/* Credential form — collapsible when OAuth is available */}
          {externalProviders.length > 0 && !showAdvanced ? (
            <div style={{ textAlign: 'center' }}>
              <Button
                type="link"
                size="small"
                onClick={() => setShowAdvanced(true)}
                style={{ color: TEXT_MUTED, fontSize: 12 }}
              >
                Sign in with credentials instead
              </Button>
            </div>
          ) : (
            <>
              {externalProviders.length > 0 && (
                <div style={{ display: 'flex', alignItems: 'center', gap: 12, margin: '4px 0' }}>
                  <div style={{ flex: 1, height: 1, background: BORDER }} />
                  <Text style={{ color: TEXT_MUTED, fontSize: 11 }}>or use credentials</Text>
                  <div style={{ flex: 1, height: 1, background: BORDER }} />
                </div>
              )}

              {isBootstrap ? (
                /* Bootstrap mode: password only */
                <div>
                  <label style={{ fontSize: 12, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)", marginBottom: 4, display: 'block' }}>
                    Bootstrap Password
                  </label>
                  <Input.Password
                    value={adminPassword}
                    onChange={(e) => setAdminPassword(e.target.value)}
                    onPressEnter={handleConnect}
                    placeholder="Bootstrap password"
                    size="large"
                    autoFocus={externalProviders.length === 0}
                    style={inputStyle}
                  />
                  <Text style={{ color: TEXT_FAINT, fontSize: 11, marginTop: 4, display: 'block' }}>
                    The admin password configured at deployment
                  </Text>
                </div>
              ) : (
                /* IAM mode: access key + secret key */
                <>
                  <div>
                    <label style={{ fontSize: 12, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)", marginBottom: 4, display: 'block' }}>
                      Access Key ID
                    </label>
                    <Input
                      value={accessKey}
                      onChange={(e) => setAccessKey(e.target.value)}
                      placeholder="Access Key ID"
                      size="large"
                      autoFocus={externalProviders.length === 0}
                      style={inputStyle}
                    />
                  </div>

                  <div>
                    <label style={{ fontSize: 12, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)", marginBottom: 4, display: 'block' }}>
                      Secret Access Key
                    </label>
                    <Input.Password
                      value={secretKey}
                      onChange={(e) => setSecretKey(e.target.value)}
                      onPressEnter={handleConnect}
                      placeholder="Secret Access Key"
                      size="large"
                      style={inputStyle}
                    />
                  </div>
                </>
              )}

              <Button
                type="primary"
                block
                size="large"
                loading={loading}
                disabled={!canSubmit}
                onClick={handleConnect}
                style={{
                  height: 48,
                  borderRadius: 10,
                  fontWeight: 700,
                  fontFamily: "var(--font-ui)",
                  fontSize: 15,
                  letterSpacing: '0.02em',
                  marginTop: 4,
                }}
              >
                {isBootstrap ? 'Sign In' : 'Connect'}
              </Button>
            </>
          )}
        </Space>
      </div>
    </div>
  );
}
