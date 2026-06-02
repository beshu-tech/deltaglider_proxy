import {
  DashboardOutlined,
  ExperimentOutlined,
  FileTextOutlined,
  DownloadOutlined,
  SecurityScanOutlined,
  TeamOutlined,
  SafetyOutlined,
  FolderOutlined,
  DatabaseOutlined,
  CloudServerOutlined,
  CloudOutlined,
  ClockCircleOutlined,
  LockOutlined,
  SettingOutlined,
  ThunderboltOutlined,
  SyncOutlined,
  SendOutlined,
} from '@ant-design/icons';
import type { ReactNode } from 'react';
import type { SectionName } from '../adminApi';
import { findEntry, leavesUnder } from '../adminNavTree';

export interface SidebarEntry {
  /** Path sub-segment: `diagnostics/dashboard`, `configuration/admission/...` */
  path: string;
  /** Plain-English label shown in the nav. */
  label: string;
  /** Icon rendered to the left of the label. */
  icon: ReactNode;
  /**
   * One-sentence explanation of what this page does. Single source of
   * truth for the page `TabHeader` (via {@link headerForPath}) and the
   * parent-overview sub-section cards (via {@link childrenForPath}).
   * Leaf entries carry it; parent entries that render an overview
   * page don't need one.
   */
  description?: string;
  /**
   * Which configuration section this entry's content maps to — the coarse
   * server PUT target (`storage`, `advanced`, …). Diagnostics entries have none.
   * NOTE: this is NOT the dirty-dot key — many leaves share one `section` but
   * must light independently. See {@link dirtyKey}.
   */
  section?: SectionName;
  /**
   * The per-leaf dirty-state key (the panel's nav path), set ONLY on
   * dirty-capable leaves. The amber dot lights iff this key is dirty; a parent
   * rolls up its descendants. Leaves WITHOUT a `dirtyKey` (immediate-save CRUD
   * like Backends/Users) never light. This decoupling fixes the bug where one
   * dirty Storage sub-section lit every sibling. See `dirtyDotForEntry`.
   */
  dirtyKey?: string;
  /** Child entries rendered below as a sub-nav. */
  children?: SidebarEntry[];
}

/**
 * The four-group IA. Exported so AdminPage/CommandPalette can walk the
 * tree to pick the right header title + derive quick navigation.
 */
