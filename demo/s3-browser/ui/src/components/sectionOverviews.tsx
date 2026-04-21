/**
 * Parent-route overview pages for the three Configuration groups
 * that hold sub-entries (Access, Storage, Advanced).
 *
 * Each component is a thin data-loader that fetches the live config,
 * computes a handful of derived counters + a few sub-section
 * summaries, and hands the result to [`SectionOverview`] for
 * presentation.
 *
 * The Admission section is a leaf (no sub-entries), so it doesn't
 * need an overview — the `AdmissionPanel` itself handles that path.
 *
 * ## Data sources
 *
 *   * `getAdminConfig()` -> the AdminConfig wire shape
 *     (bucket_policies, iam_mode, listen_addr, admission_blocks, ...)
 *   * `getSection()` is NOT used here — the overviews just need the
 *     flat field-level view, which is lighter than a full section
 *     fetch and already carries everything we summarise.
 *   * `whoami()` is NOT used — auth mode lives on the AdminConfig
 *     response as `auth_enabled`.
 */
import { useEffect, useState } from 'react';
import {
  TeamOutlined,
  FolderOutlined,
  SafetyOutlined,
  LockOutlined,
  DatabaseOutlined,
  CloudOutlined,
  CloudServerOutlined,
  SettingOutlined,
  SyncOutlined,
} from '@ant-design/icons';
import type { AdminConfig, ExternalProviderInfo } from '../adminApi';
import { getAdminConfig, whoami, getUsers, getGroups } from '../adminApi';
import SectionOverview from './SectionOverview';
import type { OverviewCard, OverviewStat } from './SectionOverview';
import { Spin, Alert } from 'antd';

interface OverviewProps {
  onNavigateAdmin: (path: string) => void;
  onSessionExpired?: () => void;
}

function useAdminConfig(onSessionExpired?: () => void) {
  const [config, setConfig] = useState<AdminConfig | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const cfg = await getAdminConfig();
        if (cancelled) return;
        if (!cfg) {
          onSessionExpired?.();
          return;
        }
        setConfig(cfg);
      } catch (e) {
        if (cancelled) return;
        setError(e instanceof Error ? e.message : 'Failed to load config');
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [onSessionExpired]);
  return { config, error };
}

// ═══════════════════════════════════════════════════
// Access — Credentials, Users, Groups, External Auth
// ═══════════════════════════════════════════════════

export function AccessOverview({ onNavigateAdmin, onSessionExpired }: OverviewProps) {
  const { config, error } = useAdminConfig(onSessionExpired);
  // Users / groups / providers counts live outside AdminConfig —
  // fetch them in parallel. We tolerate errors here: if the IAM DB
  // is empty or the IAM mode is declarative and the lister returns
  // 403, we fall back to "—".
  const [userCount, setUserCount] = useState<number | null>(null);
  const [groupCount, setGroupCount] = useState<number | null>(null);
  const [providerCount, setProviderCount] = useState<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [users, groups, who] = await Promise.all([
          getUsers().catch(() => [] as unknown as Array<unknown>),
          getGroups().catch(() => [] as unknown as Array<unknown>),
          whoami().catch(() => null),
        ]);
        if (cancelled) return;
        setUserCount(Array.isArray(users) ? users.length : null);
        setGroupCount(Array.isArray(groups) ? groups.length : null);
        setProviderCount(
          who?.external_providers
            ? (who.external_providers as ExternalProviderInfo[]).length
            : 0
        );
      } catch {
        /* leave counters null */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  if (error) {
    return <Alert type="error" showIcon message="Failed to load" description={error} />;
  }
  if (!config) {
    return <LoadingShell />;
  }

  const iamMode = config.iam_mode === 'declarative' ? 'Declarative' : 'GUI';
  const declarativeMode = config.iam_mode === 'declarative';
  const authEnabled = config.auth_enabled;

  const stats: OverviewStat[] = [
    {
      label: 'IAM mode',
      value: iamMode,
      hint:
        config.iam_mode === 'declarative'
          ? 'YAML is the source of truth; admin mutations return 403.'
          : 'DB is the source of truth; YAML seeds on an empty DB.',
      tone: declarativeMode ? 'warning' : 'normal',
    },
    {
      label: 'S3 authentication',
      value: authEnabled ? 'Enabled' : 'Open access',
      hint: authEnabled
        ? 'All S3 requests require SigV4.'
        : 'Unauthenticated S3 requests accepted (dev-only).',
      tone: authEnabled ? 'normal' : 'warning',
    },
    {
      label: 'IAM users',
      value: userCount === null ? '—' : String(userCount),
      hint: userCount === 0 ? 'Bootstrap credentials only.' : 'With per-user permissions.',
    },
    {
      label: 'OAuth providers',
      value: providerCount === null ? '—' : String(providerCount),
      hint:
        providerCount === 0
          ? 'None configured (bootstrap password only).'
          : 'External SSO sign-in available.',
    },
  ];

  const cards: OverviewCard[] = [
    {
      title: 'Credentials & mode',
      blurb:
        'Legacy SigV4 key pair, authentication mode selector, and the GUI ↔ Declarative IAM-mode toggle. Sets the context for everything else in Access.',
      icon: <LockOutlined />,
      path: 'configuration/access/credentials',
      summary: `access_key_id: ${config.access_key_id ? `${config.access_key_id.slice(0, 8)}...` : 'unset'}`,
    },
    {
      title: 'Users',
      blurb:
        'IAM users with fine-grained S3 permissions via ABAC policies. Each user gets their own access key + secret for SigV4.',
      icon: <TeamOutlined />,
      path: 'configuration/access/users',
      summary:
        userCount === null
          ? 'Loading...'
          : userCount === 0
            ? 'No IAM users — bootstrap creds only'
            : `${userCount} user${userCount === 1 ? '' : 's'}`,
      declarativeBanner: declarativeMode,
    },
    {
      title: 'Groups',
      blurb:
        'Assemble users into groups with shared permission policies. Members inherit the union of their groups\' permissions.',
      icon: <FolderOutlined />,
      path: 'configuration/access/groups',
      summary:
        groupCount === null
          ? 'Loading...'
          : `${groupCount} group${groupCount === 1 ? '' : 's'}`,
      declarativeBanner: declarativeMode,
    },
    {
      title: 'External authentication',
      blurb:
        'OAuth / OIDC providers for SSO, plus mapping rules that translate external identity claims to IAM group memberships.',
      icon: <SafetyOutlined />,
      path: 'configuration/access/ext-auth',
      summary:
        providerCount === null
          ? 'Loading...'
          : `${providerCount} provider${providerCount === 1 ? '' : 's'}`,
      declarativeBanner: declarativeMode,
    },
  ];

  return (
    <SectionOverview
      title="Access"
      description="Who can authenticate to this proxy, and how. IAM users + groups, external-auth providers, the bootstrap SigV4 key, and the IAM-source-of-truth mode."
      icon={<TeamOutlined />}
      yamlPath="access.*"
      stats={stats}
      cards={cards}
      onNavigate={onNavigateAdmin}
    />
  );
}

