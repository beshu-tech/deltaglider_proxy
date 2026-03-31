import { useState, useEffect } from 'react';
import { Button, Input, Typography, Space, Alert, Spin, message } from 'antd';
import { ApiOutlined, WarningOutlined, CheckCircleOutlined, CopyOutlined } from '@ant-design/icons';
import { testConnection, setEndpoint, setCredentials, setBucket, initFromSession } from '../s3client';
import { adminLogin, whoami, recoverDb } from '../adminApi';
import { detectDefaultEndpoint } from '../utils';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

interface Props {
  onConnect: () => void;
  showError?: boolean;
}

export default function ConnectPage({ onConnect, showError }: Props) {
  const { BORDER, TEXT_MUTED, TEXT_FAINT, TEXT_PRIMARY, ACCENT_BLUE } = useColors();
  const [accessKey, setAccessKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [adminPassword, setAdminPassword] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [authMode, setAuthMode] = useState<'bootstrap' | 'iam' | 'open' | null>(null);
  const [detecting, setDetecting] = useState(true);
  // Recovery wizard state
  const [showRecovery, setShowRecovery] = useState(false);
  const [recoveryPassword, setRecoveryPassword] = useState('');
  const [recoveryLoading, setRecoveryLoading] = useState(false);
  const [recoveryError, setRecoveryError] = useState('');
  const [recoveredHash, setRecoveredHash] = useState<{ hash: string; base64: string } | null>(null);
  const [messageApi, contextHolder] = message.useMessage();

  // Detect auth mode on mount
  useEffect(() => {
    whoami()
      .then(info => setAuthMode(info.mode as 'bootstrap' | 'iam' | 'open'))
      .catch(() => {})
      .finally(() => setDetecting(false));
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
        setRecoveredHash({
          hash: result.correct_hash,
          base64: result.correct_hash_base64 || '',
        });
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
              /* Success state */
              <>
                <div style={{ textAlign: 'center' }}>
                  <CheckCircleOutlined style={{ fontSize: 48, color: '#52c41a', marginBottom: 16 }} />
                  <div style={{ fontSize: 18, fontWeight: 700, color: TEXT_PRIMARY }}>Database Recovered</div>
                  <Text style={{ color: TEXT_MUTED, fontSize: 13, display: 'block', marginTop: 8 }}>
                    Update your configuration with this hash, then restart the server.
                  </Text>
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
                    <label style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Base64 (for Docker/env vars)</label>
                    <div style={{ display: 'flex', gap: 8, marginTop: 4 }}>
                      <Input value={recoveredHash.base64} readOnly style={{ ...inputStyle, flex: 1, fontSize: 11 }} />
                      <Button icon={<CopyOutlined />} onClick={() => copyToClipboard(recoveredHash.base64, 'Base64 hash')} />
                    </div>
                  </div>
                </div>
                <Alert type="info" showIcon message={
                  <span>Set <code>DGP_BOOTSTRAP_PASSWORD_HASH</code> in your environment or <code>bootstrap_password_hash</code> in your TOML config, then restart.</span>
                } />
              </>
            ) : (
              /* Recovery form */
              <>
                <div style={{ textAlign: 'center' }}>
                  <WarningOutlined style={{ fontSize: 48, color: '#faad14', marginBottom: 16 }} />
                  <div style={{ fontSize: 18, fontWeight: 700, color: TEXT_PRIMARY }}>Config Database Locked</div>
                  <Text style={{ color: TEXT_MUTED, fontSize: 13, display: 'block', marginTop: 8 }}>
                    The bootstrap password in your configuration does not match the encryption key of the existing IAM database. No IAM changes can be made until this is resolved.
                  </Text>
                </div>
                {recoveryError && <Alert type="error" message={recoveryError} showIcon />}
                <div>
                  <label style={{ fontSize: 12, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)", marginBottom: 4, display: 'block' }}>
                    Original Bootstrap Password
                  </label>
                  <Input.Password
                    value={recoveryPassword}
                    onChange={(e) => setRecoveryPassword(e.target.value)}
                    onPressEnter={handleRecover}
                    placeholder="Enter the password that was used to create the database"
                    size="large"
                    autoFocus
                    style={inputStyle}
                  />
                </div>
                <Button
                  type="primary"
                  block
                  size="large"
                  loading={recoveryLoading}
                  disabled={!recoveryPassword.trim()}
                  onClick={handleRecover}
                  style={{ height: 48, borderRadius: 10, fontWeight: 700, fontFamily: "var(--font-ui)", fontSize: 15 }}
                >
                  Try Password
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
              {isBootstrap ? 'Enter the bootstrap password to sign in.' : 'Sign in with your S3 credentials.'}
            </Text>
          </div>

          {showError && !error && (
            <Alert type="warning" message="Stored credentials are invalid or the endpoint is unreachable." showIcon />
          )}
          {error && <Alert type="error" message={error} showIcon />}

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
                autoFocus
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
                  autoFocus
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
        </Space>
      </div>
    </div>
  );
}
