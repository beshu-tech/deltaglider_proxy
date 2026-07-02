import { cloneElement, isValidElement, useState, useEffect, useCallback, useMemo } from 'react';
import { Typography, Button, Input, Alert, Space, Spin, Drawer, message, Modal } from 'antd';
import { checkSession, adminLogin, whoami, loginAs, exportBackup, importBackup, ImportBackupError, type ExternalProviderInfo, type ImportBackupMode } from '../adminApi';
import { getCredentials, initFromSession } from '../s3client';
import { LockOutlined, MenuOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { useIsNarrow } from '../useIsNarrow';
import FullScreenHeader from './FullScreenHeader';
import UsersPanel from './UsersPanel';
import GroupsPanel from './GroupsPanel';
import AuthenticationPanel from './AuthenticationPanel';
import BackendsPanel from './BackendsPanel';
import MetricsPage from './MetricsPage';
import OAuthProviderList from './OAuthProviderList';
import AdminSidebar from './AdminSidebar';
import { headerForPath, ADMIN_IA } from './adminNavigation';
import { findEntry } from '../adminNavTree';
import AdmissionPanel from './AdmissionPanel';
import SessionsPanel from './SessionsPanel';
import CredentialsModePanel from './CredentialsModePanel';
import BucketsPanel from './BucketsPanel';
import SetupWizard from './SetupWizard';
import TracePanel from './TracePanel';
import AuditLogPanel from './AuditLogPanel';
import LogsPanel from './LogsPanel';
import DeltaEfficiencyPanel from './DeltaEfficiencyPanel';
import EventOutboxPanel from './EventOutboxPanel';
import CommandPalette, {
  FileTextOutlined as PaletteFileTextOutlined,
  ImportOutlined as PaletteImportOutlined,
  RocketOutlined,
  LogoutOutlined,
  QuestionCircleOutlined,
} from './CommandPalette';
import JobsPanel from './jobs/JobsPanel';
import SystemPanel from './SystemPanel';
import WebhookDeliveryPanel from './WebhookDeliveryPanel';
import { useAdminConfig } from '../queries/config';
import { useNavigation } from '../NavigationContext';
import { resolveAdminPath as remapAdminPath } from '../adminPathRemap';
import { buildViewUrl } from '../urlState';
import TabHeader from './TabHeader';
import { YamlImportExportModal } from './YamlImportExportModal';
import { FullIamYamlModal } from './FullIamYamlModal';
import { useDirtyGlobalIndicators, requestApplyFirst } from '../useDirtySection';
import type { SectionName } from '../adminApi';
import type { AccountMenuConfigProps } from './AccountMenu';

const { Text } = Typography;


/**
 * Viewport-narrow detection hook (Wave 10.1 §10.4). Returns true
 * when the window is below `breakpoint` pixels wide — used to
 * swap the persistent sidebar for an AntD Drawer. Listens to
 * `resize` so toggling dev-tools / rotating the device is picked
 * up live without a full reload. 900px matches the plan's promise
 * ("sidebar collapses to drawer at <900px").
 */
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
  accountMenu?: React.ReactNode;
  canAdmin?: boolean;
  /** Open the app-wide keyboard-shortcuts modal (owned by App). */
  onShowShortcuts: () => void;
}