// ═══════════════════════════════════════════════════
// Storage — Backends + Buckets
// ═══════════════════════════════════════════════════

export function StorageOverview({ onNavigateAdmin, onSessionExpired }: OverviewProps) {
  const { config, error } = useAdminConfig(onSessionExpired);
  if (error) {
    return <Alert type="error" showIcon message="Failed to load" description={error} />;
  }
  if (!config) {
    return <LoadingShell />;
  }

  const backendType = config.backend_type;
  const backendLabel =
    backendType === 's3'
      ? config.backend_endpoint || 'AWS S3'
      : config.backend_path || '—';

  const buckets = Object.entries(config.bucket_policies || {});
  const publicBuckets = buckets.filter(([, p]) => p.public || (p.public_prefixes && p.public_prefixes.length > 0));
  const quotaBuckets = buckets.filter(([, p]) => p.quota_bytes != null);

  const namedBackendCount = config.backends?.length || 0;

  const stats: OverviewStat[] = [
    {
      label: 'Default backend',
      value: backendType === 's3' ? 'S3' : 'Filesystem',
      hint: backendLabel,
    },
    {
      label: 'Named backends',
      value: String(namedBackendCount),
      hint:
        namedBackendCount === 0
          ? 'Default only.'
          : `${namedBackendCount} routing target${namedBackendCount === 1 ? '' : 's'}`,
      tone: namedBackendCount === 0 ? 'muted' : 'normal',
    },
    {
      label: 'Buckets',
      value: String(buckets.length),
      hint:
        buckets.length === 0
          ? 'No per-bucket policies.'
          : `${publicBuckets.length} public · ${quotaBuckets.length} with quota`,
    },
    {
      label: 'Compression default',
      value: (config.max_delta_ratio ?? 0) > 0 ? 'Enabled' : 'Disabled',
      hint: `max_delta_ratio: ${config.max_delta_ratio?.toFixed(2) ?? '—'}`,
    },
  ];

  const cards: OverviewCard[] = [
    {
      title: 'Backends',
      blurb:
        'Default storage backend (filesystem or S3-compatible) plus named backends for multi-target routing. Per-bucket policies can route individual buckets to a named backend.',
      icon: <DatabaseOutlined />,
      path: 'configuration/storage/backends',
      summary: `${backendType === 's3' ? 'S3' : 'Filesystem'} default · ${namedBackendCount} named`,
    },
    {
      title: 'Buckets',
      blurb:
        'Per-bucket overrides: compression toggle, delta-ratio threshold, public-read prefixes, quotas, and virtual-to-real name aliases.',
      icon: <CloudOutlined />,
      path: 'configuration/storage/buckets',
      summary:
        buckets.length === 0
          ? 'No overrides'
          : `${buckets.length} bucket${buckets.length === 1 ? '' : 's'} with policies`,
    },
  ];

  return (
    <SectionOverview
      title="Storage"
      description="Where data physically lives, and how this proxy treats it. Backend(s) + per-bucket overrides (compression, public read, quota, routing)."
      icon={<CloudServerOutlined />}
      yamlPath="storage.*"
      stats={stats}
      cards={cards}
      onNavigate={onNavigateAdmin}
    />
  );
}

