/**
 * AdminSidebar — Wave 3 of the admin UI revamp.
 *
 * Replaces the flat 9-tab sidebar with the four-group IA from §3.1 of
 * the plan. Two groups (Diagnostics + Configuration) each hold their
 * own entries; sub-entries are nested under Access / Storage /
 * Advanced for their DB-backed collections and for the advanced
 * section's five sub-sections.
 *
 * ## URL scheme (§3.2)
 *
 * Every entry is a concrete URL the operator can bookmark + paste.
 * The URL always starts with `/_/admin/`:
 *
 *   /_/admin/diagnostics/dashboard
 *   /_/admin/diagnostics/trace
 *   /_/admin/configuration/admission
 *   /_/admin/configuration/access/credentials
 *   /_/admin/configuration/access/users
 *   /_/admin/configuration/access/groups
 *   /_/admin/configuration/access/ext-auth
 *   /_/admin/configuration/storage/backends
 *   /_/admin/configuration/storage/buckets
 *   /_/admin/configuration/storage/replication
 *   /_/admin/configuration/advanced/listener
 *   /_/admin/configuration/advanced/caches
 *   /_/admin/configuration/advanced/limits
 *   /_/admin/configuration/advanced/logging
 *   /_/admin/configuration/advanced/sync
 *
 * ## Dirty-state indicator (§5.2)
 *
 * Each Configuration entry whose underlying section is currently
 * dirty shows an amber dot next to the label. Subscribed to the
 * module-level dirty set via `subscribeToDirtyState`.
 *
 * ## Back-compat
 *
 * The old flat routes (`admin/users`, `admin/backends`, etc.) still
 * navigate through the same AdminPage — `resolveAdminPath` in
 * AdminPage.tsx maps them to the new sub-paths. Bookmarked old URLs
 * keep working.
 */
import { useEffect, useRef, useState } from 'react';
import {
  DashboardOutlined,
  ExperimentOutlined,
  FileTextOutlined,
  SecurityScanOutlined,
  TeamOutlined,
  SafetyOutlined,
  FolderOutlined,
  DatabaseOutlined,
  CloudServerOutlined,
  CloudOutlined,
  LockOutlined,
  SettingOutlined,
  SyncOutlined,
} from '@ant-design/icons';
import type { ReactNode } from 'react';
import { useColors } from '../ThemeContext';
import {
  subscribeToDirtyState,
  getDirtySections,
} from '../useDirtySection';
import type { SectionName } from '../adminApi';

interface SidebarEntry {
  /** Path sub-segment: `diagnostics/dashboard`, `configuration/admission/...` */
  path: string;
  /** Plain-English label shown in the nav. */
  label: string;
  /** Icon rendered to the left of the label. */
  icon: ReactNode;
  /**
   * Which configuration section this entry's content maps to — used
   * to drive the dirty-state amber dot. Diagnostics entries have no
   * section.
   */
  section?: SectionName;
  /** Child entries rendered below as a sub-nav. */
  children?: SidebarEntry[];
}

/**
 * The four-group IA. Exported so AdminPage can walk the tree to pick
 * the right header title + decide whether a given path is a group
 * (non-leaf) or a leaf page.
 */
export const ADMIN_IA: Array<{ group: string; entries: SidebarEntry[] }> = [
  {
    group: 'Diagnostics',
    entries: [
      {
        path: 'diagnostics/dashboard',
        label: 'Dashboard',
        icon: <DashboardOutlined />,
      },
      {
        path: 'diagnostics/trace',
        label: 'Trace',
        icon: <ExperimentOutlined />,
      },
      {
        // Wave 11: in-memory audit log viewer. Read-only; backs
        // onto the server-side ring buffer in src/audit.rs.
        path: 'diagnostics/audit',
        label: 'Audit log',
        icon: <FileTextOutlined />,
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
          },
          {
            path: 'configuration/access/users',
            label: 'Users',
            icon: <TeamOutlined />,
            section: 'access',
          },
          {
            path: 'configuration/access/groups',
            label: 'Groups',
            icon: <FolderOutlined />,
            section: 'access',
          },
          {
            path: 'configuration/access/ext-auth',
            label: 'External authentication',
            icon: <SafetyOutlined />,
            section: 'access',
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
          },
          {
            path: 'configuration/storage/buckets',
            label: 'Buckets',
            icon: <CloudOutlined />,
            section: 'storage',
          },
          {
            path: 'configuration/storage/replication',
            label: 'Object replication',
            icon: <SyncOutlined />,
            section: 'storage',
          },
          // Encryption-at-rest config — per-backend as of v0.9. Lives
          // inside the Backends panel (one subsection per backend
          // card); no longer a top-level sidebar entry.
        ],
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
          },
          {
            path: 'configuration/advanced/caches',
            label: 'Caches',
            icon: <DatabaseOutlined />,
            section: 'advanced',
          },
          {
            path: 'configuration/advanced/limits',
            label: 'Limits',
            icon: <CloudOutlined />,
            section: 'advanced',
          },
          {
            path: 'configuration/advanced/logging',
            label: 'Logging',
            icon: <DatabaseOutlined />,
            section: 'advanced',
          },
          {
            path: 'configuration/advanced/sync',
            label: 'Config DB sync',
            icon: <SyncOutlined />,
            section: 'advanced',
          },
        ],
      },
    ],
  },
];

