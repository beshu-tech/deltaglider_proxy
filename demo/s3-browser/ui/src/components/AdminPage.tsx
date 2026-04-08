import { useState, useEffect, useCallback } from 'react';
import { Typography, Button, Input, Alert, Space, Spin, message } from 'antd';
import { checkSession, adminLogin, whoami, loginAs, exportBackup, importBackup, type ExternalProviderInfo } from '../adminApi';
import { getCredentials } from '../s3client';
import {
  CloudOutlined,
  CloudServerOutlined,
  DatabaseOutlined,
  TeamOutlined,
  FolderOutlined,
  LockOutlined,
  DashboardOutlined,
  DownloadOutlined,
  UploadOutlined,
  SafetyOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import FullScreenHeader from './FullScreenHeader';
import SettingsPage from './SettingsPage';
import UsersPanel from './UsersPanel';
import GroupsPanel from './GroupsPanel';
import AuthenticationPanel from './AuthenticationPanel';
import BackendsPanel from './BackendsPanel';
import MetricsPage from './MetricsPage';
import OAuthProviderList from './OAuthProviderList';
import { useNavigation } from '../NavigationContext';
import TabHeader from './TabHeader';

const { Text } = Typography;

const VALID_TABS = new Set(['users', 'groups', 'auth', 'metrics', 'backends', 'backend', 'limits', 'security', 'logging']);
// Redirect old compression tab URLs to backends
const TAB_REDIRECTS: Record<string, string> = { compression: 'backends' };

const TABS: Array<{ key: string; label: string; icon: React.ReactNode; title: string; description: string }> = [
  { key: 'users', label: 'Users', icon: <TeamOutlined />, title: 'User Management', description: 'Create and manage IAM users with fine-grained S3 permissions. Each user gets their own access key and secret for SigV4 authentication.' },
  { key: 'groups', label: 'Groups', icon: <FolderOutlined />, title: 'Groups', description: 'Organize users into groups with shared permission policies. Users inherit all permissions from their groups.' },
  { key: 'auth', label: 'Authentication', icon: <SafetyOutlined />, title: 'External Authentication', description: 'Configure OAuth/OIDC providers for single sign-on. Map employee emails to groups for automatic permission assignment.' },
  { key: 'metrics', label: 'Metrics', icon: <DashboardOutlined />, title: 'Metrics & Monitoring', description: 'Live Prometheus metrics for request traffic, cache performance, delta compression ratios, and storage savings.' },
  { key: 'backends', label: 'Storage', icon: <CloudServerOutlined />, title: 'Storage & Compression', description: 'Configure storage backends, delta compression defaults, per-bucket routing and policies.' },
  { key: 'backend', label: 'Connection', icon: <DatabaseOutlined />, title: 'Primary Backend', description: 'Configure the default storage backend connection. This is used when no named backends are configured or as fallback.' },
  { key: 'limits', label: 'Limits', icon: <CloudOutlined />, title: 'Request Limits', description: 'Protect the server from overload with request timeouts, concurrency limits, and multipart upload caps.' },
  { key: 'security', label: 'Security', icon: <LockOutlined />, title: 'Security & Sessions', description: 'Configure SigV4 clock skew tolerance, replay detection, rate limiting, session TTL, and cookie security.' },
  { key: 'logging', label: 'Logging', icon: <DatabaseOutlined />, title: 'Logging', description: 'Control log verbosity at runtime. Changes take effect immediately without restart.' },
];

interface AdminPageProps {
  onBack: () => void;
  onSessionExpired?: () => void;
  subPath?: string;
}

export default function AdminPage({ onBack, onSessionExpired, subPath }: AdminPageProps) {
  const colors = useColors();
  const { navigate } = useNavigation();

  // Derive active tab from URL sub-path, with redirects for renamed tabs
  const rawTab = subPath || '';
  const activeTab = VALID_TABS.has(rawTab) ? rawTab : TAB_REDIRECTS[rawTab] || 'users';
  const setActiveTab = useCallback((tab: string) => {
    navigate(`admin/${tab}`);
  }, [navigate]);

  const [authed, setAuthed] = useState(false);
  const [checkingSession, setCheckingSession] = useState(true);
  const [externalProviders, setExternalProviders] = useState<ExternalProviderInfo[]>([]);
  const [accessDenied, setAccessDenied] = useState(false);
  const [password, setPassword] = useState('');
  const [loginLoading, setLoginLoading] = useState(false);
  const [pendingGroupId, setPendingGroupId] = useState<number | null>(null);
  const [loginError, setLoginError] = useState('');

  // Check existing session on mount, or auto-login for IAM admins
  useEffect(() => {
    setCheckingSession(true);
    setAccessDenied(false);

    (async () => {
      const hasSession = await checkSession();
      if (hasSession) {
        setAuthed(true);
        setCheckingSession(false);
        return;
      }

      const info = await whoami();
      setExternalProviders(info.external_providers || []);

      // In IAM mode, attempt auto-login with the current S3 credentials.
      // loginAs will succeed if the user is an IAM admin, or return 403 otherwise.
      if (info.mode === 'iam') {
        const creds = getCredentials();
        const ak = creds.accessKeyId;
        const sk = creds.secretAccessKey;
        if (ak && sk) {
          const result = await loginAs(ak, sk);
          if (result.ok) {
            setAuthed(true);
          } else {
            setAccessDenied(true);
          }
        }
      }

      setCheckingSession(false);
    })();
  }, []);

  const handleLogin = async () => {
    setLoginLoading(true);
    setLoginError('');
    try {
      const res = await adminLogin(password);
      if (res.ok) {
        setAuthed(true);
        setPassword('');
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

  // Periodic session check every 5 minutes while page is active
  useEffect(() => {
    if (!authed) return;
    const id = setInterval(async () => {
      const valid = await checkSession();
      if (!valid) {
        onSessionExpired?.();
      }
    }, 5 * 60 * 1000);
    return () => clearInterval(id);
  }, [authed, onSessionExpired]);

  const navigateToGroup = useCallback((groupId: number) => {
    setPendingGroupId(groupId);
    setActiveTab('groups');
  }, [setActiveTab]);

  const renderContent = () => {
    const tab = TABS.find(t => t.key === activeTab);
    const header = tab ? <TabHeader icon={tab.icon} title={tab.title} description={tab.description} /> : null;

    if (activeTab === 'users') {
      return <>{header}<UsersPanel onSessionExpired={onSessionExpired} onNavigateToGroup={navigateToGroup} /></>;
    }
    if (activeTab === 'groups') {
      return <>{header}<GroupsPanel onSessionExpired={onSessionExpired} initialGroupId={pendingGroupId} onGroupSelected={() => setPendingGroupId(null)} /></>;
    }
    if (activeTab === 'auth') {
      return <>{header}<AuthenticationPanel onSessionExpired={onSessionExpired} /></>;
    }
    if (activeTab === 'backends') {
      return <>{header}<BackendsPanel onSessionExpired={onSessionExpired} /></>;
    }
    if (activeTab === 'metrics') {
      return <>{header}<MetricsPage onBack={onBack} embedded /></>;
    }
    return (
      <>
        {header}
        <SettingsPage
          onSessionExpired={onSessionExpired}
          embeddedTab={activeTab}
        />
      </>
    );
  };

  // Access denied (IAM user without admin permissions)
  if (!authed && !checkingSession && accessDenied) {
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

  // Login gate (bootstrap password + optional OAuth buttons)
  if (!authed && !checkingSession) {
    return (
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', flex: 1, background: colors.BG_BASE }}>
        <form onSubmit={e => { e.preventDefault(); handleLogin(); }} style={{ width: 380, padding: 40 }}>
          <div style={{ textAlign: 'center', marginBottom: 24 }}>
            <LockOutlined style={{ fontSize: 32, color: colors.ACCENT_BLUE, marginBottom: 12 }} />
            <div><Text strong style={{ fontSize: 18, fontFamily: 'var(--font-ui)' }}>Admin Login</Text></div>
            <Text type="secondary" style={{ fontSize: 13 }}>
              {externalProviders.length > 0 ? 'Sign in to continue.' : 'Enter the bootstrap password to continue.'}
            </Text>
          </div>
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
            placeholder="Bootstrap password"
            value={password}
            onChange={e => setPassword(e.target.value)}
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

  if (checkingSession) {
    return (
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', flex: 1, background: colors.BG_BASE }}>
        <Spin size="large" />
      </div>
    );
  }

  return (
    <div style={{
      display: 'flex',
      flexDirection: 'column',
      flex: 1,
      background: colors.BG_BASE,
    }}>
      <FullScreenHeader title="Admin Settings" onBack={onBack} />

      {/* Body: sidebar tabs + content */}
      <div style={{ flex: 1, display: 'flex', overflow: 'hidden' }}>
        {/* Vertical tab sidebar */}
        <nav style={{
          width: 200,
          borderRight: `1px solid ${colors.BORDER}`,
          background: colors.BG_CARD,
          padding: '12px 0',
          flexShrink: 0,
        }}>
          {TABS.map(tab => {
            const isActive = tab.key === activeTab;
            return (
              <button
                key={tab.key}
                onClick={() => setActiveTab(tab.key)}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: 10,
                  width: '100%',
                  padding: '10px 20px',
                  border: 'none',
                  background: isActive ? colors.ACCENT_BLUE + '18' : 'transparent',
                  borderLeft: isActive ? `3px solid ${colors.ACCENT_BLUE}` : '3px solid transparent',
                  color: isActive ? colors.ACCENT_BLUE : colors.TEXT_SECONDARY,
                  fontSize: 14,
                  fontWeight: isActive ? 600 : 400,
                  cursor: 'pointer',
                  fontFamily: 'var(--font-ui)',
                  textAlign: 'left',
                  transition: 'all 0.15s ease',
                }}
              >
                {tab.icon}
                {tab.label}
              </button>
            );
          })}
          {/* Backup/Restore */}
          <div style={{ borderTop: `1px solid ${colors.BORDER}`, margin: '8px 12px 0', padding: '10px 0 0' }}>
            <div style={{ fontSize: 10, fontWeight: 600, letterSpacing: 0.5, textTransform: 'uppercase', color: colors.TEXT_MUTED, padding: '0 8px 6px', fontFamily: 'var(--font-ui)' }}>
              Backup
            </div>
            <div style={{ display: 'flex', gap: 4, padding: '0 8px' }}>
              <Button
                size="small"
                icon={<DownloadOutlined />}
                onClick={async () => {
                  try {
                    const data = await exportBackup();
                    const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
                    const url = URL.createObjectURL(blob);
                    const a = document.createElement('a');
                    a.href = url;
                    a.download = `dgp-iam-backup-${new Date().toISOString().slice(0, 10)}.json`;
                    a.click();
                    URL.revokeObjectURL(url);
                    message.success('IAM backup exported');
                  } catch (e) {
                    message.error('Export failed: ' + (e instanceof Error ? e.message : 'unknown'));
                  }
                }}
                style={{ flex: 1, fontSize: 11 }}
              >
                Export
              </Button>
              <Button
                size="small"
                icon={<UploadOutlined />}
                onClick={() => {
                  const input = document.createElement('input');
                  input.type = 'file';
                  input.accept = '.json';
                  input.onchange = async () => {
                    const file = input.files?.[0];
                    if (!file) return;
                    try {
                      const text = await file.text();
                      const data = JSON.parse(text);
                      const result = await importBackup(data);
                      message.success(`Imported: ${result.users_created} users, ${result.groups_created} groups (${result.users_skipped} skipped)`);
                      // Reload current tab
                      window.location.reload();
                    } catch (e) {
                      message.error('Import failed: ' + (e instanceof Error ? e.message : 'invalid JSON'));
                    }
                  };
                  input.click();
                }}
                style={{ flex: 1, fontSize: 11 }}
              >
                Import
              </Button>
            </div>
          </div>
        </nav>

        {/* Tab content */}
        <div style={{ flex: 1, overflow: 'auto' }}>
          {renderContent()}
        </div>
      </div>
    </div>
  );
}
