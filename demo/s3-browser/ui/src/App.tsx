import { useState, useEffect, useRef, useCallback } from 'react';
import { Layout, Spin, Empty, Grid } from 'antd';
import useS3Browser from './useS3Browser';
import TopBar from './components/TopBar';
import BulkActionBar from './components/BulkActionBar';
import Sidebar from './components/Sidebar';
import ObjectTable from './components/ObjectTable';
import InspectorPanel from './components/InspectorPanel';
import FilePreview from './components/FilePreview';
import AdminPage from './components/AdminPage';
import DropZone from './components/DropZone';
import UploadPage from './components/UploadPage';
import ConnectPage from './components/ConnectPage';
import MetricsPage from './components/MetricsPage';
import DocsPage from './components/DocsPage';
import { getBucket, hasCredentials, disconnect, initFromSession, getCredentials } from './s3client';
import { adminLogout, whoami, checkSession } from './adminApi';
import type { WhoamiResponse } from './adminApi';
import { useColors } from './ThemeContext';
import useComputeSize from './useComputeSize';
import { NavigationContext } from './NavigationContext';

const { Content } = Layout;
const { useBreakpoint } = Grid;

type View = 'browser' | 'upload' | 'metrics' | 'docs' | 'admin';

/** Full-screen views hide the main sidebar and TopBar */
const FULLSCREEN_VIEWS: Set<View> = new Set(['admin', 'docs']);

const BASE = '/_/';

const SEGMENT_TO_VIEW: Record<string, View> = {
  '': 'browser',
  'browse': 'browser',
  'upload': 'upload',
  'metrics': 'metrics',
  'docs': 'docs',
  'admin': 'admin',
};

/** Parse pathname into view + sub-path */
function parsePath(): { view: View; subPath: string } {
  let path = window.location.pathname;
  if (path.startsWith(BASE)) path = path.slice(BASE.length);
  else if (path.startsWith('/')) path = path.slice(1);
  path = path.replace(/\/+$/, ''); // trim trailing slashes

  const segments = path.split('/');
  const view = SEGMENT_TO_VIEW[segments[0] || ''] ?? 'browser';
  const subPath = segments.slice(1).join('/');
  return { view, subPath };
}

