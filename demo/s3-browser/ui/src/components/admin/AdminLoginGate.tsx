import { useState } from 'react';
import { Alert, Button, Input, Space, Typography } from 'antd';
import { LockOutlined } from '@ant-design/icons';
import { adminLogin, type ExternalProviderInfo } from '../../adminApi';
import { initFromSession } from '../../s3client';
import { useColors } from '../../ThemeContext';
import OAuthProviderList from '../OAuthProviderList';

const { Text } = Typography;

/** IAM user without the admin action: nothing to sign in to. */
export function AdminAccessDenied({ onBack }: { onBack: () => void }) {
  const colors = useColors();
  return (
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', flex: 1, background: colors.BG_BASE }}>
      <div style={{ width: 380, padding: 40, textAlign: 'center' }}>
        <LockOutlined style={{ fontSize: 32, color: colors.ACCENT_RED, marginBottom: 12 }} />
        <div><Text strong style={{ fontSize: 18, fontFamily: 'var(--font-ui)' }}>Access Denied</Text></div>
        <Text type="secondary" style={{ fontSize: 13, display: 'block', marginTop: 8, marginBottom: 24 }}>
          Your account does not have admin permissions. Contact an administrator to grant you the &quot;admin&quot; action.
        </Text>
        <Button type="primary" onClick={onBack} style={{ borderRadius: 10 }}>Back to Browser</Button>
      </div>
    </div>
  );
}

interface LoginGateProps {
  externalProviders: ExternalProviderInfo[];
  /** A file-browser-only session is active (explains why Settings still asks). */
  s3BrowserSessionOnly: boolean;
  /** An error from the automatic sign-in attempt, shown until the next try. */
  initialError?: string;
  onAuthed: () => void;
  onBack: () => void;
}

/** Admin password sign-in, plus OAuth buttons when providers exist. */
export function AdminLoginGate({ externalProviders, s3BrowserSessionOnly, initialError = '', onAuthed, onBack }: LoginGateProps) {
  const colors = useColors();
  const [password, setPassword] = useState('');
  const [loginLoading, setLoginLoading] = useState(false);
  const [loginError, setLoginError] = useState(initialError);

  const handleLogin = async () => {
    if (loginLoading) return; // in-flight guard: prevents Enter + form-submit double-fire
    setLoginLoading(true);
    setLoginError('');
    try {
      const res = await adminLogin(password);
      if (res.ok) {
        onAuthed();
        setPassword('');
        // Bootstrap session may attach S3 creds (legacy keys or anonymous open-access).
        await initFromSession().catch(() => {});
      } else {
        setLoginError(res.error || 'Login failed');
        setPassword('');
      }
    } catch {
      setLoginError('Network error');
    } finally {
      setLoginLoading(false);
    }
  };

  return (
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', flex: 1, background: colors.BG_BASE }}>
      <form onSubmit={e => { e.preventDefault(); handleLogin(); }} style={{ width: 380, padding: 40 }}>
        <div style={{ textAlign: 'center', marginBottom: 24 }}>
          <LockOutlined style={{ fontSize: 32, color: colors.ACCENT_BLUE, marginBottom: 12 }} />
          <div><Text strong style={{ fontSize: 18, fontFamily: 'var(--font-ui)' }}>Admin Login</Text></div>
          <Text type="secondary" style={{ fontSize: 13 }}>
            {externalProviders.length > 0 ? 'Sign in to continue.' : 'Enter the admin password to continue.'}
          </Text>
        </div>
        {s3BrowserSessionOnly && (
          <Alert
            type="info"
            showIcon
            message="File browser session active"
            description="You are signed in for S3 browsing only. Use the admin password (or OAuth if configured) below to open a full administrator session."
            style={{ marginBottom: 16, borderRadius: 8 }}
          />
        )}
        {/* OAuth provider buttons */}
        {externalProviders.length > 0 && (
          <div style={{ marginBottom: 16 }}>
            <OAuthProviderList providers={externalProviders} nextUrl="/_/admin" />
            <div style={{ display: 'flex', alignItems: 'center', gap: 12, margin: '16px 0' }}>
              <div style={{ flex: 1, height: 1, background: colors.BORDER }} />
              <Text type="secondary" style={{ fontSize: 12 }}>or</Text>
              <div style={{ flex: 1, height: 1, background: colors.BORDER }} />
            </div>
          </div>
        )}
        {loginError && <Alert type="error" message={loginError} showIcon style={{ marginBottom: 16, borderRadius: 8 }} />}
        <Input.Password
          placeholder="Admin password"
          value={password}
          onChange={e => setPassword(e.target.value)}
          onPressEnter={handleLogin}
          size="large"
          autoFocus={externalProviders.length === 0}
          style={{ borderRadius: 10, marginBottom: 16 }}
        />
        <Space style={{ width: '100%' }} direction="vertical">
          <Button type="primary" htmlType="submit" block size="large" loading={loginLoading} disabled={!password}
            style={{ borderRadius: 10, height: 44, fontWeight: 600 }}>
            Sign In
          </Button>
          <Button type="text" block onClick={onBack} style={{ color: colors.TEXT_MUTED }}>Cancel</Button>
        </Space>
      </form>
    </div>
  );
}
