import { useState, useEffect, useRef, useCallback } from 'react';
import { Layout, Spin, Empty, Grid, Button } from 'antd';
import { DeleteOutlined } from '@ant-design/icons';
import useS3Browser from './useS3Browser';
import TopBar from './components/TopBar';
import Sidebar from './components/Sidebar';
import ObjectTable from './components/ObjectTable';
import InspectorPanel from './components/InspectorPanel';
import FilePreview from './components/FilePreview';
import AdminPage from './components/AdminPage';
import DropZone from './components/DropZone';
import UploadPage from './components/UploadPage';
import ConnectPage from './components/ConnectPage';
import MetricsPage from './components/MetricsPage';
import ApiDocsPage from './components/ApiDocsPage';
import { getBucket, hasCredentials, setCredentials } from './s3client';
import { adminLogout, whoami } from './adminApi';
import type { WhoamiResponse } from './adminApi';
import { useColors } from './ThemeContext';
import useComputeSize from './useComputeSize';

const { Content } = Layout;
const { useBreakpoint } = Grid;

type View = 'browser' | 'upload' | 'metrics' | 'docs' | 'admin';

const HASH_TO_VIEW: Record<string, View> = {
  '': 'browser',
  '#/': 'browser',
  '#/browse': 'browser',
  '#/upload': 'upload',
  '#/settings': 'browser', // legacy redirect
  '#/admin': 'admin',
  '#/admin/users': 'admin',
  '#/admin/groups': 'admin',
  '#/admin/connection': 'admin',
  '#/admin/backend': 'admin',
  '#/admin/proxy': 'admin',
  '#/admin/metrics': 'admin',
  '#/admin/bootstrap': 'admin',
  '#/metrics': 'metrics',
  '#/docs': 'docs',
};

const VIEW_TO_HASH: Record<View, string> = {
  browser: '#/browse',
  upload: '#/upload',
  metrics: '#/metrics',
  docs: '#/docs',
  admin: '#/admin',
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


export default function App() {
  const colors = useColors();

  const [view, setView] = useHashRouter();
  const [siderOpen, setSiderOpen] = useState(false);
  const [needsConnect, setNeedsConnect] = useState(!hasCredentials());
  const [firstLoadDone, setFirstLoadDone] = useState(false);
  const [previewObject, setPreviewObject] = useState<import('./types').S3Object | null>(null);
  const [identity, setIdentity] = useState<WhoamiResponse | null>(null);

  // Check identity after S3 connection is established
  useEffect(() => {
    if (!needsConnect) {
      const ak = localStorage.getItem('dg-access-key-id') || undefined;
      const sk = localStorage.getItem('dg-secret-access-key') || undefined;
      whoami(ak, sk).then(setIdentity);
    } else {
      setIdentity(null);
    }
  }, [needsConnect]);

  const screens = useBreakpoint();
  const isMobile = !screens.md;
  const mainRef = useRef<HTMLElement>(null);

  const s3 = useS3Browser();
  const folderSize = useComputeSize();

  // Clear folder size computations and preview when prefix or bucket changes
  useEffect(() => {
    folderSize.cancelAll();
    setPreviewObject(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [s3.prefix]);

  // Dynamic page title on view change
  useEffect(() => {
    const titles: Record<View, string> = {
      browser: `${getBucket()} — DeltaGlider Proxy`,
      upload: 'Upload — DeltaGlider Proxy',
      metrics: 'Metrics — DeltaGlider Proxy',
      docs: 'API Reference — DeltaGlider Proxy',
      admin: 'Admin Settings — DeltaGlider Proxy',
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
    if (view === 'admin') {
      return (
        <AdminPage
          onBack={() => setView('browser')}
          onSessionExpired={() => setView('browser')}
        />
      );
    }

    if (view === 'metrics') {
      return <MetricsPage onBack={() => setView('browser')} />;
    }

    if (view === 'docs') {
      return <ApiDocsPage onBack={() => setView('browser')} />;
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
              folderSizes={folderSize.sizes}
              onComputeSize={folderSize.compute}
              onCancelSize={folderSize.cancel}
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
        {view !== 'admin' && (
          <Sidebar
            onUploadClick={() => { setView('upload'); setSiderOpen(false); }}
            onMutate={s3.mutate}
            refreshTrigger={s3.refreshTrigger}
            onBucketChange={handleBucketChange}
            open={siderOpen}
            onClose={() => setSiderOpen(false)}
            isMobile={isMobile}
            onSettingsClick={() => {
              setView('admin');
              setSiderOpen(false);
            }}
            onDocsClick={() => {
              setView('docs');
              setSiderOpen(false);
            }}
            onLogout={handleLogout}
            currentUser={localStorage.getItem('dg-access-key-id') || undefined}
            displayName={identity?.user?.name || undefined}
            canAdmin={identity?.mode === 'bootstrap' || identity?.mode === 'open' || identity?.user?.is_admin === true}
          />
        )}

        <Layout style={{ flex: 1, background: colors.BG_BASE }}>
          {view !== 'admin' && (
            <TopBar
              prefix={s3.prefix}
              onNavigate={(p) => { setView('browser'); s3.navigate(p); }}
              isMobile={isMobile}
              onMenuClick={() => setSiderOpen(true)}
              onRefresh={s3.mutate}
              searchQuery={s3.searchQuery}
              onSearchChange={s3.setSearchQuery}
              refreshing={s3.refreshing}
              showHidden={s3.showHidden}
              onToggleHidden={() => s3.setShowHidden(!s3.showHidden)}
            />
          )}

          <main id="main-content" ref={mainRef} tabIndex={-1} style={{ outline: 'none', flex: 1, overflow: 'auto', display: 'flex', flexDirection: 'column' }}>
            <Content style={{ display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0 }}>
              {renderContent()}
            </Content>
          </main>
        </Layout>
      </Layout>

      <InspectorPanel
        object={s3.selected}
        onClose={() => s3.setSelected(null)}
        onDeleted={s3.mutate}
        onPreview={setPreviewObject}
        isMobile={isMobile}
        headCache={s3.headCache}
      />

      <FilePreview
        open={previewObject !== null}
        object={previewObject}
        onClose={() => setPreviewObject(null)}
      />
      {view === 'browser' && <DropZone onDrop={s3.uploadFiles} prefix={s3.prefix} />}
    </Layout>
  );
}
