import { useState, useEffect, useRef, useCallback, useMemo, lazy, Suspense } from 'react';
import { Layout, Spin, Empty, Grid, Button, Space } from 'antd';
import useS3Browser from './useS3Browser';
import TopBar from './components/TopBar';
import BulkActionBar from './components/BulkActionBar';
import Sidebar from './components/Sidebar';
import ObjectTable from './components/ObjectTable';
import InspectorPanel from './components/InspectorPanel';
import FilePreview from './components/FilePreview';
import DropZone from './components/DropZone';
import UploadPage from './components/UploadPage';
import ConnectPage from './components/ConnectPage';
// Heavy admin/docs/metrics pages are lazy-loaded so the file-browser
// shell doesn't pay for Monaco / mermaid / recharts on first paint.
const AdminPage = lazy(() => import('./components/AdminPage'));
const MetricsPage = lazy(() => import('./components/MetricsPage'));
const DocsPage = lazy(() => import('./components/DocsPage'));
import FileBrowserSessionTip from './components/FileBrowserSessionTip';
import AccountMenu from './components/AccountMenu';
import DemoDataGenerator from './components/DemoDataGenerator';
import { getBucket, hasCredentials, disconnect, initFromSession, getCredentials } from './s3client';
import { clearSessionCredentials } from './sessionApi';
import { adminLogout, whoami, checkSession, resolveIamIdentity } from './adminApi';
import type { WhoamiResponse } from './adminApi';
import { deriveSessionCapabilities } from './sessionCapabilities';
import { useColors } from './ThemeContext';
import useComputeSize from './useComputeSize';
import { NavigationContext } from './NavigationContext';
import { canRequestPrefixUsageScan, canUse, writablePrefixesForBucket } from './permissions';

const { Content } = Layout;
const { useBreakpoint } = Grid;

type View = 'browser' | 'upload' | 'metrics' | 'docs' | 'admin';

/** Full-screen views hide the main sidebar and TopBar */
const FULLSCREEN_VIEWS: Set<View> = new Set(['admin', 'docs']);

const BASE = '/_/';

