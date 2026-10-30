import { Button, Space } from 'antd';
import { CompassOutlined } from '@ant-design/icons';
import { findEntry } from '../../adminNavTree';
import { ADMIN_IA } from '../adminNavigation';
import { EmptyState } from '../StatePlaceholders';

/** An admin URL that names no page: say so, and offer the nearest real one. */
export default function AdminNotFound({ path, nearest, onNavigate }: {
  path: string;
  nearest: string;
  onNavigate: (path: string) => void;
}) {
  const label = findEntry(ADMIN_IA, nearest)?.label;
  const nearestIsDashboard = nearest === 'dashboard' || !label;
  return (
    <EmptyState
      icon={<CompassOutlined />}
      title="This settings page does not exist"
      hint={<>There is no page at <code>/_/admin/{path}</code>. The link may be old or mistyped.</>}
      action={
        <Space wrap>
          {!nearestIsDashboard && (
            <Button type="primary" onClick={() => onNavigate(nearest)}>
              Go to {label}
            </Button>
          )}
          {/* Same words as the setup wizard's exit. */}
          <Button type={nearestIsDashboard ? 'primary' : 'default'} onClick={() => onNavigate('dashboard')}>
            Back to the dashboard
          </Button>
        </Space>
      }
    />
  );
}