function usePathRouter() {
  const [state, setState] = useState(parsePath);
  const skipNext = useRef(false);

  // Redirect old hash-based URLs on first load
  useEffect(() => {
    if (window.location.hash.startsWith('#/')) {
      const oldPath = window.location.hash.slice(1); // e.g., "/admin/users"
      window.history.replaceState(null, '', BASE + oldPath.replace(/^\//, ''));
      setState(parsePath());
    }
  }, []);

  const navigate = useCallback((path: string, replace = false) => {
    const cleanPath = path.replace(/^\//, '');
    const fullPath = BASE + cleanPath;
    if (window.location.pathname + window.location.hash === fullPath) return;
    skipNext.current = true;
    if (replace) {
      window.history.replaceState(null, '', fullPath);
    } else {
      window.history.pushState(null, '', fullPath);
    }
    setState(parsePath());
  }, []);

  useEffect(() => {
    const onPopState = () => {
      if (skipNext.current) { skipNext.current = false; return; }
      setState(parsePath());
    };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  return { view: state.view, subPath: state.subPath, navigate };
}


export default function App() {
  const colors = useColors();

  const { view, subPath, navigate } = usePathRouter();
  const [siderOpen, setSiderOpen] = useState(false);
  const [needsConnect, setNeedsConnect] = useState(true); // start true, resolved in useEffect
  const [sessionLoading, setSessionLoading] = useState(true);
  const [firstLoadDone, setFirstLoadDone] = useState(false);
  const [previewObject, setPreviewObject] = useState<import('./types').S3Object | null>(null);
  const [identity, setIdentity] = useState<WhoamiResponse | null>(null);

  const [hasAdminSession, setHasAdminSession] = useState(false);

    // Restore credentials from server-side session on mount.
  // Also check for admin session (OAuth sets a session cookie even if S3 creds
  // don't have permissions — the user IS authenticated, just may lack S3 access).
  useEffect(() => {
    Promise.all([initFromSession(), checkSession()]).then(([restored, hasSession]) => {
      setHasAdminSession(hasSession);
      setNeedsConnect(!(restored || hasSession));
    }).catch(() => {
      setNeedsConnect(true);
    }).finally(() => {
      setSessionLoading(false);
    });
  }, []);

  // When session is restored and we're connected, reload the S3 browser
  // and fetch identity. Runs AFTER React commits the needsConnect state.
  useEffect(() => {
    if (!needsConnect && !sessionLoading) {
      s3.reconnect();
      whoami().then(setIdentity);
    } else if (needsConnect) {
      setIdentity(null);
      setHasAdminSession(false);
    }
  }, [needsConnect, sessionLoading]);

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

  // One-time stale-credential check after first load.
  // If we have a valid admin session (e.g. from OAuth), don't bounce to ConnectPage
  // even if S3 calls fail — the user is authenticated but may lack S3 permissions.
  useEffect(() => {
    if (!s3.loading && !firstLoadDone) {
      setFirstLoadDone(true);
      if (!s3.connected && !hasAdminSession) {
        setNeedsConnect(true);
      }
    }
  }, [s3.loading, s3.connected, firstLoadDone, hasAdminSession]);

  const handleLogout = () => {
    disconnect();
    adminLogout().catch(() => {});
    setFirstLoadDone(false);
    setNeedsConnect(true);
    setIdentity(null);
    setHasAdminSession(false);
    navigate('browse');
  };

  const handleBucketChange = (newBucket: string) => {
    s3.changeBucket(newBucket);
    navigate('browse');
  };

  const isEmpty = s3.objects.length === 0 && s3.folders.length === 0;

  if (sessionLoading) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: '100vh' }}>
        <Spin size="large" tip="Restoring session..." />
      </div>
    );
  }

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
          onBack={() => navigate('browse')}
          onSessionExpired={() => navigate('browse')}
          subPath={subPath}
        />
      );
    }

    if (view === 'metrics') {
      return <MetricsPage onBack={() => navigate('browse')} />;
    }

    if (view === 'docs') {
      return <DocsPage onBack={() => navigate('browse')} docId={subPath || undefined} />;
    }

    if (view === 'upload') {
      return (
        <UploadPage
          prefix={s3.prefix}
          onBack={() => navigate('browse')}
          onDone={() => s3.mutate()}
        />
      );
    }

    return (
      <>
        {s3.selectedKeys.size > 0 && (
          <BulkActionBar
            selectedCount={s3.selectedKeys.size}
            onDelete={s3.bulkDelete}
            onCopy={s3.bulkCopy}
            onMove={s3.bulkMove}
            onDownloadZip={s3.downloadZip}
            deleting={s3.deleting}
          />
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
              onNavigate={(p) => { navigate('browse'); s3.navigate(p); }}
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
              onAutoPopulateSizes={folderSize.autoPopulate}
              onPreview={setPreviewObject}
            />
          )}
        </div>
      </>
    );
  };

  return (
    <NavigationContext.Provider value={{ navigate, subPath }}>
    <Layout style={{ minHeight: '100vh', background: colors.BG_BASE }}>
      {/* Skip to content link */}
      <a href="#main-content" className="sr-only sr-only-focusable">
        Skip to main content
      </a>

      <Layout style={{ flexDirection: 'row', flex: 1 }}>
        {!FULLSCREEN_VIEWS.has(view) && (
          <Sidebar
            onUploadClick={() => { navigate('upload'); setSiderOpen(false); }}
            onMutate={s3.mutate}
            refreshTrigger={s3.refreshTrigger}
            onBucketChange={handleBucketChange}
            open={siderOpen}
            onClose={() => setSiderOpen(false)}
            isMobile={isMobile}
            onSettingsClick={() => {
              navigate('admin');
              setSiderOpen(false);
            }}
            onDocsClick={() => {
              navigate('docs');
              setSiderOpen(false);
            }}
            onLogout={handleLogout}
            currentUser={getCredentials().accessKeyId || undefined}
            displayName={identity?.user?.name || undefined}
            canAdmin={identity?.mode === 'bootstrap' || identity?.mode === 'open' || identity?.user?.is_admin === true || hasAdminSession}
            proxyVersion={identity?.version}
          />
        )}

        <Layout style={{ flex: 1, background: colors.BG_BASE }}>
          {!FULLSCREEN_VIEWS.has(view) && (
            <TopBar
              prefix={s3.prefix}
              onNavigate={(p) => { navigate('browse'); s3.navigate(p); }}
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
    </NavigationContext.Provider>
  );
}
