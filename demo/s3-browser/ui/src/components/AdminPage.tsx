import { useState, useEffect, useCallback } from 'react';
import { Typography, Button, Input, Alert, Space, Spin, message } from 'antd';
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
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import FullScreenHeader from './FullScreenHeader';
import SettingsPage from './SettingsPage';
import UsersPanel from './UsersPanel';
import GroupsPanel from './GroupsPanel';
import AuthenticationPanel from './AuthenticationPanel';
import BackendsPanel from './BackendsPanel';
import MetricsPage from './MetricsPage';
import OAuthProviderList from './OAuthProviderList';
import AdminSidebar from './AdminSidebar';
import AdmissionPanel from './AdmissionPanel';
import CredentialsModePanel from './CredentialsModePanel';
import RightRailActions from './RightRailActions';
import {
  AccessOverview,
  StorageOverview,
  AdvancedOverview,
} from './sectionOverviews';
import { useNavigation } from '../NavigationContext';
import TabHeader from './TabHeader';
import { YamlImportExportModal } from './YamlImportExportModal';
import { FileTextOutlined, ImportOutlined } from '@ant-design/icons';
import { useDirtyGlobalIndicators } from '../useDirtySection';
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
  // Storage sub-sections — legacy 'backends' covered both backends +
  // bucket policies on one page; keep pointing there until Wave 6
  // splits them.
  'backends': 'configuration/storage/backends',
  'backend': 'configuration/storage/backends',
  'compression': 'configuration/storage/backends',
  // Advanced sub-sections
  'limits': 'configuration/advanced/limits',
  'security': 'configuration/advanced/listener',
  'logging': 'configuration/advanced/logging',
};

/**
 * Resolve an incoming `subPath` (anything the browser presents) to a
 * canonical path in the new 4-group scheme. Falls back to the default
 * landing page when the path is empty or unknown.
 */
