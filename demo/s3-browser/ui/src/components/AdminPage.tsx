import { cloneElement, isValidElement, useState, useEffect, useCallback, useMemo } from 'react';
import { Button, Spin, Drawer } from 'antd';
import { checkSession, whoami, loginAs, isNotAdminDenial, type ExternalProviderInfo, type LoginAsResult } from '../adminApi';
import { getCredentials } from '../s3client';
import { MenuOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { useIsNarrow } from '../useIsNarrow';
import FullScreenHeader from './FullScreenHeader';
import AdminSidebar from './AdminSidebar';
import { ADMIN_IA } from './adminNavigation';
import { findEntry } from '../adminNavTree';
import CommandPalette, {
  FileTextOutlined as PaletteFileTextOutlined,
  ImportOutlined as PaletteImportOutlined,
  RocketOutlined,
  LogoutOutlined,
  QuestionCircleOutlined,
} from './CommandPalette';
import { useNavigation } from '../NavigationContext';
import { resolveAdminPath as remapAdminPath } from '../adminPathRemap';
import { buildViewUrl, parseAdminQuery } from '../urlState';
import { useOverlayClose } from '../hooks/useOverlayClose';
import { useBackupImportExport } from '../hooks/useBackupImportExport';
import { YamlImportExportModal } from './YamlImportExportModal';
import { FullIamYamlModal } from './FullIamYamlModal';
import { useDirtyGlobalIndicators, requestApplyFirst } from '../useDirtySection';
import type { SectionName } from '../adminApi';
import type { AccountMenuConfigProps } from './AccountMenu';
import { normalizeUiError } from '../errorHandling';
import { AdminRouteContent, type AdminRouteContext } from './admin/adminRoutes';
import { AdminAccessDenied, AdminLoginGate } from './admin/AdminLoginGate';
import RestoreBackupModal from './admin/RestoreBackupModal';

/**
 * Resolve an incoming `subPath` to a canonical leaf of the 7-group IA.
 * The exhaustive old→new table lives in `adminPathRemap.ts` (regression-
 * tested); membership in the live IA is the passthrough test.
 */
function resolveAdminPath(subPath: string): string {
  return remapAdminPath(subPath, (p) => Boolean(findEntry(ADMIN_IA, p)));
}

interface AdminPageProps {
  onBack: () => void;
  onSessionExpired?: () => void;
  subPath?: string;
  /** Raw query string (with leading `?`) for admin deep-links (?job=…&tab=…). */
  search?: string;
  accountMenu?: React.ReactNode;
  canAdmin?: boolean;
  /** Open the app-wide keyboard-shortcuts modal (owned by App). */
  onShowShortcuts: () => void;
  /** Running proxy version from whoami (App owns identity). */
  proxyVersion?: string;
}

export default function AdminPage({ onBack, onSessionExpired, subPath, search, accountMenu, canAdmin = false, onShowShortcuts, proxyVersion }: AdminPageProps) {
  const colors = useColors();
  const { navigate } = useNavigation();
  // Hook up the `● ` tab-title prefix + beforeunload guard for any
  // section with unsaved edits. Mounting at AdminPage is the single
  // sensible home; moving higher would fire the guard on non-admin
  // pages, moving lower would miss the case where the operator
  // navigates away from a dirty section.
  useDirtyGlobalIndicators();

  // Command palette (⌘K / Ctrl+K) — Wave 10 polish. Global keydown
  // listener only mounts while AdminPage is up so the shortcut doesn't
  // interfere with other views. The shortcuts-help modal (`?`) is now
  // owned by App (app-wide); AdminPage delegates via `onShowShortcuts`.
  const [paletteOpen, setPaletteOpen] = useState(false);

  // Mobile drawer (Wave 10.1 §10.4). Below 900px the persistent
  // 220px sidebar is replaced with an AntD Drawer that slides in
  // from the left. Hamburger trigger lives in the header extra
  // slot. Auto-closes on navigation (see navigateAdmin below).
  const isNarrow = useIsNarrow(900);
  const [mobileNavOpen, setMobileNavOpen] = useState(false);

  // Derive canonical admin path (§3.2). Legacy flat URLs (`users`,
  // `backends`, etc.) are mapped to the new hierarchy.
  const rawSubPath = (subPath || '').replace(/^\/+/, '').replace(/\/+$/, '');
  const adminPath = resolveAdminPath(subPath || '');
  const activeSection = sectionForPath(adminPath);
  // The per-leaf dirty/apply key for ⌘S dispatch (panels register under this,
  // not the coarse section). `activeSection` is still used for the avatar
  // menu's section-YAML target (which IS section-scoped).
  const activeApplyKeys = useMemo(() => applyKeysForPath(adminPath), [adminPath]);
  const navigateAdmin = useCallback(
    (path: string) => {
      navigate(buildViewUrl('admin', path));
      // Close the mobile drawer (if open) on navigation. Harmless
      // no-op on wide viewports where the drawer is never shown.
      setMobileNavOpen(false);
    },
    [navigate]
  );

  // Canonicalise the URL bar on legacy-flat hits. When the operator
  // lands on `/_/admin/users` (a bookmarked v0.7.x URL), the content
  // already renders the Users panel because `resolveAdminPath` mapped
  // it — but the URL in the bar still reads `/_/admin/users`. Operators
  // pasting the URL elsewhere would still spread the legacy form.
  // `replaceState` silently upgrades the URL to the canonical
  // hierarchical form without adding a history entry. Browser back/
  // forward still works correctly.
  useEffect(() => {
    // Only canonicalise when the resolved path actually differs from
    // the raw sub-path (legacy hit). Skip on the landing page
    // (empty sub-path -> diagnostics/dashboard) — that's a fresh
    // navigation, not a legacy bookmark.
    if (rawSubPath && rawSubPath !== adminPath) {
      // Preserve any query params (e.g. ?job=…&tab=…) that were present
      // on the legacy URL — the canonical path must not wipe them.
      const query = parseAdminQuery(search ?? window.location.search);
      navigate(buildViewUrl('admin', adminPath, Object.keys(query).length > 0 ? query : undefined), { replace: true });
    }
  }, [rawSubPath, adminPath, navigate, search]);

  const [authed, setAuthed] = useState(false);
  const [checkingSession, setCheckingSession] = useState(true);
  const [externalProviders, setExternalProviders] = useState<ExternalProviderInfo[]>([]);
  const [accessDenied, setAccessDenied] = useState(false);
  /** Valid session from access-key / open connect — file browser only, not Settings sign-in. */
  const [s3BrowserSessionOnly, setS3BrowserSessionOnly] = useState(false);
  const [pendingGroupId, setPendingGroupId] = useState<number | null>(null);
  const [loginError, setLoginError] = useState('');
  // YAML import/export modal state. Mode flips between 'import'
  // (paste YAML → validate → apply) and 'export' (fetch current
  // canonical YAML → copy to clipboard).
  const [yamlModalMode, setYamlModalMode] = useState<'import' | 'export' | null>(null);
  const [iamYamlMode, setIamYamlMode] = useState<'import' | 'export' | null>(null);
  const backup = useBackupImportExport();

  // Back-button close for modals (Tier 1.6). When a modal opens we push a
  // history entry with ?modal=… so Back closes the modal instead of leaving
  // the page. On direct-load/shared link, closeOverlay replaces the URL.
  const { markPushed, closeOverlay } = useOverlayClose();

  const openYamlModal = useCallback((mode: 'import' | 'export') => {
    setYamlModalMode(mode);
    navigate(buildViewUrl('admin', adminPath, { modal: 'yaml' }));
    markPushed();
  }, [navigate, adminPath, markPushed]);

  const closeYamlModal = useCallback(() => {
    closeOverlay(buildViewUrl('admin', adminPath), navigate);
    setYamlModalMode(null);
  }, [closeOverlay, navigate, adminPath]);

  const openIamYamlModal = useCallback((mode: 'import' | 'export') => {
    setIamYamlMode(mode);
    navigate(buildViewUrl('admin', adminPath, { modal: 'iam' }));
    markPushed();
  }, [navigate, adminPath, markPushed]);

  const closeIamYamlModal = useCallback(() => {
    closeOverlay(buildViewUrl('admin', adminPath), navigate);
    setIamYamlMode(null);
  }, [closeOverlay, navigate, adminPath]);

  // Sync modal state with URL on Back/Forward (popstate). When the URL
  // no longer carries ?modal=…, close the corresponding modal.
  useEffect(() => {
    const q = parseAdminQuery(search ?? '');
    if (q.modal !== 'yaml' && yamlModalMode !== null) setYamlModalMode(null);
    if (q.modal !== 'iam' && iamYamlMode !== null) setIamYamlMode(null);
  }, [search, yamlModalMode, iamYamlMode]);

  // Global keyboard shortcuts (Wave 10 / 10.1 §10.3):
  //
  //   ⌘K / Ctrl+K — open the command palette (quick nav).
  //   ⌘S / Ctrl+S — Apply the current dirty section (if any). Does
  //                 NOT preventDefault when no dirty section handler
  //                 is registered, so the browser's native "save
  //                 page" fires normally on Diagnostics pages.
  //   ?           — open the shortcuts reference. Ignored when focus
  //                 is in an input / textarea / contenteditable so
  //                 the literal character still lands in text fields.
  //
  // Only active AFTER admin auth — no reason to hijack ⌘K on the
  // bootstrap login screen. Modifier match is strict (no shift / alt)
  // so we don't hijack ⌘⇧K (Chrome's "clear console") or ⌘⌥K.
  useEffect(() => {
    if (!authed) return;
    const onKey = (e: KeyboardEvent) => {
      const isBareCmdCtrl =
        (e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey;
      if (isBareCmdCtrl && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setPaletteOpen(true);
        return;
      }
      if (isBareCmdCtrl && e.key.toLowerCase() === 's') {
        // Dispatch to the currently-visible section's Apply handler.
        // If nothing is registered (e.g. Diagnostics pages, clean
        // Configuration pages), let the browser's default fire — we
        // don't want to silently eat ⌘S when there's no contextual
        // meaning.
        if (activeApplyKeys.length > 0 && requestApplyFirst(activeApplyKeys)) {
          e.preventDefault();
        }
        return;
      }
      // `?` (shortcuts help) is handled app-wide by App's global listener,
      // not here — avoids a double-open when both fire.
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [authed, activeApplyKeys]);

  // Memoised palette extra-actions. The underlying handlers
  // (`setYamlModalMode`, `onShowShortcuts`, `navigateAdmin`, `onBack`)
  // are stable, so the array only changes if those change. A fresh
  // array each render would invalidate the palette's useMemo chain
  // (commands → filtered) on every keystroke in the search input —
  // unnecessary work, especially on lower-powered devices.
  const paletteExtraActions = useMemo(
    () => [
      {
        id: 'action:show-yaml',
        label: 'Show YAML',
        hint: 'View current config as canonical YAML (secrets redacted)',
        keywords: 'show yaml view config copy',
        icon: <PaletteFileTextOutlined />,
        onRun: () => setYamlModalMode('export'),
      },
      {
        id: 'action:apply-yaml',
        label: 'Apply YAML',
        hint: 'Paste a YAML config document — validate, then apply',
        keywords: 'apply yaml upload config paste',
        icon: <PaletteImportOutlined />,
        onRun: () => setYamlModalMode('import'),
      },
      {
        id: 'action:setup-wizard',
        label: 'Setup wizard',
        hint: 'Walk through the 5-step onboarding for a fresh deployment',
        keywords: 'setup wizard onboarding first-run init',
        icon: <RocketOutlined />,
        onRun: () => navigateAdmin('setup'),
      },
      {
        id: 'action:shortcuts-help',
        label: 'Keyboard shortcuts',
        hint: 'Show the full list of admin UI shortcuts',
        keywords: 'help shortcuts keyboard bindings',
        icon: <QuestionCircleOutlined />,
        shortcut: '?',
        onRun: () => onShowShortcuts(),
      },
      {
        id: 'action:back-to-browser',
        label: 'Back to Browser',
        hint: 'Leave admin and return to the S3 object browser',
        keywords: 'back browser exit close admin',
        icon: <LogoutOutlined />,
        onRun: () => onBack(),
      },
    ],
    [navigateAdmin, onBack, onShowShortcuts]
  );

  // Check existing session on mount, or auto-login for IAM admins
  useEffect(() => {
    let cancelled = false;
    setCheckingSession(true);
    setAccessDenied(false);
    setS3BrowserSessionOnly(false);

    (async () => {
      const info = await whoami();
      if (cancelled) return;
      setExternalProviders(info.external_providers || []);

      // A failed probe (5xx, network) is not "no session": fall through to the
      // login gate, which still works, rather than hang on the spinner.
      const session = await checkSession().catch(() => ({ valid: false, admin_gui: false }));
      if (cancelled) return;
      if (session.valid) {
        if (session.admin_gui) {
          if (info.user?.is_admin) {
            setAuthed(true);
          } else {
            setAccessDenied(true);
          }
          setCheckingSession(false);
          return;
        }
        // Valid browser S3 session but no admin GUI cookie yet — still allow bootstrap / OAuth
        // login here (open-access and access-key connects both land here).
        setS3BrowserSessionOnly(true);
        setCheckingSession(false);
        return;
      }

      // In IAM mode, attempt auto-login with the current S3 credentials.
      // loginAs will succeed if the user is an IAM admin, or return 403 otherwise.
      if (info.mode === 'iam') {
        const creds = getCredentials();
        const ak = creds.accessKeyId;
        const sk = creds.secretAccessKey;
        if (ak && sk) {
          const result = await loginAs(ak, sk).catch(
            (e): LoginAsResult => ({ ok: false, status: 0, error: normalizeUiError(e, 'Network error') }),
          );
          if (cancelled) return;
          if (result.ok) {
            setAuthed(true);
          } else if (isNotAdminDenial(result)) {
            setAccessDenied(true);
          } else {
            // Rate limit / server error: not an answer about the user. Show it
            // on the login gate instead of claiming "not an admin".
            setLoginError(result.error);
          }
        }
      }

      if (cancelled) return;
      setCheckingSession(false);
    })();

    return () => { cancelled = true; };
  }, []);

  const navigateToGroup = useCallback(
    (groupId: number) => {
      setPendingGroupId(groupId);
      // Write ?group=<id> so the URL is the source of truth for the selection
      // (direct-load + Back/Forward). pendingGroupId stays as a fallback for
      // the rare case where the param is absent on mount.
      navigate(buildViewUrl('admin', 'access/groups', { group: String(groupId) }));
      setMobileNavOpen(false);
    },
    [navigate]
  );

  const routeCtx: AdminRouteContext = {
    onSessionExpired,
    onBack,
    navigateAdmin,
    navigateToGroup,
    pendingGroupId,
    clearPendingGroupId: () => setPendingGroupId(null),
    onExportBackup: backup.exportFullBackup,
    onImportBackup: backup.importFullBackup,
    search,
    proxyVersion,
  };

  // Access denied (IAM user without admin permissions)
  if (!authed && !checkingSession && accessDenied) {
    return <AdminAccessDenied onBack={onBack} />;
  }

  // Login gate (admin password + optional OAuth buttons)
  if (!authed && !checkingSession) {
    return (
      <AdminLoginGate
        externalProviders={externalProviders}
        s3BrowserSessionOnly={s3BrowserSessionOnly}
        initialError={loginError}
        onAuthed={() => setAuthed(true)}
        onBack={onBack}
      />
    );
  }

  if (checkingSession) {
    return (
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', flex: 1, background: colors.BG_BASE }}>
        <Spin size="large" />
      </div>
    );
  }

  const adminAccountMenu = canAdmin && isValidElement<AccountMenuConfigProps>(accountMenu)
    ? cloneElement(accountMenu, {
        configSection: activeSection,
        onShowFullConfigYaml: () => openYamlModal('export'),
        onImportFullConfigYaml: () => openYamlModal('import'),
        onExportFullIam: () => openIamYamlModal('export'),
        onImportFullIam: () => openIamYamlModal('import'),
      })
    : accountMenu;

  return (
    <div style={{
      display: 'flex',
      flexDirection: 'column',
      flex: 1,
      background: colors.BG_BASE,
    }}>
      <FullScreenHeader
        title="Admin Settings"
        onBack={onBack}
        onShowShortcuts={onShowShortcuts}
        leading={
          isNarrow ? (
            <Button
              size="small"
              type="text"
              icon={<MenuOutlined />}
              onClick={() => setMobileNavOpen(true)}
              aria-label="Open navigation"
              style={{ color: colors.TEXT_MUTED }}
            />
          ) : null
        }
        accountMenu={adminAccountMenu}
      />
      <YamlImportExportModal
        open={yamlModalMode !== null}
        mode={yamlModalMode ?? 'export'}
        onClose={closeYamlModal}
        onApplied={() => {
          // Soft refresh — reload the page so every panel re-fetches
          // from the updated /config endpoint. The alternative (piping
          // refresh signals to every tab's child component) is too
          // fragile for a surface this cross-cutting.
          window.location.reload();
        }}
      />
      <FullIamYamlModal
        open={iamYamlMode !== null}
        mode={iamYamlMode ?? 'export'}
        onClose={closeIamYamlModal}
        onApplied={() => {
          // Full IAM was reconciled — reload so every IAM-aware panel
          // (Users, Groups, Auth providers) re-fetches from the DB.
          window.location.reload();
        }}
      />
      <RestoreBackupModal
        file={backup.restoreFile}
        onCancel={backup.cancelRestore}
        onRestore={backup.runBackupImport}
      />

      {/* ⌘K command palette — fuzzy navigation over every admin page,
          plus a handful of shell-level quick actions (Export YAML,
          Import YAML, Setup wizard, Back to Browser). Mounts here so
          the extra actions can close over the same setters already
          wired into the header buttons (no prop drilling).

          Gated on `paletteOpen` so the ~20-line `useMemo` chain
          inside (navCommands/actionCommands/allCommands/rows/items)
          doesn't re-evaluate on every AdminPage render while the
          palette is closed. Tradeoff: we skip the AntD close-fade
          animation — on Esc the modal snaps shut. Acceptable,
          imperceptible in practice. */}
      {paletteOpen && (
        <CommandPalette
          open={paletteOpen}
          onClose={() => setPaletteOpen(false)}
          onNavigateAdmin={navigateAdmin}
          extraActions={paletteExtraActions}
        />
      )}

      {/* Body: sidebar + content (§3.1 four-group IA) */}
      <div style={{ flex: 1, display: 'flex', overflow: 'hidden' }}>
        {/* Mobile drawer (Wave 10.1 §10.4) — same sidebar contents,
            slide-in from the left. Only rendered below 900px. The
            persistent sidebar (next block) hides on narrow viewports. */}
        {isNarrow && (
          <Drawer
            title={null}
            placement="left"
            open={mobileNavOpen}
            onClose={() => setMobileNavOpen(false)}
            closable={false}
            size={260}
            styles={{
              body: { padding: 0, background: colors.BG_CARD },
              header: { display: 'none' },
            }}
          >
            <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
              <div style={{ flex: 1, minHeight: 0 }}>
                <AdminSidebar activePath={adminPath} onNavigate={navigateAdmin} />
              </div>
            </div>
          </Drawer>
        )}
        {/* Persistent sidebar. Hidden on narrow viewports (<900px) —
            replaced with the Drawer above. */}
        <div
          style={{
            display: isNarrow ? 'none' : 'flex',
            flexDirection: 'column',
            flexShrink: 0,
            borderRight: `1px solid ${colors.BORDER}`,
          }}
        >
          <div style={{ flex: 1, minHeight: 0 }}>
            <AdminSidebar activePath={adminPath} onNavigate={navigateAdmin} />
          </div>
        </div>

        {/* Content pane — single column, full width available.
            Config YAML actions now live in the avatar menu, so the
            header stays focused on navigation/account state while
            Configuration forms keep the space reclaimed from the old
            right rail. Apply/Discard for dirty state renders inline
            inside each section panel as an alert banner. */}
        <div
          style={{
            flex: 1,
            // min-width:0 lets the flex pane shrink below content's intrinsic
            // width — without it, wide rows force horizontal overflow on mobile.
            minWidth: 0,
            overflow: 'auto',
          }}
        >
          <AdminRouteContent path={adminPath} ctx={routeCtx} />
        </div>
      </div>
    </div>
  );
}

/**
 * Resolve which section a Configuration admin path edits — used by
 * the avatar menu's Config group to pick the section YAML target.
 * Returns undefined for Diagnostics pages and the first-run wizard
 * (no section scope).
 */
function sectionForPath(path: string): SectionName | undefined {
  // ORDER MATTERS: admission lives under access/ in the IA but maps to
  // its own YAML section — match it before the access/ prefix.
  if (path === 'access/admission') return 'admission';
  if (path.startsWith('access/')) return 'access';
  if (path.startsWith('storage/') || path === 'jobs') return 'storage';
  if (path === 'system' || path.startsWith('integrations/')) return 'advanced';
  return undefined;
}

/**
 * The dirty/apply key for the currently-active admin path — the leaf entry's
 * `dirtyKey` (its nav path for dirty-capable sub-panels, or the section name
 * for single-panel sections like Admission). ⌘S dispatches through this so it
 * reaches the visible panel's Apply handler, which registers under the SAME
 * per-leaf key (not the coarse `SectionName`). Returns undefined when the path
 * isn't a dirty-capable config leaf (Diagnostics, immediate-save CRUD, etc.).
 */
/// ⌘S dispatch candidates for a path: the single `applyKey` if set, else the
/// leaf's `dirtyKeys` (multi-editor pages like System register per-editor and
/// have no single applyKey — without this fallback ⌘S was dead there).
function applyKeysForPath(path: string): string[] {
  const entry = findEntry(ADMIN_IA, path);
  if (!entry) return [];
  if (entry.applyKey) return [entry.applyKey];
  return entry.dirtyKeys ?? [];
}