export const ADMIN_IA: Array<{ group: string; entries: SidebarEntry[] }> = [
  {
    group: 'Diagnostics',
    entries: [
      {
        path: 'diagnostics/dashboard',
        label: 'Dashboard',
        icon: <DashboardOutlined />,
        description:
          'Health, metrics, and admission-chain preview. Landing page for the admin UI.',
      },
      {
        path: 'diagnostics/trace',
        label: 'Trace',
        icon: <ExperimentOutlined />,
        description:
          'Evaluate a synthetic request against the current admission chain. See which block fires and why.',
      },
      {
        // Wave 11: in-memory audit log viewer. Read-only; backs
        // onto the server-side ring buffer in src/audit.rs.
        path: 'diagnostics/audit',
        label: 'Audit log',
        icon: <FileTextOutlined />,
        description:
          'Recent authentication + mutation events from this process (in-memory ring, default 500 entries). Stdout remains authoritative for long-term audit.',
      },
      {
        // v0.9.18: per-deltaspace efficiency report. Backs onto
        // the GET /_/api/admin/diagnostics/delta-efficiency
        // endpoint. Surfaces prefixes whose reference baseline
        // produces too-large deltas (the v0.9.17 1.70.0-pre5
        // incident shape).
        path: 'diagnostics/delta-efficiency',
        label: 'Delta efficiency',
        icon: <ThunderboltOutlined />,
        description:
          "Scan a bucket's deltaspaces and surface prefixes where the reference baseline is producing larger deltas than expected. Read-only diagnostic; you decide what to re-upload.",
      },
      {
        path: 'diagnostics/event-outbox',
        label: 'Event outbox',
        icon: <DatabaseOutlined />,
        description:
          'Durable object mutation events, delivery state, retry backoff, and failed webhook rows from the encrypted config DB.',
      },
    ],
  },
  {
    group: 'Configuration',
    entries: [
      {
        path: 'configuration/admission',
        label: 'Admission',
        icon: <SecurityScanOutlined />,
        section: 'admission',
        // Single-panel section: dirtyKey defaults to the section name in
        // useSectionEditor, so the nav key matches it ('admission').
        dirtyKey: 'admission',
        description:
          'Pre-auth request gating. Blocks are evaluated top to bottom; first match wins. Synthesized blocks from bucket public_prefixes fire after operator-authored ones.',
      },
      {
        path: 'configuration/access',
        label: 'Access',
        icon: <TeamOutlined />,
        section: 'access',
        children: [
          {
            path: 'configuration/access/credentials',
            label: 'Credentials & mode',
            icon: <LockOutlined />,
            section: 'access',
            dirtyKey: 'configuration/access/credentials',
            description:
              'IAM mode (GUI vs. declarative), authentication mode, legacy SigV4 bootstrap credentials, admin password.',
          },
          {
            path: 'configuration/access/users',
            label: 'Users',
            icon: <TeamOutlined />,
            section: 'access',
            description:
              'IAM users with fine-grained S3 permissions. In declarative IAM mode, this panel is read-only — edit your YAML instead.',
          },
          {
            path: 'configuration/access/groups',
            label: 'Groups',
            icon: <FolderOutlined />,
            section: 'access',
            description:
              'Organize users into groups with shared permission policies.',
          },
          {
            path: 'configuration/access/ext-auth',
            label: 'External authentication',
            icon: <SafetyOutlined />,
            section: 'access',
            description:
              'OAuth/OIDC providers and group mapping rules for SSO.',
          },
        ],
      },
      {
        path: 'configuration/storage',
        label: 'Storage',
        icon: <CloudServerOutlined />,
        section: 'storage',
        children: [
          {
            path: 'configuration/storage/backends',
            label: 'Backends',
            icon: <DatabaseOutlined />,
            section: 'storage',
            description:
              'Storage backends, default backend selection, connection tests, and encryption-at-rest.',
          },
          {
            path: 'configuration/storage/buckets',
            label: 'Buckets',
            icon: <CloudOutlined />,
            section: 'storage',
            dirtyKey: 'configuration/storage/buckets',
            description:
              'Per-bucket policies: compression overrides, delta ratio, public prefixes, quotas, aliases.',
          },
          {
            path: 'configuration/storage/replication',
            label: 'Object replication',
            icon: <SyncOutlined />,
            section: 'storage',
            dirtyKey: 'configuration/storage/replication',
            description:
              'Object data replication between buckets and prefixes. Rules are storage config; runtime state lives in the encrypted config DB.',
          },
          {
            path: 'configuration/storage/lifecycle',
            label: 'Object lifecycle',
            icon: <ClockCircleOutlined />,
            section: 'storage',
            dirtyKey: 'configuration/storage/lifecycle',
            description:
              'Delete-only object expiration rules with read-only preview, guarded run-now, and scheduler history.',
          },
          // Encryption-at-rest config — per-backend as of v0.9. Lives
          // inside the Backends panel (one subsection per backend
          // card); no longer a top-level sidebar entry.
        ],
      },
      {
        path: 'configuration/recovery',
        label: 'Backup',
        icon: <DownloadOutlined />,
        description:
          'Download a full backup bundle or restore one (IAM and control-plane state).',
      },
      {
        path: 'configuration/advanced',
        label: 'Advanced',
        icon: <SettingOutlined />,
        section: 'advanced',
        children: [
          {
            path: 'configuration/advanced/listener',
            label: 'Listener & TLS',
            icon: <CloudServerOutlined />,
            section: 'advanced',
            dirtyKey: 'configuration/advanced/listener',
            description: 'HTTP listen address, TLS cert and key paths.',
          },
          {
            path: 'configuration/advanced/caches',
            label: 'Caches',
            icon: <DatabaseOutlined />,
            section: 'advanced',
            dirtyKey: 'configuration/advanced/caches',
            description:
              'Reference cache, metadata cache, codec concurrency, blocking-thread pool size.',
          },
          {
            path: 'configuration/advanced/limits',
            label: 'Limits',
            icon: <CloudOutlined />,
            section: 'advanced',
            dirtyKey: 'configuration/advanced/limits',
            description:
              'Request timeouts, concurrency caps, multipart-upload limits. Most are env-var driven.',
          },
          {
            path: 'configuration/advanced/logging',
            label: 'Logging',
            icon: <DatabaseOutlined />,
            section: 'advanced',
            dirtyKey: 'configuration/advanced/logging',
            description:
              'tracing-subscriber EnvFilter string. Changes take effect immediately without restart.',
          },
          {
            path: 'configuration/advanced/sync',
            label: 'Config DB sync',
            icon: <SyncOutlined />,
            section: 'advanced',
            dirtyKey: 'configuration/advanced/sync',
            description:
              'S3 bucket for encrypted IAM/config database HA across proxy instances. This is not object replication.',
          },
          {
            path: 'configuration/advanced/event-delivery',
            label: 'Webhook delivery',
            icon: <SendOutlined />,
            section: 'advanced',
            dirtyKey: 'configuration/advanced/event-delivery',
            description:
              'Deliver durable object events to HTTP webhooks: endpoints, auth headers, retry backoff, retention. Applies live, no restart.',
          },
        ],
      },
    ],
  },
];

/**
 * Header metadata ({ icon, title, description }) for an admin page,
 * derived from the matching ADMIN_IA entry. Single source of truth —
 * the sidebar label doubles as the page title. Returns undefined for
 * paths with no entry (overview parents, the setup wizard).
 */
export function headerForPath(
  path: string
): { icon: ReactNode; title: string; description: string } | undefined {
  const entry = findEntry(ADMIN_IA, path);
  if (!entry || entry.description === undefined) return undefined;
  return { icon: entry.icon, title: entry.label, description: entry.description };
}

/** Leaf entries directly under a Configuration parent (Access/Storage/Advanced). */
export function childrenForPath(sectionPath: string): SidebarEntry[] {
  return leavesUnder(ADMIN_IA, sectionPath);
}
