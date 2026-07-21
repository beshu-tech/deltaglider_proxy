import { useState, useEffect, useRef, useCallback, useMemo, lazy, Suspense } from 'react';
import { Alert, Layout, Spin, Empty, Grid, Button, Progress, Space } from 'antd';
import useS3Browser from './useS3Browser';
import TopBar from './components/TopBar';
import BulkActionBar from './components/BulkActionBar';
import { useBucketMaintenance } from './queries/maintenance';
import { browserBannerText, phaseLabel, activePercent } from './maintenanceStatus';
import Sidebar from './components/Sidebar';
import ObjectTable from './components/ObjectTable';
import InspectorPanel from './components/InspectorPanel';
import FilePreview from './components/FilePreview';
import DropZone from './components/DropZone';
import UploadPage from './components/UploadPage';
import ConnectPage, { type ConnectOutcome } from './components/ConnectPage';
// Heavy admin/docs/metrics pages are lazy-loaded so the file-browser
// shell doesn't pay for Monaco / mermaid / recharts on first paint.
const AdminPage = lazy(() => import('./components/AdminPage'));
const MetricsPage = lazy(() => import('./components/MetricsPage'));
const DocsPage = lazy(() => import('./components/DocsPage'));
import FileBrowserSessionTip from './components/FileBrowserSessionTip';
import AccountMenu from './components/AccountMenu';
import DemoDataGenerator from './components/DemoDataGenerator';
import ShortcutsHelp from './components/ShortcutsHelp';
import { useGlobalShortcuts } from './useGlobalShortcuts';
import { useBrowserKeyboardNav } from './useBrowserKeyboardNav';
import { getBucket, hasCredentials, disconnect, initFromSession, getCredentials } from './s3client';
import { clearSessionCredentials } from './sessionApi';
import { adminLogout, whoami, checkSession, resolveIamIdentity } from './adminApi';
import type { WhoamiResponse } from './adminApi';
import { deriveSessionCapabilities } from './sessionCapabilities';
import { useColors } from './ThemeContext';
import useComputeSize from './useComputeSize';
import { NavigationContext } from './NavigationContext';
import { canRequestPrefixUsageScan, canUse, writablePrefixesForBucket } from './permissions';
import { useUrlRouter } from './useUrlRouter';
import { buildViewUrl, buildBrowserUrl, type View } from './urlState';
import { useOverlayClose } from './hooks/useOverlayClose';
import type { S3Object } from './types';

const { Content } = Layout;
const { useBreakpoint } = Grid;

/** Full-screen views hide the main sidebar and TopBar */
const FULLSCREEN_VIEWS: Set<View> = new Set(['admin', 'docs']);

// Shared Suspense fallback for lazy admin / metrics / docs page chunks.
// Module-scope so we don't reallocate it on every render.
const LAZY_FALLBACK = (
  <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', padding: 48 }}>
    <Spin size="large" />
  </div>
);

