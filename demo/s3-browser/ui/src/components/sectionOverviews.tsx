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
import { TeamOutlined, CloudServerOutlined, SettingOutlined } from '@ant-design/icons';
import type { ExternalProviderInfo } from '../adminApi';
import { whoami, getUsers, getGroups } from '../adminApi';
import { useAdminConfig } from '../queries/config';
import SectionOverview from './SectionOverview';
import type { OverviewCard, OverviewStat } from './SectionOverview';
import { childrenForPath } from './adminNavigation';
import { Spin, Alert } from 'antd';

/**
 * Build the sub-section card list for a Configuration parent from the
 * ADMIN_IA tree (single source for title + icon + path), layering on
 * a section-specific contextual `blurb` and `extra` (summary +
 * declarative banner) keyed by path. The ordering follows ADMIN_IA.
 */
function cardsFromIA(
  sectionPath: string,
  blurbs: Record<string, string>,
  extra: (path: string) => Pick<OverviewCard, 'summary' | 'declarativeBanner'>
): OverviewCard[] {
  return childrenForPath(sectionPath).map((entry) => ({
    title: entry.label,
    icon: entry.icon,
    path: entry.path,
    blurb: blurbs[entry.path] ?? '',
    ...extra(entry.path),
  }));
}

interface OverviewProps {
  onNavigateAdmin: (path: string) => void;
  onSessionExpired?: () => void;
}

/**
 * Overview-config reader: thin wrapper over the shared `useAdminConfig`
 * react-query hook that preserves the prior `{ config, error }` contract
 * (string error, session-expired callback on a null/401 response).
 */
function useOverviewConfig(onSessionExpired?: () => void) {
  const { data, error: queryError, isError } = useAdminConfig();
  // getAdminConfig resolves to `null` on a non-OK (e.g. 401) response;
  // treat that as a session-expiry signal like the old effect did.
  useEffect(() => {
    if (data === null) onSessionExpired?.();
  }, [data, onSessionExpired]);
  const config = data ?? null;
  const error = isError
    ? queryError instanceof Error
      ? queryError.message
      : 'Failed to load config'
    : null;
  return { config, error };
}

// ═══════════════════════════════════════════════════
// Access — Credentials, Users, Groups, External Auth
// ═══════════════════════════════════════════════════

export function AccessOverview({ onNavigateAdmin, onSessionExpired }: OverviewProps) {
  const { config, error } = useOverviewConfig(onSessionExpired);
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

  const cards = cardsFromIA(
    'configuration/access',
    {
      'configuration/access/credentials':
        'Legacy SigV4 key pair, authentication mode selector, and the GUI ↔ Declarative IAM-mode toggle. Sets the context for everything else in Access.',
      'configuration/access/users':
        'IAM users with fine-grained S3 permissions via ABAC policies. Each user gets their own access key + secret for SigV4.',
      'configuration/access/groups':
        'Assemble users into groups with shared permission policies. Members inherit the union of their groups\' permissions.',
      'configuration/access/ext-auth':
        'OAuth / OIDC providers for SSO, plus mapping rules that translate external identity claims to IAM group memberships.',
    },
    (path) => {
      switch (path) {
        case 'configuration/access/credentials':
          return {
            summary: `access_key_id: ${config.access_key_id ? `${config.access_key_id.slice(0, 8)}...` : 'unset'}`,
          };
        case 'configuration/access/users':
          return {
            summary:
              userCount === null
                ? 'Loading...'
                : userCount === 0
                  ? 'No IAM users — bootstrap creds only'
                  : `${userCount} user${userCount === 1 ? '' : 's'}`,
            declarativeBanner: declarativeMode,
          };
        case 'configuration/access/groups':
          return {
            summary:
              groupCount === null
                ? 'Loading...'
                : `${groupCount} group${groupCount === 1 ? '' : 's'}`,
            declarativeBanner: declarativeMode,
          };
        case 'configuration/access/ext-auth':
          return {
            summary:
              providerCount === null
                ? 'Loading...'
                : `${providerCount} provider${providerCount === 1 ? '' : 's'}`,
            declarativeBanner: declarativeMode,
          };
        default:
          return {};
      }
    }
  );

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
  const { config, error } = useOverviewConfig(onSessionExpired);
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

  const cards = cardsFromIA(
    'configuration/storage',
    {
      'configuration/storage/backends':
        'Default storage backend (filesystem or S3-compatible), named backend targets, connection tests, and encryption-at-rest.',
      'configuration/storage/buckets':
        'Per-bucket overrides: compression toggle, delta-ratio threshold, public-read prefixes, quotas, and virtual-to-real name aliases.',
      'configuration/storage/replication':
        'One-way run-now object copy between buckets or prefixes. Rules go through the engine, so encryption and delta compression stay transparent.',
      'configuration/storage/lifecycle':
        'Delete-only expiration rules with read-only preview, guarded run-now, and scheduler history. Disabled by default.',
    },
    (path) => {
      switch (path) {
        case 'configuration/storage/backends':
          return {
            summary: `${backendType === 's3' ? 'S3' : 'Filesystem'} default · ${namedBackendCount} named`,
          };
        case 'configuration/storage/buckets':
          return {
            summary:
              buckets.length === 0
                ? 'No overrides'
                : `${buckets.length} bucket${buckets.length === 1 ? '' : 's'} with policies`,
          };
        case 'configuration/storage/replication':
          return { summary: 'Rules, run history, failures' };
        case 'configuration/storage/lifecycle':
          return { summary: 'Preview, delete, history' };
        default:
          return {};
      }
    }
  );

  return (
    <SectionOverview
      title="Storage"
      description="Where data physically lives, how buckets are treated, and how objects move between storage locations."
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
  const { config, error } = useOverviewConfig(onSessionExpired);
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

  const cards = cardsFromIA(
    'configuration/advanced',
    {
      'configuration/advanced/listener':
        'HTTP listen address, optional TLS cert / key paths, backend SigV4 credentials. Changes usually require a restart.',
      'configuration/advanced/caches':
        'Reference-baseline cache (delta reconstruction) and object-metadata cache. Larger caches trade memory for throughput.',
      'configuration/advanced/limits':
        'Request timeouts, concurrency caps, multipart-upload limits. Protects the proxy from overload and abuse. Env-var only.',
      'configuration/advanced/logging':
        'tracing-subscriber EnvFilter that drives every log line. Hot-reloadable — changes take effect on the next request.',
      'configuration/advanced/sync':
        'Replicate the encrypted IAM/config database to S3 so proxy instances share users, groups, and OAuth providers. Separate from object replication.',
    },
    (path) => {
      switch (path) {
        case 'configuration/advanced/listener':
          return { summary: `${config.listen_addr} · auth ${config.auth_enabled ? 'on' : 'off'}` };
        case 'configuration/advanced/caches':
          return {
            summary: `${config.cache_size_mb} MB reference · ${config.metadata_cache_mb} MB metadata`,
          };
        case 'configuration/advanced/limits':
          return {
            summary: `${config.max_concurrent_requests} concurrent · ${config.request_timeout_secs}s timeout`,
          };
        case 'configuration/advanced/logging':
          return { summary: logPresetLabel };
        case 'configuration/advanced/sync':
          return { summary: configSyncEnabled ? `Bucket: ${config.config_sync_bucket}` : 'Disabled' };
        default:
          return {};
      }
    }
  );

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
      <Spin description="Loading..." />
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
