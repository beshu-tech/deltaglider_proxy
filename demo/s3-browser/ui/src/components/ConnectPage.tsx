import { useState } from 'react';
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
  const [endpoint, setEndpointVal] = useState(detectDefaultEndpoint());
  const [accessKey, setAccessKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [adminPassword, setAdminPassword] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [authMode, setAuthMode] = useState<'bootstrap' | 'iam' | 'open' | null>(null);

  const handleConnect = async () => {
    setLoading(true);
    setError('');
    try {
      // Step 1: Admin login first (if password provided) — this auto-populates
      // S3 credentials in the server-side session, so we may not need manual S3 creds.
      if (adminPassword) {
        const adminResult = await adminLogin(adminPassword);
        if (!adminResult.ok) {
          setError(`Admin login failed: ${adminResult.error || 'Invalid password'}`);
          setLoading(false);
          return;
        }

        // Try to restore S3 creds from the session (auto-populated by login)
        const restored = await initFromSession();
        if (restored) {
          // Login auto-connected us — discover buckets
          try {
            const info = await whoami(accessKey || undefined, secretKey || undefined);
            setAuthMode(info.mode);
          } catch { /* non-critical */ }
          onConnect();
          return;
        }
        // If no creds came back (open-access mode), fall through to manual S3 entry
      }

      // Step 2: Manual S3 connection (if no admin login, or admin had no S3 creds)
      const cleanEndpoint = endpoint.trim().replace(/\/+$/, '');
      if (!accessKey.trim() || !secretKey.trim()) {
        setError('S3 Access Key and Secret Key are required');
        setLoading(false);
        return;
      }
      const result = await testConnection(cleanEndpoint, accessKey, secretKey);
      if (!result.ok) {
        setError(`S3 connection failed: ${result.error || 'Unknown error'}`);
        setLoading(false);
        return;
      }

      // S3 succeeded — persist credentials to server session
      setEndpoint(cleanEndpoint);
      setCredentials(accessKey, secretKey);
      if (result.buckets && result.buckets.length > 0) {
        setBucket(result.buckets[0]);
      }

      // Detect auth mode
      try {
        const info = await whoami(accessKey, secretKey);
        setAuthMode(info.mode);
      } catch { /* non-critical */ }

      onConnect();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Connection failed');
    } finally {
      setLoading(false);
    }
  };

  // Admin password alone is enough (login auto-connects). S3 creds are needed only for manual connect.
  const canSubmit = adminPassword.trim() || (endpoint.trim() && accessKey.trim() && secretKey.trim());

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
        <Space orientation="vertical" size="large" style={{ width: '100%' }}>
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
            <Text style={{ color: TEXT_MUTED, fontSize: 13, display: 'block', marginTop: 12 }}>Connect to your S3-compatible endpoint.</Text>
          </div>

          {showError && !error && (
            <Alert type="warning" message="Stored credentials are invalid or the endpoint is unreachable." showIcon />
          )}
          {error && <Alert type="error" message={error} showIcon />}

          <div>
            <label style={{ fontSize: 12, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)", marginBottom: 4, display: 'block' }}>
              Endpoint URL
            </label>
            <Input
              value={endpoint}
              onChange={(e) => setEndpointVal(e.target.value)}
              placeholder="http://localhost:9000"
              size="large"
              style={{
                background: 'var(--input-bg)',
                borderColor: BORDER,
                borderRadius: 10,
                height: 44,
                fontFamily: "var(--font-mono)",
                fontSize: 13,
              }}
            />
          </div>

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
              style={{
                background: 'var(--input-bg)',
                borderColor: BORDER,
                borderRadius: 10,
                height: 44,
                fontFamily: "var(--font-mono)",
                fontSize: 13,
              }}
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
              style={{
                background: 'var(--input-bg)',
                borderColor: BORDER,
                borderRadius: 10,
                height: 44,
                fontFamily: "var(--font-mono)",
                fontSize: 13,
              }}
            />
          </div>

          {authMode !== 'iam' && (
            <div>
              <label style={{ fontSize: 12, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)", marginBottom: 4, display: 'block' }}>
                Bootstrap Password
              </label>
              <Input.Password
                value={adminPassword}
                onChange={(e) => setAdminPassword(e.target.value)}
                onPressEnter={handleConnect}
                placeholder="Bootstrap password (optional)"
                size="large"
                style={{
                  background: 'var(--input-bg)',
                  borderColor: BORDER,
                  borderRadius: 10,
                  height: 44,
                  fontFamily: "var(--font-mono)",
                  fontSize: 13,
                }}
              />
              <Text style={{ color: TEXT_FAINT, fontSize: 11, marginTop: 4, display: 'block' }}>
                Required for admin settings before IAM users are created
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