export default function AdminPage({ onBack, onSessionExpired, subPath, accountMenu, canAdmin = false, onShowShortcuts }: AdminPageProps) {
  const colors = useColors();
  const { navigate } = useNavigation();
  // Declarative IAM: the IAM-writing backup-restore modes (full / iam-only /
  // preserve-bootstrap) would 403 server-side, so the restore modal offers only
  // Config Only. Read the mode from the shared config query (cached).
  const { data: adminCfg } = useAdminConfig();
  const iamDeclarative = adminCfg?.iam_mode === 'declarative';
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
      navigate(buildViewUrl('admin', adminPath), { replace: true });
    }
  }, [rawSubPath, adminPath, navigate]);

  const [authed, setAuthed] = useState(false);
  const [checkingSession, setCheckingSession] = useState(true);
  const [externalProviders, setExternalProviders] = useState<ExternalProviderInfo[]>([]);
  const [accessDenied, setAccessDenied] = useState(false);
  /** Valid session from access-key / open connect — file browser only, not Settings sign-in. */
  const [s3BrowserSessionOnly, setS3BrowserSessionOnly] = useState(false);
  const [password, setPassword] = useState('');
  const [loginLoading, setLoginLoading] = useState(false);
  const [pendingGroupId, setPendingGroupId] = useState<number | null>(null);
  const [loginError, setLoginError] = useState('');
  // YAML import/export modal state. Mode flips between 'import'
  // (paste YAML → validate → apply) and 'export' (fetch current
  // canonical YAML → copy to clipboard).
  const [yamlModalMode, setYamlModalMode] = useState<'import' | 'export' | null>(null);
  const [iamYamlMode, setIamYamlMode] = useState<'import' | 'export' | null>(null);
  const [restoreFile, setRestoreFile] = useState<File | null>(null);

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

      const session = await checkSession();
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
          const result = await loginAs(ak, sk);
          if (cancelled) return;
          if (result.ok) {
            setAuthed(true);
          } else {
            setAccessDenied(true);
          }
        }
      }

      if (cancelled) return;
      setCheckingSession(false);
    })();

    return () => { cancelled = true; };
  }, []);

  const handleLogin = async () => {
    setLoginLoading(true);
    setLoginError('');
    try {
      const res = await adminLogin(password);
      if (res.ok) {
        setAuthed(true);
        setPassword('');
        // Bootstrap session may attach S3 creds (legacy keys or anonymous open-access).
        await initFromSession().catch(() => {});
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
      const session = await checkSession();
      if (!session.valid) {
        onSessionExpired?.();
      }
    }, 5 * 60 * 1000);
    return () => clearInterval(id);
  }, [authed, onSessionExpired]);

  const navigateToGroup = useCallback(
    (groupId: number) => {
      setPendingGroupId(groupId);
      navigateAdmin('access/groups');
    },
    [navigateAdmin]
  );

  const handleExportFullBackup = useCallback(async () => {
    try {
      const { blob, filename } = await exportBackup();
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = filename;
      a.click();
      URL.revokeObjectURL(url);
      message.success('Full backup exported');
    } catch (e) {
      message.error(
        'Export failed: ' + (e instanceof Error ? e.message : 'unknown')
      );
    }
  }, []);

  const runBackupImport = useCallback(async (file: File, mode: ImportBackupMode) => {
    try {
      const isZip =
        file.name.toLowerCase().endsWith('.zip') ||
        file.type === 'application/zip' ||
        file.type === 'application/x-zip-compressed';
      const result = isZip
        ? await importBackup(file, mode)
        : await importBackup(JSON.parse(await file.text()), 'iam-only');
      const ext = result.external_identities_created ?? 0;
      message.success(
        `Imported: ${result.users_created} users, ${result.groups_created} groups, ${ext} OIDC identities (${result.users_skipped} skipped)`
      );
      window.location.reload();
    } catch (e) {
      if (e instanceof ImportBackupError) {
        console.error('Full backup restore failed', {
          file: { name: file.name, type: file.type, size: file.size },
          status: e.status,
          response: e.response,
        });
        message.error(e.message, 8);
      } else {
        console.error('Full backup restore failed before request', e);
        message.error(
          'Import failed: ' + (e instanceof Error ? e.message : 'invalid file')
        );
      }
    } finally {
      setRestoreFile(null);
    }
  }, []);

  const handleImportFullBackup = useCallback(() => {
    const input = document.createElement('input');
    input.type = 'file';
    // Accept zip (new default) AND json (pre-v0.8.4 IAM-only backups
    // still round-trip via the content-type-sniffing import handler).
    input.accept = '.zip,.json,application/zip,application/json';
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      const isZip =
        file.name.toLowerCase().endsWith('.zip') ||
        file.type === 'application/zip' ||
        file.type === 'application/x-zip-compressed';
      if (isZip) {
        setRestoreFile(file);
      } else {
        runBackupImport(file, 'iam-only');
      }
    };
    input.click();
  }, [runBackupImport]);

  /**
   * Render the content pane for the current admin path.
   *
   * Wave 3's scope is the *sidebar* + *URL structure* — the content
   * pane still delegates to the existing panels (UsersPanel,
   * AuthenticationPanel, BackendsPanel, SettingsPage). Waves 4-7 will
   * replace these one at a time with section-editor components that
   * speak the section-level config API.
   *
   * Unknown paths fall through to the dashboard (diagnostics/
   * dashboard) rather than erroring — a fresh install or a dropped
   * URL segment should land somewhere sensible.
   */
  const renderContent = () => {
    const meta = headerForPath(adminPath);
    const header = meta ? (
      <TabHeader icon={meta.icon} title={meta.title} description={meta.description} saveModel={meta.saveModel} />
    ) : null;

    // First-run wizard (Wave 8). Reachable explicitly via
    // /_/admin/setup. Surface-wise it is its own full-page flow
    // with its own hero — no TabHeader needed.
    if (adminPath === 'setup') {
      return (
        <SetupWizard
          onComplete={() => navigateAdmin('dashboard')}
          onCancel={() => navigateAdmin('dashboard')}
        />
      );
    }

    // Diagnostics
    if (adminPath === 'dashboard') {
      // Skip the outer section header — MetricsPage's toolbar
      // already carries title + live indicator + tab switcher +
      // refresh controls. Rendering both would duplicate the
      // page-level identity and steal vertical real estate.
      return <MetricsPage onBack={onBack} embedded />;
    }
    if (adminPath === 'diagnostics/trace') {
      return (
        <>
          {header}
          <TracePanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'diagnostics/audit') {
      return (
        <>
          {header}
          <AuditLogPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'diagnostics/logs') {
      return (
        <>
          {header}
          <LogsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'diagnostics/delta-efficiency') {
      return (
        <>
          {header}
          <DeltaEfficiencyPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'integrations/event-outbox') {
      return (
        <>
          {header}
          <EventOutboxPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }

    // Configuration — Admission (Wave 4)
    if (adminPath === 'access/admission') {
      return (
        <>
          {header}
          <AdmissionPanel
            onSessionExpired={onSessionExpired}
            onNavigateToBucket={(_bucket) =>
              // Wave 6 will deep-link into a specific bucket editor;
              // until then we land on the Buckets sub-tab.
              navigateAdmin('storage/buckets')
            }
          />
        </>
      );
    }

    // Configuration — Access (Wave 5): dedicated Credentials & mode
    // panel. The IAM mode radio is the central decision; bootstrap
    // SigV4 credentials + admin password change are siblings. The
    // legacy SettingsPage `security` tab conflated Access +
    // rate-limit + session-TTL into one page; the latter two move
    // to Advanced (Wave 7).
    if (adminPath === 'access/credentials') {
      return (
        <>
          {header}
          <CredentialsModePanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'access/users') {
      return (
        <>
          {header}
          <UsersPanel
            onSessionExpired={onSessionExpired}
            onNavigateToGroup={navigateToGroup}
          />
        </>
      );
    }
    if (adminPath === 'access/groups') {
      return (
        <>
          {header}
          <GroupsPanel
            onSessionExpired={onSessionExpired}
            initialGroupId={pendingGroupId}
            onGroupSelected={() => setPendingGroupId(null)}
          />
        </>
      );
    }
    if (adminPath === 'access/external-auth') {
      return (
        <>
          {header}
          <AuthenticationPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'access/sessions') {
      return (
        <>
          {header}
          <SessionsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }

    // Configuration — Storage (Wave 6). Backends keeps the legacy
  // Backends owns storage infrastructure. Buckets owns per-bucket
  // policy. Object replication owns source → destination movement.
  // Object lifecycle owns delete-only expiration rules.
    if (adminPath === 'storage/backends') {
      return (
        <>
          {header}
          <BackendsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'storage/buckets') {
      return (
        <>
          {header}
          <BucketsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'jobs') {
      return (
        <>
          {header}
          <JobsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'system') {
      return (
        <>
          {header}
          <SystemPanel
            onSessionExpired={onSessionExpired}
            onExportBackup={handleExportFullBackup}
            onImportBackup={handleImportFullBackup}
          />
        </>
      );
    }
    if (adminPath === 'integrations/event-delivery') {
      return (
        <>
          {header}
          <WebhookDeliveryPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }

    // Unknown path — land on dashboard
    return (
      <>
        <MetricsPage onBack={onBack} embedded />
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
          {s3BrowserSessionOnly && (
            <Alert
              type="info"
              showIcon
              message="File browser session active"
              description="You are signed in for S3 browsing only. Use the bootstrap password (or OAuth if configured) below to open a full administrator session."
              style={{ marginBottom: 16, borderRadius: 8 }}
            />
          )}
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

  const adminAccountMenu = canAdmin && isValidElement<AccountMenuConfigProps>(accountMenu)
    ? cloneElement(accountMenu, {
        configSection: activeSection,
        onShowFullConfigYaml: () => setYamlModalMode('export'),
        onImportFullConfigYaml: () => setYamlModalMode('import'),
        onExportFullIam: () => setIamYamlMode('export'),
        onImportFullIam: () => setIamYamlMode('import'),
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
        extra={
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
        onClose={() => setYamlModalMode(null)}
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
        onClose={() => setIamYamlMode(null)}
        onApplied={() => {
          // Full IAM was reconciled — reload so every IAM-aware panel
          // (Users, Groups, Auth providers) re-fetches from the DB.
          window.location.reload();
        }}
      />
      <Modal
        title="Restore Backup"
        open={restoreFile !== null}
        onCancel={() => setRestoreFile(null)}
        footer={[
          <Button key="cancel" onClick={() => setRestoreFile(null)}>
            Cancel
          </Button>,
          <Button
            key="config"
            onClick={() => restoreFile && runBackupImport(restoreFile, 'config-only')}
          >
            Config Only
          </Button>,
          // The IAM-writing modes 403 in declarative mode (YAML owns IAM), so
          // only Config Only is offered there.
          ...(iamDeclarative ? [] : [
            <Button
              key="preserve-bootstrap"
              type="primary"
              onClick={() => restoreFile && runBackupImport(restoreFile, 'preserve-bootstrap')}
            >
              Everything Except Admin Password
            </Button>,
            <Button
              key="full"
              danger
              onClick={() => restoreFile && runBackupImport(restoreFile, 'full')}
            >
              Full Restore
            </Button>,
            <Button
              key="iam"
              onClick={() => restoreFile && runBackupImport(restoreFile, 'iam-only')}
            >
              IAM Only
            </Button>,
          ]),
        ]}
      >
        <Space direction="vertical" size={10}>
          <Text>
            Choose what to restore from <Text code>{restoreFile?.name}</Text>.
          </Text>
          {iamDeclarative && (
            <Alert
              type="warning"
              showIcon
              message="IAM is managed by YAML (declarative mode). Only Config Only is available — restore users, groups, and OIDC providers by editing access.iam_* in your YAML config and applying."
            />
          )}
          {!iamDeclarative && (
            <Alert
              type="info"
              showIcon
              message="Everything Except Admin Password restores config, backends, bucket policies, users, groups, OIDC providers, and secrets, while keeping this instance's local admin password."
            />
          )}
          {!iamDeclarative && (
            <Alert
              type="info"
              showIcon
              message="IAM Only skips config and backend changes; use it only when you want users/groups/OIDC without restoring storage settings."
            />
          )}
          {!iamDeclarative && (
            <Alert
              type="warning"
              showIcon
              message="Full Restore also tries to restore the backup's admin password and will fail if it doesn't match the admin password this instance was set up with."
            />
          )}
        </Space>
      </Modal>

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
            width={260}
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
          {renderContent()}
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