interface Props {
  /** Current sub-path under `/_/admin/`. */
  activePath: string;
  /** Navigate callback — receives the new sub-path. */
  onNavigate: (path: string) => void;
}

export default function AdminSidebar({ activePath, onNavigate }: Props) {
  const { BG_CARD, BORDER, ACCENT_BLUE, TEXT_SECONDARY, TEXT_MUTED } = useColors();
  // Re-render whenever the dirty set changes (§5.2 amber dots).
  //
  // Structural dedupe: `getDirtySections()` returns a brand-new Set
  // every call, so a naive `setDirty(getDirtySections())` would
  // trigger a sidebar re-render on EVERY panel mount/unmount —
  // even when the membership hasn't changed. This matters because
  // the sidebar renders ~17 entries with icons + amber dots; a
  // re-render of it during navigation is visible in React
  // Profiler and scales poorly as the IA grows.
  //
  // We compare the sorted-section-list by value and only call
  // setDirty when it actually differs. A Set-of-≤4 elements
  // serialises cheaply.
  const [dirty, setDirty] = useState<Set<SectionName>>(getDirtySections);
  // Seed `last` from the SAME value `useState` captured (via `dirty`)
  // rather than a fresh post-mount read of `getDirtySections()`. If
  // the dirty set mutates between mount-time `useState` and post-
  // commit `useEffect`, those two snapshots diverge and we'd either
  // emit a spurious re-render or silently miss the first legitimate
  // update. (X-ray LOW #3.)
  const serialise = (s: Set<SectionName>) => Array.from(s).sort().join('|');
  const lastKeyRef = useRef<string>(serialise(dirty));
  useEffect(() => {
    const unsub = subscribeToDirtyState(() => {
      const next = getDirtySections();
      const key = serialise(next);
      if (key !== lastKeyRef.current) {
        lastKeyRef.current = key;
        setDirty(next);
      }
    });
    return unsub;
     
  }, []);

  const renderEntry = (entry: SidebarEntry, depth: number) => {
    const isActive =
      activePath === entry.path ||
      (entry.children && activePath.startsWith(entry.path + '/'));
    const isDirty = entry.section ? dirty.has(entry.section) : false;
    return (
      <div key={entry.path}>
        <button
          onClick={() => onNavigate(entry.path)}
          aria-current={isActive ? 'page' : undefined}
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 10,
            width: '100%',
            padding: `8px 20px 8px ${20 + depth * 16}px`,
            border: 'none',
            background: isActive ? ACCENT_BLUE + '18' : 'transparent',
            borderLeft: isActive
              ? `3px solid ${ACCENT_BLUE}`
              : '3px solid transparent',
            color: isActive ? ACCENT_BLUE : TEXT_SECONDARY,
            fontSize: 13,
            fontWeight: isActive ? 600 : 400,
            cursor: 'pointer',
            fontFamily: 'var(--font-ui)',
            textAlign: 'left',
            transition: 'all 0.15s ease',
          }}
        >
          <span style={{ display: 'inline-flex', width: 16, justifyContent: 'center' }}>
            {entry.icon}
          </span>
          <span style={{ flex: 1 }}>{entry.label}</span>
          {isDirty && (
            <span
              aria-label="Unsaved changes"
              title="This section has unsaved changes"
              style={{
                width: 8,
                height: 8,
                borderRadius: 4,
                background: '#d18616',
                flexShrink: 0,
              }}
            />
          )}
        </button>
        {entry.children && entry.children.length > 0 && (
          <div>{entry.children.map((c) => renderEntry(c, depth + 1))}</div>
        )}
      </div>
    );
  };

  return (
    <nav
      style={{
        width: 220,
        borderRight: `1px solid ${BORDER}`,
        background: BG_CARD,
        padding: '12px 0',
        flexShrink: 0,
        overflowY: 'auto',
      }}
      aria-label="Admin navigation"
    >
      {ADMIN_IA.map((group) => (
        <div key={group.group} style={{ marginBottom: 12 }}>
          <div
            style={{
              fontSize: 10,
              fontWeight: 600,
              letterSpacing: 0.5,
              textTransform: 'uppercase',
              color: TEXT_MUTED,
              padding: '0 20px 6px',
              fontFamily: 'var(--font-ui)',
            }}
          >
            {group.group}
          </div>
          <div>{group.entries.map((e) => renderEntry(e, 0))}</div>
        </div>
      ))}
    </nav>
  );
}
