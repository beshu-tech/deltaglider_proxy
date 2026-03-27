import { useState } from 'react';
import { Button, Input, Space, Alert, Typography } from 'antd';
import { LockOutlined, WarningOutlined } from '@ant-design/icons';
import { changeAdminPassword } from '../adminApi';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

export default function PasswordChangeCard() {
  const { cardStyle, inputRadius } = useCardStyles();
  const { TEXT_MUTED, ACCENT_AMBER } = useColors();

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
      <Space direction="vertical" size="middle" style={{ width: '100%' }}>
        <SectionHeader icon={<LockOutlined />} title="Change Bootstrap Password" />

        <div style={{ fontSize: 13, color: TEXT_MUTED, lineHeight: 1.6 }}>
          <Text style={{ color: TEXT_MUTED, fontSize: 13 }}>
            The bootstrap password is a single infrastructure secret that serves three purposes:
          </Text>
          <ul style={{ margin: '8px 0', paddingLeft: 20 }}>
            <li><strong>Encrypts the IAM database</strong> — user credentials are stored in an encrypted SQLCipher database, keyed by this password.</li>
            <li><strong>Signs admin session cookies</strong> — authenticates your browser session for this settings panel.</li>
            <li><strong>Gates admin access</strong> — before IAM users exist, this password is required to access settings.</li>
          </ul>
        </div>

        <Alert
          type="warning"
          icon={<WarningOutlined />}
          showIcon
          message="Changing this password re-encrypts the IAM database"
          description="All active admin sessions will be invalidated. IAM users and their credentials are preserved. If you forget this password, use the CLI flag --set-bootstrap-password to reset it (warning: this wipes the IAM database)."
          style={{ borderRadius: 8 }}
        />

        <input type="text" autoComplete="username" defaultValue="admin" aria-hidden="true" style={{ display: 'none' }} />
        <Input.Password
          placeholder="Current bootstrap password"
          value={currentPassword}
          onChange={(e) => setCurrentPassword(e.target.value)}
          autoComplete="current-password"
          style={inputRadius}
        />
        <Input.Password
          placeholder="New bootstrap password"
          value={newPassword}
          onChange={(e) => setNewPassword(e.target.value)}
          autoComplete="new-password"
          style={inputRadius}
        />

        {result && (
          <Alert
            type={result.ok ? 'success' : 'error'}
            message={result.ok ? 'Bootstrap password changed. All sessions invalidated.' : (result.error || 'Failed')}
            showIcon
            style={{ borderRadius: 8 }}
          />
        )}

        <Button
          htmlType="submit"
          loading={changing}
          disabled={!currentPassword || !newPassword}
          block
          style={{ ...inputRadius, fontFamily: "var(--font-ui)", fontWeight: 600, background: ACCENT_AMBER, borderColor: ACCENT_AMBER, color: '#000' }}
        >
          Change Bootstrap Password
        </Button>
      </Space>
    </form>
  );
}