// ═══════════════════════════════════════════════════
// Advanced — Listener, Caches, Limits, Logging, Sync
// ═══════════════════════════════════════════════════

export function AdvancedOverview({ onNavigateAdmin, onSessionExpired }: OverviewProps) {
  const { config, error } = useAdminConfig(onSessionExpired);
  if (error) {
    return <Alert type="error" showIcon message="Failed to load" description={error} />;
  }
  if (!config) {
    return <LoadingShell />;
  }

  const configSyncEnabled = !!config.config_sync_bucket;

  // Log level preset detection — keep in sync with SettingsPage's
  // LOG_LEVEL_PRESETS. Short-circuit to "Custom" if the current
  // log filter doesn't match any preset.
  const logPresetLabel = matchLogPreset(config.log_level);

  const stats: OverviewStat[] = [
    {
      label: 'Listen address',
      value: config.listen_addr,
      hint: 'Incoming S3 requests bind here.',
    },
    {
      label: 'Log level',
      value: logPresetLabel,
      hint: config.log_level,
    },
    {
      label: 'Reference cache',
      value: `${config.cache_size_mb} MB`,
      hint: 'LRU cache for delta-reconstruction baselines.',
      tone: config.cache_size_mb < 1024 ? 'warning' : 'normal',
    },
    {
      label: 'Config DB sync',
      value: configSyncEnabled ? 'Enabled' : 'Disabled',
      hint: configSyncEnabled
        ? `Synced via ${config.config_sync_bucket}`
        : 'IAM state stays local to this instance.',
      tone: configSyncEnabled ? 'normal' : 'muted',
    },
  ];

  const cards: OverviewCard[] = [
    {
      title: 'Listener & TLS',
      blurb:
        'HTTP listen address, optional TLS cert / key paths, backend SigV4 credentials. Changes usually require a restart.',
      icon: <CloudServerOutlined />,
      path: 'configuration/advanced/listener',
      summary: `${config.listen_addr} · auth ${config.auth_enabled ? 'on' : 'off'}`,
    },
    {
      title: 'Caches',
      blurb:
        'Reference-baseline cache (delta reconstruction) and object-metadata cache. Larger caches trade memory for throughput.',
      icon: <DatabaseOutlined />,
      path: 'configuration/advanced/caches',
      summary: `${config.cache_size_mb} MB reference · ${config.metadata_cache_mb} MB metadata`,
    },
    {
      title: 'Limits',
      blurb:
        'Request timeouts, concurrency caps, multipart-upload limits. Protects the proxy from overload and abuse. Env-var only.',
      icon: <CloudOutlined />,
      path: 'configuration/advanced/limits',
      summary: `${config.max_concurrent_requests} concurrent · ${config.request_timeout_secs}s timeout`,
    },
    {
      title: 'Logging',
      blurb:
        'tracing-subscriber EnvFilter that drives every log line. Hot-reloadable — changes take effect on the next request.',
      icon: <DatabaseOutlined />,
      path: 'configuration/advanced/logging',
      summary: logPresetLabel,
    },
    {
      title: 'Config DB sync',
      blurb:
        'Replicate the encrypted IAM database to an S3 bucket so multiple proxy instances share users / groups / OAuth providers.',
      icon: <SyncOutlined />,
      path: 'configuration/advanced/sync',
      summary: configSyncEnabled ? `Bucket: ${config.config_sync_bucket}` : 'Disabled',
    },
  ];

  return (
    <SectionOverview
      title="Advanced"
      description="Process-level tuning: listener + TLS, caches, limits, logging, and multi-instance IAM sync. Most fields require a restart; logging is the notable exception."
      icon={<SettingOutlined />}
      yamlPath="advanced.*"
      stats={stats}
      cards={cards}
      onNavigate={onNavigateAdmin}
    />
  );
}

// ─── helpers ─────────────────────────────────────────

function LoadingShell() {
  return (
    <div
      style={{
        padding: 48,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
      }}
    >
      <Spin tip="Loading..." />
    </div>
  );
}

const LOG_PRESETS: Array<[string, string]> = [
  ['deltaglider_proxy=error,tower_http=error', 'Error'],
  ['deltaglider_proxy=warn,tower_http=warn', 'Warn'],
  ['deltaglider_proxy=info,tower_http=info', 'Info'],
  ['deltaglider_proxy=debug,tower_http=debug', 'Debug'],
  ['deltaglider_proxy=trace,tower_http=trace', 'Trace'],
];

function matchLogPreset(filter: string): string {
  const canon = filter.split(',').map((s) => s.trim()).filter(Boolean).sort().join(',');
  for (const [preset, label] of LOG_PRESETS) {
    const presetCanon = preset.split(',').map((s) => s.trim()).sort().join(',');
    if (presetCanon === canon) return label;
  }
  return 'Custom';
}
