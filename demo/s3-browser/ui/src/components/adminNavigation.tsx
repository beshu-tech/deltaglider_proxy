import {
  ApiOutlined,
  CloudOutlined,
  CloudServerOutlined,
  DashboardOutlined,
  DatabaseOutlined,
  ExperimentOutlined,
  FileTextOutlined,
  LockOutlined,
  ProfileOutlined,
  SecurityScanOutlined,
  SendOutlined,
  SettingOutlined,
  SyncOutlined,
  TeamOutlined,
  ThunderboltOutlined,
  UserOutlined,
} from '@ant-design/icons';
import type { ReactNode } from 'react';
import type { SectionName } from '../adminApi';
import { findEntry } from '../adminNavTree';

export type SaveModel = 'immediate' | 'review';

export interface SidebarEntry {
  /** Path sub-segment: `dashboard`, `access/users`, `jobs`, `system`. */
  path: string;
  /** Plain-English label shown in the nav. */
  label: string;
  /** Icon rendered to the left of the label. */
  icon: ReactNode;
  /** One-sentence explanation — the page TabHeader's single source of truth. */
  description?: string;
  /**
   * Which configuration section this entry's content maps to — the coarse
   * server PUT target (`storage`, `advanced`, …). Diagnostics entries have none.
   */
  section?: SectionName;
  /**
   * Per-leaf dirty-state keys. A leaf may host SEVERAL independent editors
   * (Jobs: replication + lifecycle; System: one per card) — the amber dot
   * lights iff ANY of them is dirty. Leaves without keys (immediate-save
   * CRUD) never light.
   */
  dirtyKeys?: string[];
  /**
   * The ⌘S dispatch key — the `useApplyHandler` registration this leaf's
   * panel owns. Multi-editor leaves register ONE queue handler under this
   * key; absent = ⌘S falls through to the browser.
   */
  applyKey?: string;
  /** Save-model badge: 'immediate' = every click is live; 'review' = Apply flow. */
  saveModel?: SaveModel;
  /** Child entries rendered below as a sub-nav. */
  children?: SidebarEntry[];
}

/**
 * The 7-group / 15-leaf IA — one group per control domain (see
 * docs/plan/admin-ui-taxonomy.md): Overview, Diagnostics, Access,
 * Storage, Jobs, Integrations, System. No parent/overview pages: every
 * entry is a destination.
 */
export const ADMIN_IA: Array<{ group: string; entries: SidebarEntry[] }> = [
  {
    group: 'Overview',
    entries: [
      {
        path: 'dashboard',
        label: 'Dashboard',
        icon: <DashboardOutlined />,
        description: 'Health, metrics, and savings at a glance.',
      },
    ],
  },
  {
    group: 'Diagnostics',
    entries: [
      {
        path: 'diagnostics/trace',
        label: 'Trace',
        icon: <ExperimentOutlined />,
        description:
          'Replay a synthetic request against the admission chain and see which rule fires.',
      },
      {
        path: 'diagnostics/audit',
        label: 'Audit log',
        icon: <FileTextOutlined />,
        description: 'Recent authentication and mutation events from this process.',
      },
      {
        path: 'diagnostics/logs',
        label: 'Logs',
        icon: <ProfileOutlined />,
        description: 'Live tail + filter of the proxy operational logs (INFO+).',
      },
      {
        path: 'diagnostics/delta-efficiency',
        label: 'Delta efficiency',
        icon: <ThunderboltOutlined />,
        description: 'Find prefixes where the delta baseline is underperforming.',
      },
    ],
  },
  {
    group: 'Access',
    entries: [
      {
        path: 'access/credentials',
        label: 'Credentials & mode',
        icon: <LockOutlined />,
        section: 'access',
        dirtyKeys: ['access/credentials'],
        applyKey: 'access/credentials',
        saveModel: 'review',
        description: 'IAM mode, authentication mode, bootstrap credentials, admin password.',
      },
      {
        path: 'access/users',
        label: 'Users',
        icon: <UserOutlined />,
        section: 'access',
        saveModel: 'immediate',
        description: 'IAM users and their S3 permissions.',
      },
      {
        path: 'access/groups',
        label: 'Groups',
        icon: <TeamOutlined />,
        section: 'access',
        saveModel: 'immediate',
        description: 'Shared permission policies for sets of users.',
      },
      {
        path: 'access/external-auth',
        label: 'External authentication',
        icon: <ApiOutlined />,
        section: 'access',
        saveModel: 'immediate',
        description: 'OAuth/OIDC providers and group mapping for SSO.',
      },
      {
        path: 'access/admission',
        label: 'Admission rules',
        icon: <SecurityScanOutlined />,
        section: 'admission',
        dirtyKeys: ['admission'],
        applyKey: 'admission',
        saveModel: 'review',
        description: 'Pre-auth request gating. First matching rule wins.',
      },
    ],
  },
  {
    group: 'Storage',
    entries: [
      {
        path: 'storage/backends',
        label: 'Backends',
        icon: <DatabaseOutlined />,
        section: 'storage',
        saveModel: 'immediate',
        description: 'Storage backends, connection tests, encryption at rest.',
      },
      {
        path: 'storage/buckets',
        label: 'Buckets',
        icon: <CloudOutlined />,
        section: 'storage',
        dirtyKeys: ['storage/buckets'],
        applyKey: 'storage/buckets',
        saveModel: 'review',
        description: 'Per-bucket settings: routing, public access, quotas, compression.',
      },
    ],
  },
  {
    group: 'Jobs',
    entries: [
      {
        path: 'jobs',
        label: 'Jobs',
        icon: <SyncOutlined />,
        section: 'storage',
        dirtyKeys: ['jobs/replication', 'jobs/lifecycle'],
        applyKey: 'jobs',
        saveModel: 'review',
        description:
          'Everything that runs in the background: replication, lifecycle, re-encryption, migrations.',
      },
    ],
  },
  {
    group: 'Integrations',
    entries: [
      {
        path: 'integrations/event-delivery',
        label: 'Event delivery',
        icon: <SendOutlined />,
        section: 'advanced',
        dirtyKeys: ['integrations/event-delivery'],
        applyKey: 'integrations/event-delivery',
        saveModel: 'review',
        description: 'Send object events to webhooks and Slack.',
      },
      {
        path: 'integrations/event-outbox',
        label: 'Event outbox',
        icon: <CloudServerOutlined />,
        description: 'The durable queue behind event delivery: delivered, retrying, failed.',
      },
    ],
  },
  {
    group: 'System',
    entries: [
      {
        path: 'system',
        label: 'System',
        icon: <SettingOutlined />,
        section: 'advanced',
        dirtyKeys: ['system/listener', 'system/caches', 'system/logging', 'system/sync'],
        saveModel: 'review',
        description: 'Listener & TLS, caches and limits, logging, config DB sync, backup.',
      },
    ],
  },
];

/**
 * Header metadata for an admin page, derived from the matching ADMIN_IA
 * entry. Single source of truth — the sidebar label doubles as the page
 * title, and the save-model badge rides along. Returns undefined for
 * paths with no entry (the setup wizard).
 */
export function headerForPath(path: string):
  | { icon: ReactNode; title: string; description: string; saveModel?: SaveModel }
  | undefined {
  const entry = findEntry(ADMIN_IA, path);
  if (!entry || entry.description === undefined) return undefined;
  return {
    icon: entry.icon,
    title: entry.label,
    description: entry.description,
    saveModel: entry.saveModel,
  };
}
