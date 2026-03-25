import { useState, useEffect } from 'react';
import { Typography, Button, Input, Alert, Space, Spin } from 'antd';
import { checkSession, adminLogin } from '../adminApi';
import {
  CloseOutlined,
  CloudOutlined,
  DatabaseOutlined,
  ControlOutlined,
  TeamOutlined,
  LockOutlined,
  ArrowLeftOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import SettingsPage from './SettingsPage';
import UsersPanel from './UsersPanel';

const { Text } = Typography;

const TABS = [
  { key: 'connection', label: 'Connection', icon: <CloudOutlined /> },
  { key: 'backend', label: 'Backend', icon: <DatabaseOutlined /> },
  { key: 'proxy', label: 'Proxy', icon: <ControlOutlined /> },
  { key: 'users', label: 'Users', icon: <TeamOutlined /> },
  { key: 'security', label: 'Security', icon: <LockOutlined /> },
];

interface AdminOverlayProps {
  open: boolean;
  onClose: () => void;
  onSessionExpired?: () => void;
}

export default function AdminOverlay({ open, onClose, onSessionExpired }: AdminOverlayProps) {
  const colors = useColors();
  const [activeTab, setActiveTab] = useState('users');
  const [authed, setAuthed] = useState(false);
  const [checkingSession, setCheckingSession] = useState(true);
  const [password, setPassword] = useState('');
  const [loginLoading, setLoginLoading] = useState(false);
  const [loginError, setLoginError] = useState('');

  // Check existing session on open
  useEffect(() => {
    if (!open) return;
    setCheckingSession(true);
    checkSession().then(valid => {
      setAuthed(valid);
      setCheckingSession(false);
    });
  }, [open]);

  // Close on Escape
  useEffect(() => {
    if (!open) return;
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', handleKey);
    return () => window.removeEventListener('keydown', handleKey);
  }, [open, onClose]);

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

  if (!open) return null;

  const renderContent = () => {
    if (activeTab === 'users') {
      return <UsersPanel onSessionExpired={onSessionExpired} />;
    }
    // For non-Users tabs, render the existing SettingsPage content
    // SettingsPage manages its own state — we pass the active tab to it
    return (
      <SettingsPage
        onBack={onClose}
        onSessionExpired={onSessionExpired}
        embeddedTab={activeTab}
      />
    );
  };

  // Login gate
  if (!authed && !checkingSession) {
    return (
      <div style={{ position: 'fixed', inset: 0, zIndex: 1000, display: 'flex', alignItems: 'center', justifyContent: 'center', background: colors.BG_BASE }}>
        <form onSubmit={e => { e.preventDefault(); handleLogin(); }} style={{ width: 380, padding: 40 }}>
          <div style={{ textAlign: 'center', marginBottom: 24 }}>
            <LockOutlined style={{ fontSize: 32, color: colors.ACCENT_BLUE, marginBottom: 12 }} />
            <div><Text strong style={{ fontSize: 18, fontFamily: 'var(--font-ui)' }}>Admin Login</Text></div>
            <Text type="secondary" style={{ fontSize: 13 }}>Enter the admin password to continue.</Text>
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
            <Button type="text" block onClick={onClose} style={{ color: colors.TEXT_MUTED }}>Cancel</Button>
          </Space>
        </form>
      </div>
    );
  }

  if (checkingSession) {
    return (
      <div style={{ position: 'fixed', inset: 0, zIndex: 1000, display: 'flex', alignItems: 'center', justifyContent: 'center', background: colors.BG_BASE }}>
        <Spin size="large" />
      </div>
    );
  }

  return (
    <div style={{
      position: 'fixed',
      inset: 0,
      zIndex: 1000,
      display: 'flex',
      flexDirection: 'column',
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
          onClick={onClose}
          style={{ color: colors.TEXT_SECONDARY, fontWeight: 500 }}
        >
          Back to Browser
        </Button>
        <Text strong style={{ fontSize: 15, fontFamily: 'var(--font-ui)', letterSpacing: 1, textTransform: 'uppercase', color: colors.TEXT_MUTED }}>
          Admin Settings
        </Text>
        <Button type="text" icon={<CloseOutlined />} onClick={onClose} style={{ color: colors.TEXT_MUTED }} />
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
        </nav>

        {/* Tab content */}
        <div style={{ flex: 1, overflow: 'auto' }}>
          {renderContent()}
        </div>
      </div>
    </div>
  );
}
