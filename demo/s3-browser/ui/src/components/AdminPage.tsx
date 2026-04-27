import { useState, useEffect, useCallback, useMemo } from 'react';
import { Typography, Button, Input, Alert, Space, Spin, Drawer, message, Dropdown } from 'antd';
import type { MenuProps } from 'antd';
import { checkSession, adminLogin, whoami, loginAs, exportBackup, importBackup, type ExternalProviderInfo } from '../adminApi';
import { getCredentials } from '../s3client';
import {
  CloudOutlined,
  CloudServerOutlined,
  DatabaseOutlined,
  TeamOutlined,
  FolderOutlined,
  LockOutlined,
  DashboardOutlined,
  DownloadOutlined,
  UploadOutlined,
  SafetyOutlined,
  ExperimentOutlined,
  SecurityScanOutlined,
  SettingOutlined,
  MenuOutlined,
  SyncOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import FullScreenHeader from './FullScreenHeader';
import UsersPanel from './UsersPanel';
import GroupsPanel from './GroupsPanel';
import AuthenticationPanel from './AuthenticationPanel';
import BackendsPanel from './BackendsPanel';
import MetricsPage from './MetricsPage';
import OAuthProviderList from './OAuthProviderList';
import AdminSidebar from './AdminSidebar';
import AdmissionPanel from './AdmissionPanel';
import CredentialsModePanel from './CredentialsModePanel';
import BucketsPanel from './BucketsPanel';
import ReplicationPanel from './ReplicationPanel';
import CopySectionYamlButton from './CopySectionYamlButton';
import SetupWizard from './SetupWizard';
import TracePanel from './TracePanel';
import AuditLogPanel from './AuditLogPanel';
import CommandPalette, {
  FileTextOutlined as PaletteFileTextOutlined,
  ImportOutlined as PaletteImportOutlined,
  RocketOutlined,
  LogoutOutlined,
  QuestionCircleOutlined,
} from './CommandPalette';
import ShortcutsHelp from './ShortcutsHelp';
import {
  ListenerTlsPanel,
  CachesPanel,
  LimitsPanel,
  LoggingPanel,
  ConfigDbSyncPanel,
} from './advancedPanels';
import {
  AccessOverview,
  StorageOverview,
  AdvancedOverview,
} from './sectionOverviews';
import { useNavigation } from '../NavigationContext';
import TabHeader from './TabHeader';
import { YamlImportExportModal } from './YamlImportExportModal';
import { DownOutlined, FileTextOutlined, ImportOutlined } from '@ant-design/icons';
import { useDirtyGlobalIndicators, requestApplyCurrent } from '../useDirtySection';
import type { SectionName } from '../adminApi';

const { Text } = Typography;

/**
 * Map legacy flat subPaths to the new 4-group IA subPaths (§3.1).
 *
 * Every bookmarkable URL before Wave 3 was `/_/admin/<tab>` where
 * `<tab>` is one of the TABS list below. We keep those URLs working —
 * operators may have pasted them in tickets / Slack — by normalising
 * to the new hierarchical form on read. The sidebar navigates using
 * the new form exclusively, so the legacy URLs only matter on the
 * first page load / refresh.
 */
const LEGACY_TO_NEW: Record<string, string> = {
  // Diagnostics — metrics keeps its own top-level route; dashboard
  // is a new page that lives only under the new scheme.
  'metrics': 'diagnostics/dashboard',
  // Access sub-sections
  'users': 'configuration/access/users',
  'groups': 'configuration/access/groups',
  'auth': 'configuration/access/ext-auth',
  // Storage sub-sections — legacy 'backends' covered backend infra
  // and bucket policy on one page. Today Backends is infra-only;
  // Buckets owns policy. Keep old bookmarks on Backends because
  // that's where the route originally landed.
  'backends': 'configuration/storage/backends',
  'backend': 'configuration/storage/backends',
  'compression': 'configuration/storage/backends',
  // Encryption moved per-backend in v0.9 — the dedicated panel is
  // gone; every backend card on the Backends page owns its own
  // encryption editor. Redirect bookmarks of the old URL.
  'encryption': 'configuration/storage/backends',
  // Advanced sub-sections
  'limits': 'configuration/advanced/limits',
  'security': 'configuration/advanced/listener',
  'logging': 'configuration/advanced/logging',
};

/**
 * Viewport-narrow detection hook (Wave 10.1 §10.4). Returns true
 * when the window is below `breakpoint` pixels wide — used to
 * swap the persistent sidebar for an AntD Drawer. Listens to
 * `resize` so toggling dev-tools / rotating the device is picked
 * up live without a full reload. 900px matches the plan's promise
 * ("sidebar collapses to drawer at <900px").
 */
function useIsNarrow(breakpoint: number = 900): boolean {
  const [narrow, setNarrow] = useState(() =>
    typeof window !== 'undefined' ? window.innerWidth < breakpoint : false
  );
  useEffect(() => {
    const onResize = () => setNarrow(window.innerWidth < breakpoint);
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, [breakpoint]);
  return narrow;
}

/**
 * Resolve an incoming `subPath` (anything the browser presents) to a
 * canonical path in the new 4-group scheme. Falls back to the default
 * landing page when the path is empty or unknown.
 */
function resolveAdminPath(subPath: string): string {
  const path = subPath.replace(/^\/+/, '').replace(/\/+$/, '');
  if (!path) return 'diagnostics/dashboard';
  // v0.9: the dedicated encryption page was deleted when encryption
  // moved per-backend. Redirect the full-depth bookmark to Backends.
  if (path === 'configuration/storage/encryption') {
    return 'configuration/storage/backends';
  }
  // Legacy flat paths (first segment only)
  const firstSegment = path.split('/')[0];
  if (LEGACY_TO_NEW[firstSegment]) {
    const remaining = path.slice(firstSegment.length);
    return LEGACY_TO_NEW[firstSegment] + remaining;
  }
  // Already a new-scheme path (including the standalone `setup` route).
  if (
    path.startsWith('diagnostics/') ||
    path.startsWith('configuration/') ||
    path === 'setup'
  ) {
    return path;
  }
  return 'diagnostics/dashboard';
}

/**
 * Header metadata for the current admin page, indexed by the new
 * canonical path. Used by `renderAdminContent` to render a
 * `TabHeader` above the page content.
 */
const PAGE_HEADERS: Record<string, { icon: React.ReactNode; title: string; description: string }> = {
  'diagnostics/dashboard': {
    icon: <DashboardOutlined />,
    title: 'Dashboard',
    description: 'Health, metrics, and admission-chain preview. Landing page for the admin UI.',
  },
  'diagnostics/trace': {
    icon: <ExperimentOutlined />,
    title: 'Admission trace',
    description: 'Evaluate a synthetic request against the current admission chain. See which block fires and why.',
  },
  'diagnostics/audit': {
    icon: <FileTextOutlined />,
    title: 'Audit log',
    description: 'Recent authentication + mutation events from this process (in-memory ring, default 500 entries). Stdout remains authoritative for long-term audit.',
  },
  'configuration/admission': {
    icon: <SecurityScanOutlined />,
    title: 'Admission',
    description: 'Pre-auth request gating. Blocks are evaluated top to bottom; first match wins. Synthesized blocks from bucket public_prefixes fire after operator-authored ones.',
  },
  'configuration/access/credentials': {
    icon: <LockOutlined />,
    title: 'Credentials & mode',
    description: 'IAM mode (GUI vs. declarative), authentication mode, legacy SigV4 bootstrap credentials, admin password.',
  },
  'configuration/access/users': {
    icon: <TeamOutlined />,
    title: 'Users',
    description: 'IAM users with fine-grained S3 permissions. In declarative IAM mode, this panel is read-only — edit your YAML instead.',
  },
  'configuration/access/groups': {
    icon: <FolderOutlined />,
    title: 'Groups',
    description: 'Organize users into groups with shared permission policies.',
  },
  'configuration/access/ext-auth': {
    icon: <SafetyOutlined />,
    title: 'External authentication',
    description: 'OAuth/OIDC providers and group mapping rules for SSO.',
  },
  'configuration/storage/backends': {
    icon: <CloudServerOutlined />,
    title: 'Backends',
    description: 'Storage backends, default backend selection, connection tests, and encryption-at-rest.',
  },
  'configuration/storage/buckets': {
    icon: <CloudOutlined />,
    title: 'Buckets',
    description: 'Per-bucket policies: compression overrides, delta ratio, public prefixes, quotas, aliases.',
  },
  'configuration/storage/replication': {
    icon: <SyncOutlined />,
    title: 'Object replication',
    description: 'Object data replication between buckets and prefixes. Rules are storage config; runtime state lives in the encrypted config DB.',
  },
  'configuration/advanced/listener': {
    icon: <CloudServerOutlined />,
    title: 'Listener & TLS',
    description: 'HTTP listen address, TLS cert and key paths.',
  },
  'configuration/advanced/caches': {
    icon: <DatabaseOutlined />,
    title: 'Caches',
    description: 'Reference cache, metadata cache, codec concurrency, blocking-thread pool size.',
  },
  'configuration/advanced/limits': {
    icon: <CloudOutlined />,
    title: 'Limits',
    description: 'Request timeouts, concurrency caps, multipart-upload limits. Most are env-var driven.',
  },
  'configuration/advanced/logging': {
    icon: <DatabaseOutlined />,
    title: 'Logging',
    description: 'tracing-subscriber EnvFilter string. Changes take effect immediately without restart.',
  },
  'configuration/advanced/sync': {
    icon: <SettingOutlined />,
    title: 'Config DB sync',
    description: 'S3 bucket for encrypted IAM/config database HA across proxy instances. This is not object replication.',
  },
};

interface AdminPageProps {
  onBack: () => void;
  onSessionExpired?: () => void;
  subPath?: string;
}

export default function AdminPage({ onBack, onSessionExpired, subPath }: AdminPageProps) {
  const colors = useColors();
  const { navigate } = useNavigation();
  // Hook up the `● ` tab-title prefix + beforeunload guard for any
  // section with unsaved edits. Mounting at AdminPage is the single
  // sensible home; moving higher would fire the guard on non-admin
  // pages, moving lower would miss the case where the operator
  // navigates away from a dirty section.
  useDirtyGlobalIndicators();

  // Command palette (⌘K / Ctrl+K) + Shortcuts help (?) — Wave 10
  // polish. Global keydown listener only mounts while AdminPage is
  // up so the shortcuts don't interfere with other views.
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);

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
  const navigateAdmin = useCallback(
    (path: string) => {
      navigate(`admin/${path}`);
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
      navigate(`admin/${adminPath}`, /* replace */ true);
    }
  }, [rawSubPath, adminPath, navigate]);

  const [authed, setAuthed] = useState(false);
  const [checkingSession, setCheckingSession] = useState(true);
  const [externalProviders, setExternalProviders] = useState<ExternalProviderInfo[]>([]);
  const [accessDenied, setAccessDenied] = useState(false);
  const [password, setPassword] = useState('');
  const [loginLoading, setLoginLoading] = useState(false);
  const [pendingGroupId, setPendingGroupId] = useState<number | null>(null);
  const [loginError, setLoginError] = useState('');
  // YAML import/export modal state. Mode flips between 'import'
  // (paste YAML → validate → apply) and 'export' (fetch current
  // canonical YAML → copy to clipboard).
  const [yamlModalMode, setYamlModalMode] = useState<'import' | 'export' | null>(null);

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
      const inText =
        e.target instanceof HTMLInputElement ||
        e.target instanceof HTMLTextAreaElement ||
        (e.target instanceof HTMLElement && e.target.isContentEditable);
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
        const section = sectionForPath(adminPath);
        if (section && requestApplyCurrent(section)) {
          e.preventDefault();
        }
        return;
      }
      if (e.key === '?' && !inText) {
        e.preventDefault();
        setHelpOpen(true);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [authed, adminPath]);

  // Memoised palette extra-actions. The underlying handlers
  // (`setYamlModalMode`, `setHelpOpen`, `navigateAdmin`, `onBack`)
  // are stable, so the only real dep is `navigateAdmin`. A fresh
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
        onRun: () => setHelpOpen(true),
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
    [navigateAdmin, onBack]
  );

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

      const info = await whoami();
      setExternalProviders(info.external_providers || []);

      // In IAM mode, attempt auto-login with the current S3 credentials.
      // loginAs will succeed if the user is an IAM admin, or return 403 otherwise.
      if (info.mode === 'iam') {
        const creds = getCredentials();
        const ak = creds.accessKeyId;
        const sk = creds.secretAccessKey;
        if (ak && sk) {
          const result = await loginAs(ak, sk);
          if (result.ok) {
            setAuthed(true);
          } else {
            setAccessDenied(true);
          }
        }
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

  const navigateToGroup = useCallback(
    (groupId: number) => {
      setPendingGroupId(groupId);
      navigateAdmin('configuration/access/groups');
    },
    [navigateAdmin]
  );

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
    const meta = PAGE_HEADERS[adminPath];
    const header = meta ? (
      <TabHeader icon={meta.icon} title={meta.title} description={meta.description} />
    ) : null;

    // First-run wizard (Wave 8). Reachable explicitly via
    // /_/admin/setup. Surface-wise it is its own full-page flow
    // with its own hero — no TabHeader needed.
    if (adminPath === 'setup') {
      return (
        <SetupWizard
          onComplete={() => navigateAdmin('diagnostics/dashboard')}
          onCancel={() => navigateAdmin('diagnostics/dashboard')}
        />
      );
    }

    // Diagnostics
    if (adminPath === 'diagnostics/dashboard') {
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

    // Configuration — Admission (Wave 4)
    if (adminPath === 'configuration/admission') {
      return (
        <>
          {header}
          <AdmissionPanel
            onSessionExpired={onSessionExpired}
            onNavigateToBucket={(_bucket) =>
              // Wave 6 will deep-link into a specific bucket editor;
              // until then we land on the Buckets sub-tab.
              navigateAdmin('configuration/storage/buckets')
            }
          />
        </>
      );
    }

    // Configuration — group parents: render the rich overview page
    // (hero + stat tiles + sub-section cards) instead of falling
    // through to a Dashboard. Admission has no sub-entries so it's
    // a leaf page handled above.
    if (adminPath === 'configuration/access') {
      return (
        <AccessOverview
          onNavigateAdmin={navigateAdmin}
          onSessionExpired={onSessionExpired}
        />
      );
    }
    if (adminPath === 'configuration/storage') {
      return (
        <StorageOverview
          onNavigateAdmin={navigateAdmin}
          onSessionExpired={onSessionExpired}
        />
      );
    }
    if (adminPath === 'configuration/advanced') {
      return (
        <AdvancedOverview
          onNavigateAdmin={navigateAdmin}
          onSessionExpired={onSessionExpired}
        />
      );
    }

    // Configuration — Access (Wave 5): dedicated Credentials & mode
    // panel. The IAM mode radio is the central decision; bootstrap
    // SigV4 credentials + admin password change are siblings. The
    // legacy SettingsPage `security` tab conflated Access +
    // rate-limit + session-TTL into one page; the latter two move
    // to Advanced (Wave 7).
    if (adminPath === 'configuration/access/credentials') {
      return (
        <>
          {header}
          <CredentialsModePanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'configuration/access/users') {
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
    if (adminPath === 'configuration/access/groups') {
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
    if (adminPath === 'configuration/access/ext-auth') {
      return (
        <>
          {header}
          <AuthenticationPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }

    // Configuration — Storage (Wave 6). Backends keeps the legacy
  // Backends owns storage infrastructure. Buckets owns per-bucket
  // policy. Object replication owns source → destination movement.
    if (adminPath === 'configuration/storage/backends') {
      return (
        <>
          {header}
          <BackendsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'configuration/storage/buckets') {
      return (
        <>
          {header}
          <BucketsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'configuration/storage/replication') {
      return (
        <>
          {header}
          <ReplicationPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    // Encryption config lives on each backend card in BackendsPanel
    // as of v0.9 — per-backend-scoped via `BackendEncryptionEditor`.
    // No top-level "encryption" route.

    // Configuration — Advanced (Wave 7). Five dedicated sub-panels,
    // each edits a different slice of `advanced.*` through the
    // section API (or for Limits, read-only env-var display).
    if (adminPath === 'configuration/advanced/listener') {
      return (
        <>
          {header}
          <ListenerTlsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'configuration/advanced/caches') {
      return (
        <>
          {header}
          <CachesPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'configuration/advanced/limits') {
      return (
        <>
          {header}
          <LimitsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'configuration/advanced/logging') {
      return (
        <>
          {header}
          <LoggingPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }
    if (adminPath === 'configuration/advanced/sync') {
      return (
        <>
          {header}
          <ConfigDbSyncPanel onSessionExpired={onSessionExpired} />
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

  const fullConfigItems: MenuProps['items'] = [
    {
      key: 'show-full-config-yaml',
      icon: <FileTextOutlined />,
      label: 'Show full config YAML',
      onClick: () => setYamlModalMode('export'),
    },
    {
      key: 'import-full-config-yaml',
      icon: <ImportOutlined />,
      label: 'Import full config YAML...',
      onClick: () => setYamlModalMode('import'),
    },
  ];

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
        extra={
          <Space size={4}>
            {/* Mobile hamburger — visible only below 900px where
                the persistent sidebar collapses to a drawer. */}
            {isNarrow && (
              <Button
                size="small"
                type="text"
                icon={<MenuOutlined />}
                onClick={() => setMobileNavOpen(true)}
                aria-label="Open navigation"
                style={{ color: colors.TEXT_MUTED }}
              />
            )}
            {/* Section-scoped Copy YAML — only renders on Configuration
                pages. Lives in the header now (not the old right-rail
                column) so Configuration pages reclaim the full content
                width for their forms. Responsive cascade intact; the
                button's label is hidden below 768px via the
                `hide-mobile` class. */}
            <CopySectionYamlButton section={sectionForPath(adminPath)} />
            <Dropdown menu={{ items: fullConfigItems }} trigger={['click']} placement="bottomRight">
              <Button
                size="small"
                type="text"
                icon={<FileTextOutlined />}
                title="Full-document YAML import/export. Section YAML is scoped to the current Configuration page."
                style={{ color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}
              >
                <span className="hide-mobile" style={{ marginLeft: 4 }}>Full config</span>
                <DownOutlined className="hide-mobile" style={{ marginLeft: 4, fontSize: 10 }} />
              </Button>
            </Dropdown>
          </Space>
        }
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
      {helpOpen && (
        <ShortcutsHelp open={helpOpen} onClose={() => setHelpOpen(false)} />
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
        {/* Four-group sidebar with a Full Backup footer.
            Since v0.8.4 the Full Backup button is the canonical
            "save everything" export: the server responds with a
            zip containing config.yaml + iam.json + secrets.json +
            manifest.json — the complete state needed to restore an
            instance. The pre-v0.8.4 label here was "IAM Backup",
            which was accurate but misleading (operators assumed it
            covered the whole proxy and restored to a broken state
            because storage/OAuth secrets weren't in scope). The
            label + tooltip + file extension all now signal "this
            is the whole thing".

            Hidden on narrow viewports (<900px) — replaced with the
            Drawer above. */}
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
          {/* Full Backup footer — zip of config.yaml + iam.json +
              secrets.json + manifest.json. Handles secrets round-
              trip atomically. */}
          <div
            style={{
              background: colors.BG_CARD,
              padding: '10px 12px 12px',
              borderTop: `1px solid ${colors.BORDER}`,
              width: 220,
            }}
          >
            <div
              style={{
                fontSize: 10,
                fontWeight: 600,
                letterSpacing: 0.5,
                textTransform: 'uppercase',
                color: colors.TEXT_MUTED,
                padding: '0 0 6px',
                fontFamily: 'var(--font-ui)',
              }}
              title="Zip containing config.yaml + iam.json + secrets.json + manifest.json. Everything needed to restore this proxy, including storage credentials, OAuth client_secrets, and the bootstrap password hash. Treat as a keystore — never commit to a public repo."
            >
              Full Backup
            </div>
            <div style={{ display: 'flex', gap: 4 }}>
              <Button
                size="small"
                icon={<DownloadOutlined />}
                onClick={async () => {
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
                  // Accept zip (new default) AND json (pre-v0.8.4
                  // IAM-only backups still round-trip via the
                  // content-type-sniffing import handler).
                  input.accept = '.zip,.json,application/zip,application/json';
                  input.onchange = async () => {
                    const file = input.files?.[0];
                    if (!file) return;
                    try {
                      // Zip path: pass the File Blob straight through
                      // — importBackup handles the content-type.
                      // JSON path: parse + pass object (legacy shape).
                      const isZip =
                        file.name.toLowerCase().endsWith('.zip') ||
                        file.type === 'application/zip' ||
                        file.type === 'application/x-zip-compressed';
                      const result = isZip
                        ? await importBackup(file)
                        : await importBackup(JSON.parse(await file.text()));
                      const ext = result.external_identities_created ?? 0;
                      message.success(
                        `Imported: ${result.users_created} users, ${result.groups_created} groups, ${ext} OIDC identities (${result.users_skipped} skipped)`
                      );
                      window.location.reload();
                    } catch (e) {
                      message.error(
                        'Import failed: ' + (e instanceof Error ? e.message : 'invalid file')
                      );
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
        </div>

        {/* Content pane — single column, full width available.
            Section-scoped Copy YAML lives in the shell header
            (CopySectionYamlButton), so Configuration forms reclaim
            the horizontal space the earlier right-rail was eating
            on viewports under ~1400px. Full-document Export/Import
            YAML also in the header. Apply/Discard for dirty state
            renders inline inside each section panel as an alert
            banner — that pattern predates the rail and survived
            the rewrite. */}
        <div
          style={{
            flex: 1,
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
 * the header's CopySectionYamlButton to pick the target section.
 * Returns undefined for Diagnostics pages and the first-run wizard
 * (no section scope).
 */
function sectionForPath(path: string): SectionName | undefined {
  if (path.startsWith('configuration/admission')) return 'admission';
  if (path.startsWith('configuration/access')) return 'access';
  if (path.startsWith('configuration/storage')) return 'storage';
  if (path.startsWith('configuration/advanced')) return 'advanced';
  return undefined;
}
