/**
 * The admin content pane as a route table: one entry per admin path (every
 * `ADMIN_IA` leaf plus the `setup` wizard; scripts/admin-page-regression-test
 * keeps the two in sync). AdminPage resolves the path and renders
 * `<AdminRouteContent>`; a panel's wiring lives here, next to its siblings,
 * instead of in a chain of `if (adminPath === …)` blocks.
 */
import type { ReactNode } from 'react';
import MetricsPage from '../MetricsPage';
import SetupWizard from '../SetupWizard';
import TracePanel from '../TracePanel';
import AuditLogPanel from '../AuditLogPanel';
import LogsPanel from '../LogsPanel';
import DeltaEfficiencyPanel from '../DeltaEfficiencyPanel';
import EventOutboxPanel from '../EventOutboxPanel';
import AdmissionPanel from '../AdmissionPanel';
import CredentialsModePanel from '../CredentialsModePanel';
import UsersPanel from '../UsersPanel';
import GroupsPanel from '../GroupsPanel';
import AuthenticationPanel from '../AuthenticationPanel';
import SessionsPanel from '../SessionsPanel';
import BackendsPanel from '../BackendsPanel';
import BucketsPanel from '../BucketsPanel';
import JobsPanel from '../jobs/JobsPanel';
import SystemPanel from '../SystemPanel';
import WebhookDeliveryPanel from '../WebhookDeliveryPanel';
import TabHeader from '../TabHeader';
import { headerForPath } from '../adminNavigation';

/** Everything a routed admin page may need from AdminPage. */
export interface AdminRouteContext {
  onSessionExpired?: () => void;
  onBack: () => void;
  navigateAdmin: (path: string) => void;
  navigateToGroup: (groupId: number) => void;
  /** Group to preselect when arriving from a user's group chip. */
  pendingGroupId: number | null;
  clearPendingGroupId: () => void;
  onExportBackup: () => void;
  onImportBackup: () => void;
  search?: string;
  proxyVersion?: string;
}

interface AdminRoute {
  /** Render the page body. */
  render: (ctx: AdminRouteContext) => ReactNode;
  /** False for pages that carry their own title (the setup wizard's hero). */
  header?: false;
}

const ADMIN_ROUTES: Record<string, AdminRoute> = {
  // First-run wizard (Wave 8): its own full-page flow with its own hero.
  'setup': {
    header: false,
    render: (c) => (
      <SetupWizard
        onComplete={() => c.navigateAdmin('dashboard')}
        onCancel={() => c.navigateAdmin('dashboard')}
        search={c.search}
      />
    ),
  },
  // The shared page header carries the title, like every other admin page;
  // MetricsPage (embedded) drops its own title and keeps only its controls.
  'dashboard': {
    render: (c) => <MetricsPage onBack={c.onBack} embedded search={c.search} proxyVersion={c.proxyVersion} />,
  },
  'diagnostics/trace': {
    render: (c) => <TracePanel onSessionExpired={c.onSessionExpired} />,
  },
  'diagnostics/audit': {
    render: (c) => <AuditLogPanel onSessionExpired={c.onSessionExpired} />,
  },
  'diagnostics/logs': {
    render: (c) => <LogsPanel onSessionExpired={c.onSessionExpired} />,
  },
  'diagnostics/delta-efficiency': {
    render: (c) => <DeltaEfficiencyPanel onSessionExpired={c.onSessionExpired} />,
  },
  'integrations/event-outbox': {
    render: (c) => <EventOutboxPanel onSessionExpired={c.onSessionExpired} />,
  },
  'integrations/event-delivery': {
    render: (c) => <WebhookDeliveryPanel onSessionExpired={c.onSessionExpired} />,
  },
  'access/admission': {
    render: (c) => (
      <AdmissionPanel
        onSessionExpired={c.onSessionExpired}
        // No per-bucket deep link yet: land on the Buckets page.
        onNavigateToBucket={(_bucket) => c.navigateAdmin('storage/buckets')}
      />
    ),
  },
  // The IAM mode radio is the central decision; bootstrap SigV4 credentials
  // and the admin password change are its siblings.
  'access/credentials': {
    render: (c) => <CredentialsModePanel onSessionExpired={c.onSessionExpired} />,
  },
  'access/users': {
    render: (c) => (
      <UsersPanel onSessionExpired={c.onSessionExpired} onNavigateToGroup={c.navigateToGroup} search={c.search} />
    ),
  },
  'access/groups': {
    render: (c) => (
      <GroupsPanel
        onSessionExpired={c.onSessionExpired}
        initialGroupId={c.pendingGroupId}
        onGroupSelected={c.clearPendingGroupId}
        search={c.search}
      />
    ),
  },
  'access/external-auth': {
    render: (c) => <AuthenticationPanel onSessionExpired={c.onSessionExpired} />,
  },
  'access/sessions': {
    render: (c) => <SessionsPanel onSessionExpired={c.onSessionExpired} />,
  },
  // Backends own storage infrastructure; Buckets own per-bucket policy.
  'storage/backends': {
    render: (c) => <BackendsPanel onSessionExpired={c.onSessionExpired} />,
  },
  'storage/buckets': {
    render: (c) => <BucketsPanel onSessionExpired={c.onSessionExpired} />,
  },
  'jobs': {
    render: (c) => <JobsPanel onSessionExpired={c.onSessionExpired} search={c.search} />,
  },
  'system': {
    render: (c) => (
      <SystemPanel
        onSessionExpired={c.onSessionExpired}
        onExportBackup={c.onExportBackup}
        onImportBackup={c.onImportBackup}
      />
    ),
  },
};

/**
 * The content pane for an admin path. Unknown paths land on the dashboard
 * rather than erroring: a fresh install or a dropped URL segment should
 * arrive somewhere sensible.
 */
export function AdminRouteContent({ path, ctx }: { path: string; ctx: AdminRouteContext }) {
  const route = ADMIN_ROUTES[path] ?? ADMIN_ROUTES.dashboard;
  const meta = route.header === false ? undefined : headerForPath(path);
  return (
    <>
      {meta && (
        <TabHeader icon={meta.icon} title={meta.title} description={meta.description} />
      )}
      {route.render(ctx)}
    </>
  );
}
