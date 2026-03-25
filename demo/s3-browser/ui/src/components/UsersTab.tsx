import { useState, useEffect, useCallback } from 'react';
import { Table, Button, Tag, Alert, Space, Popconfirm, Typography, Spin, message } from 'antd';
import { PlusOutlined, ReloadOutlined } from '@ant-design/icons';
import type { ColumnsType } from 'antd/es/table';
import type { IamUser } from '../adminApi';
import { getUsers, deleteUser } from '../adminApi';
import { useCardStyles } from './shared-styles';
import UserModal from './UserModal';

const { Text } = Typography;

interface UsersTabProps {
  onSessionExpired?: () => void;
}

function permissionSummary(user: IamUser): string {
  if (user.permissions.length === 0) return 'No access';
  const hasWildcardAction = user.permissions.some(p => p.actions.includes('*'));
  const hasWildcardResource = user.permissions.some(p => p.resources.includes('*'));
  if (hasWildcardAction && hasWildcardResource) return 'Full admin';
  return `${user.permissions.length} rule${user.permissions.length !== 1 ? 's' : ''}`;
}

export default function UsersTab({ onSessionExpired }: UsersTabProps) {
  const { cardStyle, inputRadius } = useCardStyles();
  const [users, setUsers] = useState<IamUser[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [modalOpen, setModalOpen] = useState(false);
  const [editingUser, setEditingUser] = useState<IamUser | null>(null);

  const loadUsers = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const data = await getUsers();
      setUsers(data);
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'Failed to load users';
      if (msg.includes('401')) onSessionExpired?.();
      else setError(msg);
    } finally {
      setLoading(false);
    }
  }, [onSessionExpired]);

  useEffect(() => { loadUsers(); }, [loadUsers]);

  const handleDelete = async (user: IamUser) => {
    try {
      await deleteUser(user.id);
      message.success(`User "${user.name}" deleted`);
      loadUsers();
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Delete failed');
    }
  };

  const handleCreate = () => {
    setEditingUser(null);
    setModalOpen(true);
  };

  const handleEdit = (user: IamUser) => {
    setEditingUser(user);
    setModalOpen(true);
  };

  const colHeader = (text: string) => (
    <span style={{ fontSize: 11, fontWeight: 600, letterSpacing: 0.5, whiteSpace: 'nowrap' }}>{text}</span>
  );

  const columns: ColumnsType<IamUser> = [
    {
      title: colHeader('Name'),
      dataIndex: 'name',
      key: 'name',
      width: 120,
      ellipsis: true,
      render: (text: string) => <Text strong>{text}</Text>,
    },
    {
      title: colHeader('Access Key'),
      dataIndex: 'access_key_id',
      key: 'access_key_id',
      width: 150,
      render: (text: string) => (
        <Text code copyable={{ tooltips: false }} style={{ fontFamily: 'var(--font-mono)', fontSize: 11 }}>
          {text.substring(0, 6)}...{text.substring(text.length - 4)}
        </Text>
      ),
    },
    {
      title: colHeader('Status'),
      dataIndex: 'enabled',
      key: 'enabled',
      width: 75,
      render: (enabled: boolean) => (
        <Tag color={enabled ? 'green' : 'default'} style={{ margin: 0 }}>{enabled ? 'Active' : 'Off'}</Tag>
      ),
    },
    {
      title: colHeader('Perms'),
      key: 'permissions',
      width: 80,
      render: (_: unknown, record: IamUser) => {
        const summary = permissionSummary(record);
        const color = summary === 'Full admin' ? 'blue' : summary === 'No access' ? 'default' : 'cyan';
        return <Tag color={color} style={{ margin: 0 }}>{summary}</Tag>;
      },
    },
    {
      title: colHeader(''),
      key: 'actions',
      width: 100,
      render: (_: unknown, record: IamUser) => (
        <Space size={0}>
          <Button type="link" size="small" onClick={() => handleEdit(record)}>Edit</Button>
          <Popconfirm
            title={`Delete "${record.name}"?`}
            onConfirm={() => handleDelete(record)}
            okText="Delete"
            okButtonProps={{ danger: true }}
          >
            <Button type="link" danger size="small">Del</Button>
          </Popconfirm>
        </Space>
      ),
    },
  ];

  const tabPane: React.CSSProperties = { minHeight: 420, minWidth: 0 };

  return (
    <div style={tabPane}>
      <form onSubmit={e => e.preventDefault()}>
        <div style={{ ...cardStyle, marginBottom: 0 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 16 }}>
            <Text strong style={{ fontSize: 15 }}>Users & Access Control</Text>
            <Space>
              <Button icon={<ReloadOutlined />} onClick={loadUsers} loading={loading} style={inputRadius} />
              <Button type="primary" icon={<PlusOutlined />} onClick={handleCreate} style={inputRadius}>
                Create User
              </Button>
            </Space>
          </div>

          {error && (
            <Alert
              type="error"
              message={error}
              showIcon
              closable
              onClose={() => setError('')}
              style={{ marginBottom: 16, borderRadius: 8 }}
            />
          )}

          {loading && users.length === 0 ? (
            <div style={{ textAlign: 'center', padding: 48 }}><Spin /></div>
          ) : users.length === 0 ? (
            <Alert
              type="info"
              message="No IAM users configured"
              description="The proxy is running in legacy mode (single credential) or open access. Create a user to enable multi-user IAM."
              showIcon
              style={{ borderRadius: 8 }}
            />
          ) : (
            <Table
              columns={columns}
              dataSource={users}
              rowKey="id"
              pagination={false}
              size="small"
              scroll={{ x: 560 }}
              style={{ borderRadius: 8 }}
              locale={{ emptyText: 'No users' }}
            />
          )}
        </div>
      </form>

      <UserModal
        open={modalOpen}
        user={editingUser}
        onClose={() => setModalOpen(false)}
        onSaved={loadUsers}
        onSessionExpired={onSessionExpired}
      />
    </div>
  );
}
