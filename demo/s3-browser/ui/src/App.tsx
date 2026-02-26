import { useState, useEffect, useRef, useCallback } from 'react';
import { Layout, Spin, Empty, Grid, Button, Input, Alert, Space, Typography } from 'antd';
import { DeleteOutlined, LockOutlined, ArrowLeftOutlined } from '@ant-design/icons';
import useS3Browser from './useS3Browser';
import TopBar from './components/TopBar';
import Sidebar from './components/Sidebar';
import ObjectTable from './components/ObjectTable';
import InspectorPanel from './components/InspectorPanel';
import DropZone from './components/DropZone';
import UploadPage from './components/UploadPage';
import ConnectPage from './components/ConnectPage';
import SettingsPage from './components/SettingsPage';
import { getBucket, hasCredentials, setCredentials } from './s3client';
import { checkSession, adminLogin, adminLogout } from './adminApi';
import { useColors } from './ThemeContext';

const { Content } = Layout;
const { useBreakpoint } = Grid;

type View = 'browser' | 'upload' | 'settings';

const HASH_TO_VIEW: Record<string, View> = {
  '': 'browser',
  '#/': 'browser',
  '#/browse': 'browser',
  '#/upload': 'upload',
  '#/settings': 'settings',
};

const VIEW_TO_HASH: Record<View, string> = {
  browser: '#/browse',
  upload: '#/upload',
  settings: '#/settings',
};

function readViewFromHash(): View {
  return HASH_TO_VIEW[window.location.hash] ?? 'browser';
}

function useHashRouter() {
  const [view, setViewState] = useState<View>(readViewFromHash);
  const skipNextHashChange = useRef(false);

  const setView = useCallback((v: View, replace = false) => {
    setViewState(v);
    const hash = VIEW_TO_HASH[v];
    if (window.location.hash !== hash) {
      skipNextHashChange.current = true;
      if (replace) {
        window.history.replaceState(null, '', hash);
      } else {
        window.history.pushState(null, '', hash);
      }
    }
  }, []);

  useEffect(() => {
    const onHashChange = () => {
      if (skipNextHashChange.current) {
        skipNextHashChange.current = false;
        return;
      }
      setViewState(readViewFromHash());
    };
    window.addEventListener('hashchange', onHashChange);
    return () => window.removeEventListener('hashchange', onHashChange);
  }, []);

  return [view, setView] as const;
}

const { Text } = Typography;

