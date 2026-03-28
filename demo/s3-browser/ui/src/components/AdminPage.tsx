import { useState, useEffect, useCallback } from 'react';
import { Typography, Button, Input, Alert, Space, Spin, message } from 'antd';
import { checkSession, adminLogin, whoami, loginAs, exportBackup, importBackup } from '../adminApi';
import {
  CloudOutlined,
  DatabaseOutlined,
  ControlOutlined,
  TeamOutlined,
  FolderOutlined,
  LockOutlined,
  ArrowLeftOutlined,
  DashboardOutlined,
  DownloadOutlined,
  UploadOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import SettingsPage from './SettingsPage';
import UsersPanel from './UsersPanel';
import GroupsPanel from './GroupsPanel';
import MetricsPage from './MetricsPage';

const { Text } = Typography;

const TAB_KEY_TO_HASH: Record<string, string> = {
  users: '#/admin/users',
  groups: '#/admin/groups',
  metrics: '#/admin/metrics',
  connection: '#/admin/connection',
  backend: '#/admin/backend',
  proxy: '#/admin/proxy',
  security: '#/admin/bootstrap',
};

const HASH_TO_TAB: Record<string, string> = {
  '#/admin': 'users',
  '#/admin/users': 'users',
  '#/admin/groups': 'groups',
  '#/admin/metrics': 'metrics',
  '#/admin/connection': 'connection',
  '#/admin/backend': 'backend',
  '#/admin/proxy': 'proxy',
  '#/admin/bootstrap': 'security',
};

function readTabFromHash(): string {
  return HASH_TO_TAB[window.location.hash] ?? 'users';
}

const TABS = [
  { key: 'users', label: 'Users', icon: <TeamOutlined /> },
  { key: 'groups', label: 'Groups', icon: <FolderOutlined /> },
  { key: 'metrics', label: 'Metrics', icon: <DashboardOutlined /> },
  { key: 'connection', label: 'Connection', icon: <CloudOutlined /> },
  { key: 'backend', label: 'Backend', icon: <DatabaseOutlined /> },
  { key: 'proxy', label: 'Proxy', icon: <ControlOutlined /> },
  { key: 'security', label: 'Bootstrap', icon: <LockOutlined /> },
];

interface AdminPageProps {
  onBack: () => void;
  onSessionExpired?: () => void;
}

export default function AdminPage({ onBack, onSessionExpired }: AdminPageProps) {
  const colors = useColors();
  const [activeTab, setActiveTabState] = useState(readTabFromHash);

  const setActiveTab = useCallback((tab: string) => {
    setActiveTabState(tab);
    const hash = TAB_KEY_TO_HASH[tab] || '#/admin/users';
    if (window.location.hash !== hash) {
      window.history.pushState(null, '', hash);
    }
  }, []);

  // Sync tab from hash on browser back/forward
  useEffect(() => {
    const onHashChange = () => {
      const tab = readTabFromHash();
      setActiveTabState(tab);
    };
    window.addEventListener('hashchange', onHashChange);
    return () => window.removeEventListener('hashchange', onHashChange);
  }, []);

  const [authed, setAuthed] = useState(false);
  const [checkingSession, setCheckingSession] = useState(true);
  const [, setAuthMode] = useState<'bootstrap' | 'iam' | 'open'>('bootstrap');
  const [accessDenied, setAccessDenied] = useState(false);
  const [password, setPassword] = useState('');
  const [loginLoading, setLoginLoading] = useState(false);
  const [pendingGroupId, setPendingGroupId] = useState<number | null>(null);
  const [loginError, setLoginError] = useState('');
  const savingRef = { current: false };
  const setSaving = useCallback((v: boolean) => { savingRef.current = v; }, []);

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

      const ak = localStorage.getItem('dg-access-key-id') || undefined;
      const sk = localStorage.getItem('dg-secret-access-key') || undefined;
      const info = await whoami(ak, sk);
      setAuthMode(info.mode);

      if (info.mode === 'iam' && info.user?.is_admin && sk) {
        const result = await loginAs(info.user.access_key_id, sk);
        if (result.ok) {
          setAuthed(true);
        } else {
          setAccessDenied(true);
        }
      } else if (info.mode === 'iam' && info.user && !info.user.is_admin) {
        setAccessDenied(true);
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
    if (activeTab === 'users') {
      return <UsersPanel onSessionExpired={onSessionExpired} onSavingChange={setSaving} onNavigateToGroup={navigateToGroup} />;
    }
    if (activeTab === 'groups') {
      return <GroupsPanel onSessionExpired={onSessionExpired} onSavingChange={setSaving} initialGroupId={pendingGroupId} onGroupSelected={() => setPendingGroupId(null)} />;
    }
    if (activeTab === 'metrics') {
      return <MetricsPage onBack={onBack} embedded />;
    }
    return (
      <SettingsPage
        onBack={onBack}
        onSessionExpired={onSessionExpired}
        embeddedTab={activeTab}
      />
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

  // Bootstrap login gate (only in bootstrap/open mode)
  if (!authed && !checkingSession) {
    return (
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', flex: 1, background: colors.BG_BASE }}>
        <form onSubmit={e => { e.preventDefault(); handleLogin(); }} style={{ width: 380, padding: 40 }}>
          <div style={{ textAlign: 'center', marginBottom: 24 }}>
            <LockOutlined style={{ fontSize: 32, color: colors.ACCENT_BLUE, marginBottom: 12 }} />
            <div><Text strong style={{ fontSize: 18, fontFamily: 'var(--font-ui)' }}>Bootstrap Login</Text></div>
            <Text type="secondary" style={{ fontSize: 13 }}>Enter the bootstrap password to continue.</Text>
          </div>
          {loginError && <Alert type="error" message={loginError} showIcon style={{ marginBottom: 16, borderRadius: 8 }} />}
          <Input.Password
            placeholder="Password"
            value={password}
            onChange={e => setPassword(e.target.value)}
            size="large"
            autoFocus
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
      {/* Header */}
      <div style={{
        height: 52,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: '0 20px',
        borderBottom: `1px solid ${colors.BORDER}`,
        background: colors.BG_CARD,
        flexShrink: 0,
      }}>
        <Button
          type="text"
          icon={<ArrowLeftOutlined />}
          onClick={onBack}
          style={{ color: colors.TEXT_SECONDARY, fontWeight: 500 }}
        >
          Back to Browser
        </Button>
        <Text strong style={{ fontSize: 15, fontFamily: 'var(--font-ui)', letterSpacing: 1, textTransform: 'uppercase', color: colors.TEXT_MUTED }}>
          Admin Settings
        </Text>
        {/* Spacer to balance the header layout */}
        <div style={{ width: 120 }} />
      </div>

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
