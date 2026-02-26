import { useState } from 'react';
import { Button, Input, Space, Alert } from 'antd';
import { LockOutlined } from '@ant-design/icons';
import { changeAdminPassword } from '../adminApi';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';

export default function PasswordChangeCard() {
  const { cardStyle, inputRadius } = useCardStyles();

  const [currentPassword, setCurrentPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [changing, setChanging] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; error?: string } | null>(null);

  const handleSubmit = async () => {
    setChanging(true);
    setResult(null);
    const res = await changeAdminPassword(currentPassword, newPassword);
    setResult(res);
    if (res.ok) {
      setCurrentPassword('');
      setNewPassword('');
    }
    setChanging(false);
  };

  return (
    <form onSubmit={(e) => { e.preventDefault(); handleSubmit(); }} style={cardStyle}>
      <Space orientation="vertical" size="middle" style={{ width: '100%' }}>
        <SectionHeader icon={<LockOutlined />} title="Change Admin Password" />

        <input type="text" autoComplete="username" defaultValue="admin" aria-hidden="true" style={{ display: 'none' }} />
        <Input.Password
          placeholder="Current password"
          value={currentPassword}
          onChange={(e) => setCurrentPassword(e.target.value)}
          autoComplete="current-password"
          style={inputRadius}
        />
        <Input.Password
          placeholder="New password"
          value={newPassword}
          onChange={(e) => setNewPassword(e.target.value)}
          autoComplete="new-password"
          style={inputRadius}
        />

        {result && (
          <Alert
            type={result.ok ? 'success' : 'error'}
            message={result.ok ? 'Password changed successfully.' : (result.error || 'Failed')}
            showIcon
            style={{ borderRadius: 8 }}
          />
        )}

        <Button
          htmlType="submit"
          loading={changing}
          disabled={!currentPassword || !newPassword}
          block
          style={{ ...inputRadius, fontFamily: "var(--font-ui)", fontWeight: 600 }}
        >
          Change Password
        </Button>
      </Space>
    </form>
  );
}
