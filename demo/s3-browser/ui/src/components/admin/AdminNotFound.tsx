import { Button, Result } from 'antd';
import { findEntry } from '../../adminNavTree';
import { ADMIN_IA } from '../adminNavigation';

/** An admin URL that names no page: say so, and offer the nearest real one. */
export default function AdminNotFound({ path, nearest, onNavigate }: {
  path: string;
  nearest: string;
  onNavigate: (path: string) => void;
}) {
  const label = findEntry(ADMIN_IA, nearest)?.label ?? 'Dashboard';
  return (
    <Result
      status="404"
      title="This settings page does not exist"
      subTitle={<>There is no page at <code>/_/admin/{path}</code>. The link may be old or mistyped.</>}
      extra={[
        <Button key="nearest" type="primary" onClick={() => onNavigate(nearest)}>
          Go to {label}
        </Button>,
        ...(nearest !== 'dashboard'
          ? [<Button key="dash" onClick={() => onNavigate('dashboard')}>Go to Dashboard</Button>]
          : []),
      ]}
    />
  );
}