// Shared Suspense fallback for lazy admin / metrics / docs page chunks.
// Module-scope so we don't reallocate it on every render.
const LAZY_FALLBACK = (
  <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', padding: 48 }}>
    <Spin size="large" />
  </div>
);

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

  /** Any valid session cookie (administrator sign-in or access-key file browser). */
  const [sessionValid, setSessionValid] = useState(false);
  const [sessionCaps, setSessionCaps] = useState(() =>
    deriveSessionCapabilities({ valid: false, admin_gui: false }),
  );
  const hasAdminSession = sessionCaps.adminGui;
  const activeBucket = getBucket();
  const writablePrefixes = useMemo(
    () => writablePrefixesForBucket(identity, activeBucket),
    [identity, activeBucket],
  );
  const s3 = useS3Browser({ writablePrefixes });
  const folderSize = useComputeSize();
  const computeSize = folderSize.compute;
  const reconnectS3 = s3.reconnect;
  const changeS3Bucket = s3.changeBucket;
  const cancelFolderSizes = folderSize.cancelAll;
  const computeFolderSize = useCallback((folderPrefix: string) => {
    if (!canRequestPrefixUsageScan(folderPrefix, s3.virtualFolders, hasAdminSession)) return;
    computeSize(folderPrefix);
  }, [computeSize, s3.virtualFolders, hasAdminSession]);

  const refreshSessionGate = useCallback(async () => {
    const restored = await initFromSession().catch(() => false);
    try {
      const session = await checkSession();
      setSessionValid(session.valid);
      setSessionCaps(deriveSessionCapabilities(session));
      setNeedsConnect(!(restored || session.valid));
    } catch {
      setSessionValid(false);
      setSessionCaps(deriveSessionCapabilities({ valid: false, admin_gui: false }));
      setNeedsConnect(!restored);
    }
  }, []);

  const onConnectComplete = useCallback(() => {
    void refreshSessionGate();
  }, [refreshSessionGate]);

  const pollSession = useCallback(async () => {
    try {
      const session = await checkSession();
      setSessionValid(session.valid);
      setSessionCaps(deriveSessionCapabilities(session));
    } catch {
      /* keep last-known session snapshot on transient errors */
    }
  }, []);

  // Restore credentials from server-side session on mount; same path after ConnectPage.
  useEffect(() => {
    refreshSessionGate().catch(() => setNeedsConnect(true)).finally(() => {
      setSessionLoading(false);
    });
  }, [refreshSessionGate]);

  useEffect(() => {
    if (needsConnect || sessionLoading) return;
    const id = window.setInterval(() => void pollSession(), 5 * 60 * 1000);
    return () => clearInterval(id);
  }, [needsConnect, sessionLoading, pollSession]);

  // When session is restored and we're connected, reload the S3 browser
  // and fetch identity. Runs AFTER React commits the needsConnect state.
  useEffect(() => {
    if (!needsConnect && !sessionLoading) {
      let cancelled = false;
      reconnectS3();
      (async () => {
        const nextIdentity = await whoami();
        if (nextIdentity.mode === 'iam' && !nextIdentity.user) {
          const creds = getCredentials();
          if (creds.accessKeyId && creds.secretAccessKey) {
            const resolved = await resolveIamIdentity(creds.accessKeyId, creds.secretAccessKey);
            if (resolved) {
              if (!cancelled) setIdentity(resolved);
              return;
            }
          }
        }
        if (!cancelled) setIdentity(nextIdentity);
      })();
      return () => {
        cancelled = true;
      };
    } else if (needsConnect) {
      setIdentity(null);
      setSessionValid(false);
      setSessionCaps(deriveSessionCapabilities({ valid: false, admin_gui: false }));
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
  // If we have a valid session cookie, don't bounce to ConnectPage
  // when S3 calls fail — the user may lack S3 permissions or have a transient error.
  useEffect(() => {
    if (!s3.loading && !firstLoadDone) {
      setFirstLoadDone(true);
      if (!s3.connected && !sessionValid) {
        setNeedsConnect(true);
      }
    }
  }, [s3.loading, s3.connected, firstLoadDone, sessionValid]);

  const handleLogout = async () => {
    try {
      await clearSessionCredentials();
      await adminLogout();
    } catch {
      /* still leave the app shell */
    }
    disconnect();
    try {
      sessionStorage.setItem('dg-session-user-signed-out', '1');
    } catch {
      /* private mode */
    }
    setFirstLoadDone(false);
    setNeedsConnect(true);
    setIdentity(null);
    setSessionValid(false);
    setSessionCaps(deriveSessionCapabilities({ valid: false, admin_gui: false }));
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
  const currentAccessKey = getCredentials().accessKeyId || undefined;
  // A session can also belong to a non-admin external SSO user. Only
  // show Settings after whoami resolves admin/open/bootstrap authority.
  const canAdmin = identity?.mode === 'bootstrap' || identity?.mode === 'open' || identity?.user?.is_admin === true;
  const canCreateBucket = canUse(identity, 'admin');
  const canWriteActivePrefix = Boolean(activeBucket) && canUse(identity, 'write', activeBucket, s3.prefix);
  const uploadFallbackPrefix = writablePrefixes[0] ?? null;
  const uploadPrefix = canWriteActivePrefix
    ? s3.prefix
    : (uploadFallbackPrefix ?? s3.prefix);
  const canUploadToActiveBucket = Boolean(activeBucket) && (canWriteActivePrefix || uploadFallbackPrefix !== null);
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
  const canCopyFromActiveBucket = canReadSelected && canWriteActivePrefix;
  const canMoveFromActiveBucket = canCopyFromActiveBucket && canDeleteSelected;
  const canReadActiveBucket = !activeBucket || canUse(identity, 'read', activeBucket, s3.prefix) || canUse(identity, 'list', activeBucket, s3.prefix);
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
    if (!canUploadToActiveBucket) return;
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
        onConnect={onConnectComplete}
        showError={hasCredentials()}
      />
    );
  }

  const renderContent = () => {
    if (view === 'admin') {
      return (
        <Suspense fallback={LAZY_FALLBACK}>
          <AdminPage
            onBack={navigateToBrowse}
            onSessionExpired={navigateToBrowse}
            subPath={subPath}
            accountMenu={accountMenu()}
            canAdmin={canAdmin}
          />
        </Suspense>
      );
    }

    if (view === 'metrics') {
      if (!hasAdminSession) {
        return (
          <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', padding: 48 }}>
            <Empty
              description="Sign in through Settings to view metrics and analytics. You are currently signed in for browsing files only (for example after using an access key on the sign-in screen)."
            >
              <Button type="primary" onClick={navigateToBrowse}>Back to Browser</Button>
            </Empty>
          </div>
        );
      }
      return (
        <Suspense fallback={LAZY_FALLBACK}>
          <MetricsPage onBack={navigateToBrowse} />
        </Suspense>
      );
    }

    if (view === 'docs') {
      return (
        <Suspense fallback={LAZY_FALLBACK}>
          <DocsPage onBack={navigateToBrowse} docId={subPath || undefined} accountMenu={accountMenu()} />
        </Suspense>
      );
    }

    if (view === 'upload') {
      if (!canUploadToActiveBucket) {
        return (
          <Empty
            description="You do not have permission to upload to this bucket."
            style={{ padding: '64px 0' }}
          />
        );
      }
      return (
        <UploadPage
          prefix={uploadPrefix}
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
            onDelete={canDeleteSelected && hasAdminSession ? s3.bulkDelete : undefined}
            onCopy={canCopyFromActiveBucket && hasAdminSession ? s3.bulkCopy : undefined}
            onMove={canMoveFromActiveBucket && hasAdminSession ? s3.bulkMove : undefined}
            onDownloadZip={canReadSelected && hasAdminSession ? s3.downloadZip : undefined}
            deleting={s3.deleting}
            hint={
              hasAdminSession
                ? undefined
                : 'Bulk copy, move, ZIP, and delete are available after you open Settings and sign in as an administrator. Access-key sign-in is for browsing files only.'
            }
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
              {isRootBucketEmpty && canUploadToActiveBucket && (
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
              virtualFolders={s3.virtualFolders}
              hasAdminSession={hasAdminSession}
              onComputeSize={computeFolderSize}
              onCancelSize={folderSize.cancel}
              onAutoPopulateSizes={hasAdminSession ? folderSize.autoPopulate : undefined}
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
            canUpload={canUploadToActiveBucket}
            canAdmin={canAdmin}
            includeBucketOrigins={sessionCaps.canLoadBucketOrigins}
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
              <FileBrowserSessionTip visible={view === 'browser' && sessionCaps.signedInForFilesOnly} />
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
        hasAdminSession={sessionCaps.canFetchFullAdminConfig}
      />

      <FilePreview
        open={previewObject !== null}
        object={previewObject}
        onClose={() => setPreviewObject(null)}
      />
      {view === 'browser' && canWriteActivePrefix && <DropZone onDrop={s3.uploadFiles} prefix={s3.prefix} />}
    </Layout>
    </NavigationContext.Provider>
  );
}