function resolveAdminPath(subPath: string): string {
  const path = subPath.replace(/^\/+/, '').replace(/\/+$/, '');
  if (!path) return 'diagnostics/dashboard';
  // Legacy flat paths (first segment only)
  const firstSegment = path.split('/')[0];
  if (LEGACY_TO_NEW[firstSegment]) {
    const remaining = path.slice(firstSegment.length);
    return LEGACY_TO_NEW[firstSegment] + remaining;
  }
  // Already a new-scheme path
  if (path.startsWith('diagnostics/') || path.startsWith('configuration/')) {
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
    description: 'Storage backends, default backend selection, and per-bucket routing.',
  },
  'configuration/storage/buckets': {
    icon: <CloudOutlined />,
    title: 'Buckets',
    description: 'Per-bucket policies: compression overrides, delta ratio, public prefixes, quotas, aliases.',
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
    description: 'S3 bucket for encrypted IAM DB replication across proxy instances.',
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

  // Derive canonical admin path (§3.2). Legacy flat URLs (`users`,
  // `backends`, etc.) are mapped to the new hierarchy.
  const rawSubPath = (subPath || '').replace(/^\/+/, '').replace(/\/+$/, '');
  const adminPath = resolveAdminPath(subPath || '');
  const navigateAdmin = useCallback(
    (path: string) => {
      navigate(`admin/${path}`);
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

    // Diagnostics
    if (adminPath === 'diagnostics/dashboard') {
      // Wave 9 will replace the `Dashboard` page with a proper
      // metrics + admission preview surface. Until then we reuse the
      // existing MetricsPage which is what operators already landed
      // on when clicking the `metrics` tab.
      return (
        <>
          {header}
          <MetricsPage onBack={onBack} embedded />
        </>
      );
    }
    if (adminPath === 'diagnostics/trace') {
      return (
        <>
          {header}
          <Alert
            type="info"
            showIcon
            message="Admission trace editor — coming in Wave 9."
            description="For now, use POST /_/api/admin/config/trace from the terminal or curl."
          />
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

    // Configuration — Storage
    if (
      adminPath === 'configuration/storage/backends' ||
      adminPath === 'configuration/storage/buckets'
    ) {
      // Wave 6 splits these; today they share a panel.
      return (
        <>
          {header}
          <BackendsPanel onSessionExpired={onSessionExpired} />
        </>
      );
    }

    // Configuration — Advanced (5 sub-sections that all live inside
    // SettingsPage today — map each to the right embedded tab until
    // Wave 7 pulls them out into dedicated panels).
    const advancedMap: Record<string, string> = {
      'configuration/advanced/listener': 'security',
      'configuration/advanced/caches': 'backend',
      'configuration/advanced/limits': 'limits',
      'configuration/advanced/logging': 'logging',
      'configuration/advanced/sync': 'security',
    };
    const embeddedTab = advancedMap[adminPath];
    if (embeddedTab) {
      return (
        <>
          {header}
          <SettingsPage onSessionExpired={onSessionExpired} embeddedTab={embeddedTab} />
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
            <Button
              size="small"
              type="text"
              icon={<FileTextOutlined />}
              onClick={() => setYamlModalMode('export')}
              title="Copy the current config as canonical YAML (secrets redacted). Works with dgpctl apply on the other side."
              style={{ color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}
            >
              <span className="hide-mobile" style={{ marginLeft: 4 }}>Export YAML</span>
            </Button>
            <Button
              size="small"
              type="text"
              icon={<ImportOutlined />}
              onClick={() => setYamlModalMode('import')}
              title="Paste a YAML config document — validate, then apply + persist."
              style={{ color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}
            >
              <span className="hide-mobile" style={{ marginLeft: 4 }}>Import YAML</span>
            </Button>
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

      {/* Body: sidebar + content (§3.1 four-group IA) */}
      <div style={{ flex: 1, display: 'flex', overflow: 'hidden' }}>
        {/* Four-group sidebar (AdminSidebar) with an IAM Backup
            footer. Explicitly labelled "IAM Backup" — this is a
            JSON dump of IAM users/groups/OAuth-providers/mapping-
            rules, not the config YAML. Mislabelling it "Backup" in
            Wave 3 made operators mistake it for a YAML export and
            go looking for a third Export/Import channel. It is
            genuinely its own data domain — encrypted SQLCipher
            contents vs. the config YAML document — and deserves
            its own surface rather than being hidden. */}
        <div
          style={{
            display: 'flex',
            flexDirection: 'column',
            flexShrink: 0,
            borderRight: `1px solid ${colors.BORDER}`,
          }}
        >
          <div style={{ flex: 1, minHeight: 0 }}>
            <AdminSidebar activePath={adminPath} onNavigate={navigateAdmin} />
          </div>
          {/* IAM Backup footer — JSON dump of users/groups/OAuth, not YAML */}
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
              title="JSON dump of IAM state (users, groups, OAuth providers, mapping rules). Not the config YAML — that's in the header."
            >
              IAM Backup
            </div>
            <div style={{ display: 'flex', gap: 4 }}>
              <Button
                size="small"
                icon={<DownloadOutlined />}
                onClick={async () => {
                  try {
                    const data = await exportBackup();
                    const blob = new Blob([JSON.stringify(data, null, 2)], {
                      type: 'application/json',
                    });
                    const url = URL.createObjectURL(blob);
                    const a = document.createElement('a');
                    a.href = url;
                    a.download = `dgp-iam-backup-${new Date().toISOString().slice(0, 10)}.json`;
                    a.click();
                    URL.revokeObjectURL(url);
                    message.success('IAM backup exported');
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
                  input.accept = '.json';
                  input.onchange = async () => {
                    const file = input.files?.[0];
                    if (!file) return;
                    try {
                      const text = await file.text();
                      const data = JSON.parse(text);
                      const result = await importBackup(data);
                      message.success(
                        `Imported: ${result.users_created} users, ${result.groups_created} groups (${result.users_skipped} skipped)`
                      );
                      window.location.reload();
                    } catch (e) {
                      message.error(
                        'Import failed: ' + (e instanceof Error ? e.message : 'invalid JSON')
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

        {/* Content + right-rail actions (§3.3).
            UX architecture: the rail speaks for the CURRENT section
            only (section YAML copy/paste + Apply/Discard). Full-
            document YAML I/O lives in the shell header (Export YAML
            / Import YAML at the top). This gives exactly one place
            per scope — no duplicates. Diagnostics pages have no
            section and therefore no rail. */}
        <div
          style={{
            flex: 1,
            overflow: 'auto',
            display: 'flex',
            gap: 16,
            padding: 16,
          }}
        >
          <div style={{ flex: 1, minWidth: 0 }}>{renderContent()}</div>
          <RightRailActions section={sectionForPath(adminPath)} />
        </div>
      </div>
    </div>
  );
}

/**
 * Resolve which section a Configuration admin path edits — used by
 * RightRailActions to pick the target for Copy YAML. Returns
 * undefined for Diagnostics pages (no section scope).
 */
function sectionForPath(path: string): SectionName | undefined {
  if (path.startsWith('configuration/admission')) return 'admission';
  if (path.startsWith('configuration/access')) return 'access';
  if (path.startsWith('configuration/storage')) return 'storage';
  if (path.startsWith('configuration/advanced')) return 'advanced';
  return undefined;
}