function AdminGate({ onSuccess, onBack }: { onSuccess: () => void; onBack: () => void }) {
  const { BORDER, TEXT_MUTED, ACCENT_BLUE } = useColors();
  const [password, setPassword] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  const handleSubmit = async () => {
    setLoading(true);
    setError('');
    try {
      const res = await adminLogin(password);
      if (res.ok) {
        onSuccess();
      } else {
        setError(res.error || 'Login failed');
        setPassword('');
      }
    } catch {
      setError('Network error');
      setPassword('');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: '70vh', padding: 24 }}>
      <form onSubmit={(e) => { e.preventDefault(); handleSubmit(); }} className="glass-card animate-fade-in" style={{ borderRadius: 14, padding: 'clamp(28px, 4vw, 40px)', width: '100%', maxWidth: 400 }}>
        <Space orientation="vertical" size="large" style={{ width: '100%' }}>
          <div style={{ textAlign: 'center' }}>
            <div style={{
              width: 56, height: 56, borderRadius: 14,
              background: `linear-gradient(135deg, ${ACCENT_BLUE}22, ${ACCENT_BLUE}08)`,
              border: `1px solid ${ACCENT_BLUE}33`,
              display: 'inline-flex', alignItems: 'center', justifyContent: 'center', marginBottom: 16,
            }}>
              <LockOutlined style={{ fontSize: 24, color: ACCENT_BLUE }} />
            </div>
            <Text strong style={{ display: 'block', fontSize: 18, fontFamily: "var(--font-ui)" }}>Admin Login</Text>
            <Text style={{ color: TEXT_MUTED, fontSize: 13 }}>Enter the admin password to access settings.</Text>
          </div>

          {error && <Alert type="error" message={error} showIcon />}

          <Input.Password
            placeholder="Password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            size="large"
            autoFocus
            autoComplete="current-password"
            style={{ background: 'var(--input-bg)', borderColor: BORDER, borderRadius: 10, height: 48, fontFamily: "var(--font-mono)" }}
          />

          <Space orientation="vertical" size="small" style={{ width: '100%' }}>
            <Button type="primary" htmlType="submit" block size="large" loading={loading} disabled={!password}
              style={{ height: 48, borderRadius: 10, fontWeight: 700, fontFamily: "var(--font-ui)", fontSize: 15 }}>
              Sign In
            </Button>
            <Button type="text" block icon={<ArrowLeftOutlined />} onClick={onBack}
              style={{ color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
              Back to browser
            </Button>
          </Space>
        </Space>
      </form>
    </div>
  );
}

export default function App() {
  const colors = useColors();

  const [view, setView] = useHashRouter();
  const [siderOpen, setSiderOpen] = useState(false);
  const [needsConnect, setNeedsConnect] = useState(!hasCredentials());
  const [firstLoadDone, setFirstLoadDone] = useState(false);
  const [isAdmin, setIsAdmin] = useState(false);

  // Check admin session on mount
  useEffect(() => {
    checkSession().then(setIsAdmin);
  }, []);

  const screens = useBreakpoint();
  const isMobile = !screens.md;
  const mainRef = useRef<HTMLElement>(null);

  const s3 = useS3Browser();

  // Dynamic page title on view change
  useEffect(() => {
    const titles: Record<View, string> = {
      browser: `${getBucket()} — DeltaGlider Proxy`,
      upload: 'Upload — DeltaGlider Proxy',
      settings: 'Settings — DeltaGlider Proxy',
    };
    document.title = titles[view];
  }, [view]);

  // Focus management: move focus to main content area on view change
  useEffect(() => {
    mainRef.current?.focus();
  }, [view]);

  // One-time stale-credential check after first load
  useEffect(() => {
    if (!s3.loading && !firstLoadDone) {
      setFirstLoadDone(true);
      if (!s3.connected) {
        setNeedsConnect(true);
      }
    }
  }, [s3.loading, s3.connected, firstLoadDone]);

  const handleLogout = () => {
    // Clear S3 credentials
    setCredentials('', '');
    // Clear admin session
    adminLogout().catch(() => {});
    setIsAdmin(false);
    setFirstLoadDone(false);
    setNeedsConnect(true);
    setView('browser');
  };

  const handleBucketChange = (newBucket: string) => {
    s3.changeBucket(newBucket);
    setView('browser');
  };

  const isEmpty = s3.objects.length === 0 && s3.folders.length === 0;

  if (needsConnect) {
    return (
      <ConnectPage
        onConnect={() => { setNeedsConnect(false); s3.reconnect(); }}
        showError={hasCredentials()}
      />
    );
  }

  const renderContent = () => {
    if (view === 'settings') {
      if (!isAdmin) {
        return (
          <AdminGate
            onSuccess={() => setIsAdmin(true)}
            onBack={() => setView('browser')}
          />
        );
      }
      return <SettingsPage onBack={() => setView('browser')} onSessionExpired={() => setIsAdmin(false)} />;
    }

    if (view === 'upload') {
      return (
        <UploadPage
          prefix={s3.prefix}
          onBack={() => setView('browser')}
          onDone={() => s3.mutate()}
        />
      );
    }

    return (
      <>
        {s3.selectedKeys.size > 0 && (
          <div style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'flex-end',
            padding: '8px 20px',
            borderBottom: `1px solid ${colors.BORDER}`,
          }}>
            <Button
              danger
              icon={<DeleteOutlined />}
              onClick={s3.bulkDelete}
              loading={s3.deleting}
              size="small"
            >
              {s3.deleting ? 'Deleting...' : `Delete ${s3.selectedKeys.size} selected`}
            </Button>
          </div>
        )}

        <div style={{ flex: 1, overflow: 'auto' }}>
          {s3.loading && isEmpty ? (
            <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', padding: '64px 0' }}>
              <Spin description="Loading objects..." />
            </div>
          ) : isEmpty ? (
            <Empty
              description={
                s3.searchQuery
                  ? `No results for "${s3.searchQuery}"`
                  : s3.prefix
                    ? 'This folder is empty.'
                    : 'No objects yet. Upload files or generate demo data.'
              }
              style={{ padding: '64px 0' }}
            />
          ) : (
            <ObjectTable
              objects={s3.objects}
              folders={s3.folders}
              prefix={s3.prefix}
              selected={s3.selected}
              onSelect={s3.setSelected}
              onNavigate={(p) => { setView('browser'); s3.navigate(p); }}
              selectedKeys={s3.selectedKeys}
              onToggleKey={s3.toggleKey}
              onToggleAll={s3.toggleAll}
              isMobile={isMobile}
              isTruncated={s3.isTruncated}
              refreshing={s3.refreshing}
              headCache={s3.headCache}
              onEnrichKeys={s3.enrichKeys}
            />
          )}
        </div>
      </>
    );
  };

  return (
    <Layout style={{ minHeight: '100vh', background: colors.BG_BASE }}>
      {/* Skip to content link */}
      <a href="#main-content" className="sr-only sr-only-focusable">
        Skip to main content
      </a>

      <Layout style={{ flexDirection: 'row', flex: 1 }}>
        <Sidebar
          onUploadClick={() => { setView('upload'); setSiderOpen(false); }}
          onMutate={s3.mutate}
          refreshTrigger={s3.refreshTrigger}
          onBucketChange={handleBucketChange}
          open={siderOpen}
          onClose={() => setSiderOpen(false)}
          isMobile={isMobile}
          onSettingsClick={() => {
            setView('settings');
            setSiderOpen(false);
          }}
          onLogout={handleLogout}
        />

        <Layout style={{ flex: 1, background: colors.BG_BASE }}>
          <TopBar
            prefix={s3.prefix}
            onNavigate={(p) => { setView('browser'); s3.navigate(p); }}
            isMobile={isMobile}
            onMenuClick={() => setSiderOpen(true)}
            onRefresh={s3.mutate}
            searchQuery={s3.searchQuery}
            onSearchChange={s3.setSearchQuery}
            refreshing={s3.refreshing}
          />

          <main id="main-content" ref={mainRef} tabIndex={-1} style={{ outline: 'none', flex: 1, overflow: 'auto' }}>
            <Content style={{ display: 'flex', flexDirection: 'column', minHeight: 0 }}>
              {renderContent()}
            </Content>
          </main>
        </Layout>
      </Layout>

      <InspectorPanel
        object={s3.selected}
        onClose={() => s3.setSelected(null)}
        onDeleted={s3.mutate}
        isMobile={isMobile}
        headCache={s3.headCache}
      />

      {view === 'browser' && <DropZone onDrop={s3.uploadFiles} prefix={s3.prefix} />}
    </Layout>
  );
}
