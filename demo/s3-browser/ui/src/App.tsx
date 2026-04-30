import { useState, useEffect, useRef, useCallback, useMemo } from 'react';
import { Layout, Spin, Empty, Grid, Button, Space } from 'antd';
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
import AccountMenu from './components/AccountMenu';
import DemoDataGenerator from './components/DemoDataGenerator';
import { getBucket, hasCredentials, disconnect, initFromSession, getCredentials } from './s3client';
import { adminLogout, whoami, checkSession } from './adminApi';
import type { WhoamiResponse } from './adminApi';
import { useColors } from './ThemeContext';
import useComputeSize from './useComputeSize';
import { NavigationContext } from './NavigationContext';
import { canUse } from './permissions';

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

  // Stable "go back to browser" callback. Previously inlined into
  // AdminPage / MetricsPage props as `() => navigate('browse')`, which
  // allocated a fresh arrow every App render and propagated as
  // `onBack` / `onSessionExpired` to ~10 admin panels. Each panel's
  // `loadData` `useCallback` lists `onSessionExpired` in its deps,
  // so a fresh prop → fresh callback → `useEffect([loadData])` fires
  // → re-fetches → setState → re-render wave. This one-line fix
  // eliminates the most visible cause of excessive re-renders when
  // navigating between admin sub-pages.
  const navigateToBrowse = useCallback(() => navigate('browse'), [navigate]);

  useEffect(() => {
    const onPopState = () => {
      if (skipNext.current) { skipNext.current = false; return; }
      setState(parsePath());
    };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  return { view: state.view, subPath: state.subPath, navigate, navigateToBrowse };
}


export default function App() {
  const colors = useColors();

  const { view, subPath, navigate, navigateToBrowse } = usePathRouter();
  const [siderOpen, setSiderOpen] = useState(false);
  const [needsConnect, setNeedsConnect] = useState(true); // start true, resolved in useEffect
  const [sessionLoading, setSessionLoading] = useState(true);
  const [firstLoadDone, setFirstLoadDone] = useState(false);
  const [previewObject, setPreviewObject] = useState<import('./types').S3Object | null>(null);
  const [identity, setIdentity] = useState<WhoamiResponse | null>(null);
  const [bucketCount, setBucketCount] = useState<number | null>(null);
  const [createBucketFocusSignal, setCreateBucketFocusSignal] = useState(0);

  const [hasAdminSession, setHasAdminSession] = useState(false);
  const s3 = useS3Browser();
  const folderSize = useComputeSize();
  const reconnectS3 = s3.reconnect;
  const changeS3Bucket = s3.changeBucket;
  const cancelFolderSizes = folderSize.cancelAll;

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
      let cancelled = false;
      reconnectS3();
      whoami().then((nextIdentity) => {
        if (!cancelled) setIdentity(nextIdentity);
      });
      return () => {
        cancelled = true;
      };
    } else if (needsConnect) {
      setIdentity(null);
      setHasAdminSession(false);
    }
  }, [needsConnect, reconnectS3, sessionLoading]);

  const screens = useBreakpoint();
  const isMobile = !screens.md;
  const mainRef = useRef<HTMLElement>(null);

  // Clear folder size computations and preview when prefix or bucket changes
  useEffect(() => {
    cancelFolderSizes();
    setPreviewObject(null);
  }, [cancelFolderSizes, s3.prefix]);

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

  const handleBucketChange = useCallback((newBucket: string) => {
    changeS3Bucket(newBucket);
    navigate('browse');
  }, [changeS3Bucket, navigate]);

  const isEmpty = s3.objects.length === 0 && s3.folders.length === 0;
  const hasBuckets = (bucketCount ?? 0) > 0;
  const hasNoBuckets = bucketCount === 0;
  const isRootBucketEmpty = hasBuckets && s3.prefix === '' && !s3.searchQuery && isEmpty && !s3.loading;
  const activeBucket = getBucket();
  const currentAccessKey = getCredentials().accessKeyId || undefined;
  // A session can also belong to a non-admin external SSO user. Only
  // show Settings after whoami resolves admin/open/bootstrap authority.
  const canAdmin = identity?.mode === 'bootstrap' || identity?.mode === 'open' || identity?.user?.is_admin === true;
  const canCreateBucket = canUse(identity, 'admin');
  const canWriteActiveBucket = Boolean(activeBucket) && canUse(identity, 'write', activeBucket, s3.prefix);
  const selectedKeys = Array.from(s3.selectedKeys);
  const canReadSelected = Boolean(activeBucket) && selectedKeys.length > 0 && selectedKeys.every((selectedKey) =>
    selectedKey.startsWith('folder:')
      ? canUse(identity, 'read', activeBucket, selectedKey.slice('folder:'.length))
      : canUse(identity, 'read', activeBucket, selectedKey)
  );
  const canDeleteSelected = Boolean(activeBucket) && selectedKeys.length > 0 && selectedKeys.every((selectedKey) =>
    selectedKey.startsWith('folder:')
      ? canUse(identity, 'delete', activeBucket, selectedKey.slice('folder:'.length))
      : canUse(identity, 'delete', activeBucket, selectedKey)
  );
  const canReadSelectedObject = Boolean(activeBucket && s3.selected) && canUse(identity, 'read', activeBucket, s3.selected?.key ?? '');
  const canDeleteSelectedObject = Boolean(activeBucket && s3.selected) && canUse(identity, 'delete', activeBucket, s3.selected?.key ?? '');
  const canCopyFromActiveBucket = canReadSelected && canWriteActiveBucket;
  const canMoveFromActiveBucket = canCopyFromActiveBucket && canDeleteSelected;
  const canReadActiveBucket = !activeBucket || canUse(identity, 'read', activeBucket) || canUse(identity, 'list', activeBucket);
  const accountMenu = (includeBrowserToggles = false) => (
    <AccountMenu
      identityLabel={identity?.user?.name || currentAccessKey || 'user'}
      canAdmin={canAdmin}
      onBrowserClick={() => navigate('browse')}
      onSettingsClick={() => navigate('admin')}
      onDocsClick={() => navigate('docs')}
      onLogout={handleLogout}
      showHidden={includeBrowserToggles ? s3.showHidden : undefined}
      onToggleHidden={includeBrowserToggles ? () => s3.setShowHidden(!s3.showHidden) : undefined}
      placement="down"
      compact
      avatarOnly
    />
  );
  const requestCreateBucket = () => {
    if (!canCreateBucket) return;
    navigate('browse');
    setSiderOpen(true);
    setCreateBucketFocusSignal((n) => n + 1);
  };
  const openUpload = () => {
    if (!canWriteActiveBucket) return;
    navigate('upload');
    setSiderOpen(false);
  };

  // Stable context value so consumers of `useNavigation()` (AdminPage,
  // Sidebar, TopBar) don't re-render on every App render. Previously
  // the inline `value={{ navigate, subPath }}` allocated a fresh
  // object each time, which React treats as a changed context → every
  // consumer re-renders, cascading into every admin panel's
  // data-fetch effect. `navigate` is already `useCallback`'d;
  // `subPath` is derived state that only changes on actual navigation.
  //
  // MUST be declared before any early return below — otherwise the
  // hook count differs between "loading" and "ready" renders and
  // React throws #310 (rendered more hooks than previous render).
  const navValue = useMemo(() => ({ navigate, subPath }), [navigate, subPath]);

  if (sessionLoading) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: '100vh' }}>
        <Spin size="large" description="Restoring session..." />
      </div>
    );
  }

  if (needsConnect) {
    return (
      <ConnectPage
        onConnect={() => setNeedsConnect(false)}
        showError={hasCredentials()}
      />
    );
  }

  const renderContent = () => {
    if (view === 'admin') {
      return (
        <AdminPage
          onBack={navigateToBrowse}
          onSessionExpired={navigateToBrowse}
          subPath={subPath}
          accountMenu={accountMenu()}
        />
      );
    }

    if (view === 'metrics') {
      return <MetricsPage onBack={navigateToBrowse} />;
    }

    if (view === 'docs') {
      return <DocsPage onBack={navigateToBrowse} docId={subPath || undefined} accountMenu={accountMenu()} />;
    }

    if (view === 'upload') {
      if (!canWriteActiveBucket) {
        return (
          <Empty
            description="You do not have permission to upload to this bucket."
            style={{ padding: '64px 0' }}
          />
        );
      }
      return (
        <UploadPage
          prefix={s3.prefix}
          onBack={navigateToBrowse}
          onDone={() => s3.mutate()}
        />
      );
    }

    return (
      <>
        {s3.selectedKeys.size > 0 && (
          <BulkActionBar
            selectedCount={s3.selectedKeys.size}
            onDelete={canDeleteSelected ? s3.bulkDelete : undefined}
            onCopy={canCopyFromActiveBucket ? s3.bulkCopy : undefined}
            onMove={canMoveFromActiveBucket ? s3.bulkMove : undefined}
            onDownloadZip={canReadSelected ? s3.downloadZip : undefined}
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
                  : hasNoBuckets
                    ? 'Create a bucket before uploading objects or generating demo data.'
                    : s3.prefix
                      ? 'This folder is empty.'
                      : 'No objects yet. Upload files or generate demo data.'
              }
              style={{ padding: '64px 0' }}
            >
              {hasNoBuckets && canCreateBucket && (
                <Button type="link" onClick={requestCreateBucket} style={{ paddingInline: 0 }}>
                  Create a bucket
                </Button>
              )}
              {isRootBucketEmpty && canWriteActiveBucket && (
                <Space direction="vertical" size={4} align="center">
                  <DemoDataGenerator
                    onDone={s3.mutate}
                    variant="empty-state"
                    label={`Add demo data to ${activeBucket}`}
                  />
                  <Button type="link" onClick={openUpload} style={{ paddingInline: 0 }}>
                    Upload from computer
                  </Button>
                </Space>
              )}
            </Empty>
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
    <NavigationContext.Provider value={navValue}>
    <Layout style={{ minHeight: '100vh', background: colors.BG_BASE }}>
      {/* Skip to content link */}
      <a href="#main-content" className="sr-only sr-only-focusable">
        Skip to main content
      </a>

      <Layout style={{ flexDirection: 'row', flex: 1 }}>
        {!FULLSCREEN_VIEWS.has(view) && (
          <Sidebar
            onUploadClick={openUpload}
            onBucketChange={handleBucketChange}
            onBucketsChanged={setBucketCount}
            createBucketFocusSignal={createBucketFocusSignal}
            canCreateBucket={canCreateBucket}
            canDeleteBucket={(bucket) => canUse(identity, 'admin', bucket)}
            canUpload={canWriteActiveBucket}
            open={siderOpen}
            onClose={() => setSiderOpen(false)}
            isMobile={isMobile}
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
              canRefresh={canReadActiveBucket}
              searchQuery={s3.searchQuery}
              onSearchChange={s3.setSearchQuery}
              refreshing={s3.refreshing}
              accountMenu={accountMenu(true)}
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
        canDelete={canDeleteSelectedObject}
        canRead={canReadSelectedObject}
      />

      <FilePreview
        open={previewObject !== null}
        object={previewObject}
        onClose={() => setPreviewObject(null)}
      />
      {view === 'browser' && canWriteActiveBucket && <DropZone onDrop={s3.uploadFiles} prefix={s3.prefix} />}
    </Layout>
    </NavigationContext.Provider>
  );
}
