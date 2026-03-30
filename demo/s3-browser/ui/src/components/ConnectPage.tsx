import { useState, useEffect } from 'react';
import { Button, Input, Typography, Space, Alert, Spin } from 'antd';
import { ApiOutlined } from '@ant-design/icons';
import { testConnection, setEndpoint, setCredentials, setBucket, initFromSession } from '../s3client';
import { adminLogin, whoami } from '../adminApi';
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
