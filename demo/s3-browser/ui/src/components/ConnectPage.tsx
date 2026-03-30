import { useState, useEffect } from 'react';
import { Button, Input, Typography, Space, Alert } from 'antd';
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

  // Detect auth mode on mount (before user interacts)
  useEffect(() => {
    whoami().then(info => setAuthMode(info.mode as 'bootstrap' | 'iam' | 'open')).catch(() => {});
  }, []);

  const handleConnect = async () => {
    setLoading(true);
    setError('');
    try {
      // Step 1: Admin login first (if password provided)
      if (adminPassword) {
        const adminResult = await adminLogin(adminPassword);
        if (!adminResult.ok) {
          setError(`Admin login failed: ${adminResult.error || 'Invalid password'}`);
          setLoading(false);
          return;
        }

        // Try to restore S3 creds from the session
        const restored = await initFromSession();
        if (restored) {
          try {
            const info = await whoami();
            setAuthMode(info.mode as 'bootstrap' | 'iam' | 'open');
          } catch { /* non-critical */ }
          onConnect();
          return;
        }
      }

      // Step 2: Manual S3 connection
      const cleanEndpoint = detectDefaultEndpoint().replace(/\/+$/, '');
      if (!accessKey.trim() || !secretKey.trim()) {
        setError('Access Key and Secret Key are required');
        setLoading(false);
        return;
      }
      const result = await testConnection(cleanEndpoint, accessKey, secretKey);
      if (!result.ok) {
        setError(`Connection failed: ${result.error || 'Unknown error'}`);
        setLoading(false);
        return;
      }

      setEndpoint(cleanEndpoint);
      setCredentials(accessKey, secretKey);
      if (result.buckets && result.buckets.length > 0) {
        setBucket(result.buckets[0]);
      }

      try {
        const info = await whoami();
        setAuthMode(info.mode as 'bootstrap' | 'iam' | 'open');
      } catch { /* non-critical */ }

      onConnect();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Connection failed');
    } finally {
      setLoading(false);
    }
  };

  const showBootstrap = authMode === 'bootstrap' || authMode === null;
  const canSubmit = (showBootstrap && adminPassword.trim()) || (accessKey.trim() && secretKey.trim());

  const inputStyle = {
    background: 'var(--input-bg)',
    borderColor: BORDER,
    borderRadius: 10,
    height: 44,
    fontFamily: "var(--font-mono)" as const,
    fontSize: 13,
  };

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
              {showBootstrap ? 'Sign in with your credentials.' : 'Sign in with your S3 credentials.'}
            </Text>
          </div>

          {showError && !error && (
            <Alert type="warning" message="Stored credentials are invalid or the endpoint is unreachable." showIcon />
          )}
          {error && <Alert type="error" message={error} showIcon />}

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
              placeholder="Secret Access Key"
              size="large"
              onPressEnter={!showBootstrap ? handleConnect : undefined}
              style={inputStyle}
            />
          </div>

          {showBootstrap && (
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
                style={inputStyle}
              />
              <Text style={{ color: TEXT_FAINT, fontSize: 11, marginTop: 4, display: 'block' }}>
                Required for admin access before IAM users are created
              </Text>
            </div>
          )}

          <Button
            type="primary"
            block
            size="large"
            loading={loading}
            disabled={!canSubmit}
            onClick={handleConnect}
            onKeyDown={(e) => e.key === 'Enter' && canSubmit && handleConnect()}
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
            Connect
          </Button>
        </Space>
      </div>
    </div>
  );
}