export default function App() {
  const colors = useColors();

  const { view, subPath, browser, navigate, search } = useUrlRouter();
  // Stable "go back to browser" callback. Previously inlined into
  // AdminPage / MetricsPage props as `() => navigate('browse')`, which
  // allocated a fresh arrow every App render and propagated as
  // `onBack` / `onSessionExpired` to ~10 admin panels. Each panel's
  // `loadData` `useCallback` lists `onSessionExpired` in its deps,
  // so a fresh prop → fresh callback → `useEffect([loadData])` fires
  // → re-fetches → setState → re-render wave. This one-line fix
  // eliminates the most visible cause of excessive re-renders when
  // navigating between admin sub-pages. `navigate` is stable, and
  // `buildViewUrl('browser')` is a constant, so this stays stable too.
  const navigateToBrowse = useCallback(() => navigate(buildViewUrl('browser')), [navigate]);
  const [siderOpen, setSiderOpen] = useState(false);
  const [needsConnect, setNeedsConnect] = useState(true); // start true, resolved in useEffect
  const [sessionLoading, setSessionLoading] = useState(true);
  const [firstLoadDone, setFirstLoadDone] = useState(false);
  const [previewObject, setPreviewObject] = useState<import('./types').S3Object | null>(null);
  const [identity, setIdentity] = useState<WhoamiResponse | null>(null);
  const [bucketCount, setBucketCount] = useState<number | null>(null);
  const [createBucketFocusSignal, setCreateBucketFocusSignal] = useState(0);
  // Files dropped onto the browser (from Finder) are staged here and the Upload
  // view is opened so the user can confirm/adjust the destination prefix before
  // they commit. Consumed once by UploadPage on mount, then cleared.
  const [droppedFiles, setDroppedFiles] = useState<File[]>([]);
  // Destination prefix captured at the moment the Upload view was opened (drop
  // or sidebar button). Needed because navigating to /_/upload wipes
  // browser.prefix from the URL, so uploadPrefix recomputes to '' — without
  // this capture the upload view would always open at bucket root. A stale
  // value across history back/forward into /_/upload is fine: it restores the
  // previous upload session's target, and the destination is editable anyway.
  const [uploadSeedPrefix, setUploadSeedPrefix] = useState<string | null>(null);
  // App-wide keyboard-shortcuts help modal (opened by `?`, the header help
  // icon, and the admin command palette). Owned here so a single instance
  // serves every view.
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const showShortcuts = useCallback(() => setShortcutsOpen(true), []);

  /** Any valid session cookie (administrator sign-in or access-key file browser). */
  const [sessionValid, setSessionValid] = useState(false);
  const [sessionCaps, setSessionCaps] = useState(() =>
    deriveSessionCapabilities({ valid: false, admin_gui: false }),
  );
  const hasAdminSession = sessionCaps.adminGui;
  // The URL is the source of truth for the active bucket (browser.bucket).
  // Fall back to the s3client module state during the brief window before the
  // first bucket is chosen / synced.
  const activeBucket = browser.bucket || getBucket();
  // Maintenance (re-encryption) status for the active bucket. Polls fast
  // while a job runs; the banner self-dismisses on completion. Reads keep
  // working — only write affordances are disabled below.
  const bucketMaintenance = useBucketMaintenance(view === 'browser' ? activeBucket || null : null).data?.active ?? null;
  const bucketBusy = bucketMaintenance !== null;
  // Canonicalize the URL once a bucket is active but absent from the path
  // (landing at bare /_/browse: the Sidebar selects the first bucket into
  // s3client module state, but nothing put it in the URL). Without the bucket
  // segment, buildBrowserUrl drops the prefix, so folder clicks silently no-op
  // until the user switches buckets. REPLACE (not push) so it doesn't add a
  // spurious history entry. Fixes the v1.3.1 "folders don't open at /_/browse".
  useEffect(() => {
    if (view === 'browser' && !browser.bucket && activeBucket) {
      navigate(buildBrowserUrl({ bucket: activeBucket }), { replace: true });
    }
  }, [view, browser.bucket, activeBucket, navigate]);
  const writablePrefixes = useMemo(
    () => writablePrefixesForBucket(identity, activeBucket),
    [identity, activeBucket],
  );
  const s3 = useS3Browser({
    writablePrefixes,
    bucket: browser.bucket,
    prefix: browser.prefix,
    q: browser.q,
    object: browser.object,
    navigateUrl: navigate,
  });

  // --- FilePreview deep-link (?object=…&preview=1) ---
  // Opening the preview pushes ?preview=1 alongside ?object=… so the overlay
  // is deep-linkable, reload-safe, and Back closes it without over-navigating
  // out of the folder.
  const { markPushed: markPreviewPushed, closeOverlay: closePreviewOverlay } = useOverlayClose();

  const openPreview = useCallback(
    (obj: S3Object) => {
      setPreviewObject(obj);
      navigate(buildBrowserUrl({ ...browser, object: obj.key, preview: '1' }));
      markPreviewPushed();
    },
    [browser, navigate, markPreviewPushed],
  );

  // Close: remove preview=1 but keep ?object=… so the inspector stays open.
  const closePreview = useCallback(() => {
    const key = previewObject?.key ?? browser.object;
    closePreviewOverlay(buildBrowserUrl({ ...browser, object: key, preview: '' }), navigate);
    setPreviewObject(null);
  }, [browser, previewObject, navigate, closePreviewOverlay]);

  // Sync preview state with the URL: open on direct-load with ?preview=1,
  // close when Back / navigation removes the flag.
  useEffect(() => {
    if (browser.preview === '1' && browser.object && !previewObject) {
      const obj = s3.objects.find((o) => o.key === browser.object);
      if (obj) setPreviewObject(obj);
    }
    if (browser.preview !== '1' && previewObject) {
      setPreviewObject(null);
    }
  }, [browser.preview, browser.object, previewObject, s3.objects]);

  // App-wide shortcuts: ⌘/Ctrl+, → Settings, ⌘/Ctrl+/ → Docs, ? → help.
  // Disabled on the connect/login screen so we don't hijack keys there.
  useGlobalShortcuts({
    onSettings: () => navigate(buildViewUrl('admin')),
    onDocs: () => navigate(buildViewUrl('docs')),
    onHelp: showShortcuts,
    enabled: !needsConnect,
  });
  // Arrow-key navigation of the object browser (only while the browser view is
  // active). Returns the keyboard cursor for ObjectTable to highlight + follow.
  const browserNav = useBrowserKeyboardNav({
    folders: s3.folders,
    objects: s3.objects,
    prefix: s3.prefix,
    navigate: s3.navigate,
    openInspector: s3.openInspector,
    enabled: view === 'browser' && !needsConnect,
  });
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

  // ConnectPage tells us what kind of session it just established, so we can
  // light up (or gate) admin-only UI optimistically instead of waiting for the
  // checkSession round-trip. refreshSessionGate() still runs as the authoritative
  // reconcile — the optimistic set just avoids a flash of wrong capabilities.
  const onConnectComplete = useCallback((outcome: ConnectOutcome) => {
    setSessionCaps(
      deriveSessionCapabilities({ valid: true, admin_gui: outcome.kind === 'admin' }),
    );
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
    navigate(buildViewUrl('browser'));
  };

  const handleBucketChange = useCallback((newBucket: string) => {
    // changeBucket() already navigates the URL to /browse/<bucket>/ (PUSH).
    changeS3Bucket(newBucket);
  }, [changeS3Bucket]);

  const isEmpty = s3.objects.length === 0 && s3.folders.length === 0;
  const hasBuckets = (bucketCount ?? 0) > 0;
  const hasNoBuckets = bucketCount === 0;
  const isRootBucketEmpty = hasBuckets && s3.prefix === '' && !s3.searchQuery && isEmpty && !s3.loading;
  const currentAccessKey = getCredentials().accessKeyId || undefined;
  // A session can also belong to a non-admin external SSO user. Only
  // show Settings after whoami resolves admin/open/bootstrap authority.
  const canAdmin = identity?.mode === 'bootstrap' || identity?.mode === 'open' || identity?.user?.is_admin === true;
  const canCreateBucket = canUse(identity, 'admin');
  const canWriteActivePrefix = Boolean(activeBucket) && !bucketBusy && canUse(identity, 'write', activeBucket, s3.prefix);
  const uploadFallbackPrefix = writablePrefixes[0] ?? null;
  const uploadPrefix = canWriteActivePrefix
    ? s3.prefix
    : (uploadFallbackPrefix ?? s3.prefix);
  const canUploadToActiveBucket = Boolean(activeBucket) && !bucketBusy && (canWriteActivePrefix || uploadFallbackPrefix !== null);
  const selectedKeys = Array.from(s3.selectedKeys);
  const canReadSelected = Boolean(activeBucket) && selectedKeys.length > 0 && selectedKeys.every((selectedKey) =>
    selectedKey.startsWith('folder:')
      ? canUse(identity, 'read', activeBucket, selectedKey.slice('folder:'.length))
      : canUse(identity, 'read', activeBucket, selectedKey)
  );
  const canDeleteSelected = Boolean(activeBucket) && !bucketBusy && selectedKeys.length > 0 && selectedKeys.every((selectedKey) =>
    selectedKey.startsWith('folder:')
      ? canUse(identity, 'delete', activeBucket, selectedKey.slice('folder:'.length))
      : canUse(identity, 'delete', activeBucket, selectedKey)
  );
  const canReadSelectedObject = Boolean(activeBucket && s3.inspectorObject) && canUse(identity, 'read', activeBucket, s3.inspectorObject?.key ?? '');
  const canDeleteSelectedObject = Boolean(activeBucket && s3.inspectorObject) && canUse(identity, 'delete', activeBucket, s3.inspectorObject?.key ?? '');
  const canCopyFromActiveBucket = canReadSelected && canWriteActivePrefix;
  const canMoveFromActiveBucket = canCopyFromActiveBucket && canDeleteSelected;
  const canReadActiveBucket = !activeBucket || canUse(identity, 'read', activeBucket, s3.prefix) || canUse(identity, 'list', activeBucket, s3.prefix);
  const accountMenu = (includeBrowserToggles = false) => (
    <AccountMenu
      identityLabel={identity?.user?.name || currentAccessKey || 'user'}
      canAdmin={canAdmin}
      onBrowserClick={() => navigate(buildViewUrl('browser'))}
      onSettingsClick={() => navigate(buildViewUrl('admin'))}
      onDocsClick={() => navigate(buildViewUrl('docs'))}
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
    navigate(buildViewUrl('browser'));
    setSiderOpen(true);
    setCreateBucketFocusSignal((n) => n + 1);
  };
  const openUpload = () => {
    if (!canUploadToActiveBucket) return;
    // Capture the destination BEFORE the URL changes: navigating to /_/upload
    // resets browser.prefix (and so uploadPrefix) to '', which would seed the
    // upload view at bucket root instead of the folder the user is in.
    setUploadSeedPrefix(uploadPrefix);
    navigate(buildViewUrl('upload'));
    setSiderOpen(false);
  };
  // Finder→Chrome drop on the browser: stage the files (folders already
  // flattened to real files by DropZone) and open the Upload view at the
  // current folder so the user confirms/adjusts the destination before
  // committing — rather than silently uploading in place.
  const handleBrowserDrop = useCallback((files: File[]) => {
    if (!canUploadToActiveBucket || files.length === 0) return;
    setDroppedFiles(files);
    // Same prefix capture as openUpload — see the comment there.
    setUploadSeedPrefix(uploadPrefix);
    navigate(buildViewUrl('upload'));
    setSiderOpen(false);
  }, [canUploadToActiveBucket, uploadPrefix, navigate]);

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
            search={search}
            accountMenu={accountMenu()}
            canAdmin={canAdmin}
            onShowShortcuts={showShortcuts}
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
          <MetricsPage onBack={navigateToBrowse} search={search} />
        </Suspense>
      );
    }

    if (view === 'docs') {
      return (
        <Suspense fallback={LAZY_FALLBACK}>
          <DocsPage onBack={navigateToBrowse} docId={subPath || undefined} accountMenu={accountMenu()} onShowShortcuts={showShortcuts} />
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
          prefix={uploadSeedPrefix ?? uploadPrefix}
          initialFiles={droppedFiles}
          onConsumeInitialFiles={() => setDroppedFiles([])}
          onBack={navigateToBrowse}
          onDone={() => s3.mutate()}
          onFinish={(destPrefix) => {
            if (activeBucket) {
              navigate(buildBrowserUrl({ bucket: activeBucket, prefix: destPrefix }));
            } else {
              navigateToBrowse();
            }
          }}
        />
      );
    }

    return (
      <>
        {bucketMaintenance && (
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 14,
              padding: '10px 16px',
              background: 'rgba(217, 119, 6, 0.10)',
              borderBottom: '1px solid rgba(217, 119, 6, 0.35)',
            }}
          >
            <Spin size="small" />
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontSize: 13, fontWeight: 600 }}>
                {browserBannerText(bucketMaintenance)}
              </div>
              <Progress
                percent={activePercent(bucketMaintenance) ?? 100}
                status="active"
                showInfo={activePercent(bucketMaintenance) != null}
                size="small"
                strokeColor="#d97706"
                style={{ margin: '4px 0 0', maxWidth: 420 }}
              />
            </div>
            <span style={{ fontSize: 12, opacity: 0.75, whiteSpace: 'nowrap' }}>
              {phaseLabel(bucketMaintenance)}
            </span>
          </div>
        )}
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
          {/* An unrefreshed FAULT always outranks the friendly empty state:
              a failed listing must never render as "No objects yet — add
              demo data" (the beshu-b2 incident: a bucket whose backend was
              unreachable looked innocently empty). With stale rows on hand
              we show them under a warning banner instead. */}
          {s3.error && !isEmpty && (
            <Alert
              type="warning"
              showIcon
              banner
              message="Showing a stale listing — the last refresh failed"
              description={s3.error}
              style={{ borderRadius: 0 }}
            />
          )}
          {s3.loading && isEmpty ? (
            <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', padding: '64px 0' }}>
              <Spin description="Loading objects..." />
            </div>
          ) : s3.error && isEmpty ? (
            <div style={{ maxWidth: 560, margin: '48px auto', padding: '0 16px' }}>
              <Alert
                type="error"
                showIcon
                message={
                  /NoSuchBucket/i.test(s3.error)
                    ? `Bucket '${activeBucket}' was not found on its storage backend`
                    : /ServiceUnavailable|unavailable|503/i.test(s3.error)
                      ? `Bucket '${activeBucket}' is currently unavailable`
                      : 'Failed to load objects'
                }
                description={
                  <Space direction="vertical" size={8}>
                    <span>{s3.error}</span>
                    {/NoSuchBucket/i.test(s3.error) && (
                      <span>
                        The bucket exists in this proxy's configuration but not on the
                        backend it routes to — check the bucket's backend under
                        Settings → Storage → Buckets, or create it on that backend.
                      </span>
                    )}
                    <Button size="small" onClick={s3.mutate}>
                      Retry
                    </Button>
                  </Space>
                }
                style={{ borderRadius: 8 }}
              />
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
              selected={s3.inspectorObject}
              onSelect={(obj) => s3.openInspector(obj.key)}
              onNavigate={s3.navigate}
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
              onPreview={openPreview}
              cursorKey={browserNav.cursorKey}
              onCursorChange={browserNav.setCursorKey}
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
              bucket={activeBucket}
              prefix={s3.prefix}
              onNavigate={s3.navigate}
              isMobile={isMobile}
              onMenuClick={() => setSiderOpen(true)}
              onRefresh={s3.mutate}
              canRefresh={canReadActiveBucket}
              searchQuery={s3.searchQuery}
              onSearchChange={s3.setSearchQuery}
              refreshing={s3.refreshing}
              canAdmin={canAdmin}
              onShowShortcuts={showShortcuts}
              accountMenu={accountMenu(true)}
              deltaSummary={view === 'browser' ? s3.deltaSummary : null}
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
        object={s3.inspectorObject}
        onClose={s3.closeInspector}
        onDeleted={s3.mutate}
        onPreview={openPreview}
        isMobile={isMobile}
        headCache={s3.headCache}
        canDelete={canDeleteSelectedObject}
        canRead={canReadSelectedObject}
        hasAdminSession={sessionCaps.canFetchFullAdminConfig}
      />

      <FilePreview
        open={previewObject !== null}
        object={previewObject}
        onClose={closePreview}
      />
      {shortcutsOpen && (
        <ShortcutsHelp open={shortcutsOpen} onClose={() => setShortcutsOpen(false)} />
      )}
      {view === 'browser' && canWriteActivePrefix && <DropZone onDrop={handleBrowserDrop} prefix={s3.prefix} />}
    </Layout>
    </NavigationContext.Provider>
  );
}
