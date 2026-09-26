import { useState } from 'react';
import { Button, Input, Space, Alert, Typography } from 'antd';
import { LockOutlined, WarningOutlined } from '@ant-design/icons';
import { changeAdminPassword } from '../adminApi';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import { CodeTokenText } from './CodeTokenText';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

export default function PasswordChangeCard() {
  const { cardStyle, inputRadius } = useCardStyles();
  const { TEXT_MUTED, TEXT_PRIMARY, ACCENT_AMBER } = useColors();

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

        <div style={{ fontSize: 13, color: TEXT_MUTED, lineHeight: 1.6 }}>
          <Text style={{ color: TEXT_MUTED, fontSize: 13 }}>
            The admin password does two things:
          </Text>
          <ul style={{ margin: '8px 0', paddingLeft: 20 }}>
            <li><strong>Signs admin session cookies</strong>: it authenticates your browser session for this settings panel.</li>
            <li><strong>Gates admin access</strong>: before IAM users exist, you need this password to open settings.</li>
          </ul>
          <Text style={{ color: TEXT_MUTED, fontSize: 13 }}>
            It does not encrypt the IAM database. The database has its own key: <code>DGP_CONFIG_DB_KEY</code>, or the key
            file <code>deltaglider_config.db.key</code> next to the database.
          </Text>
        </div>

        <Alert
          type="warning"
          icon={<WarningOutlined />}
          showIcon
          title="Changing this password signs you and every other admin out"
          description={
            <CodeTokenText text="All active admin sessions end. IAM users and their credentials stay as they are. If you forget this password, reset it with the CLI flag --set-bootstrap-password. The reset keeps the IAM database." />
          }
          style={{ borderRadius: 8 }}
        />

        <input type="text" autoComplete="username" defaultValue="admin" aria-hidden="true" style={{ display: 'none' }} />
        <Input.Password
          placeholder="Current admin password"
          value={currentPassword}
          onChange={(e) => setCurrentPassword(e.target.value)}
          autoComplete="current-password"
          style={inputRadius}
        />
        <Input.Password
          placeholder="New admin password"
          value={newPassword}
          onChange={(e) => setNewPassword(e.target.value)}
          autoComplete="new-password"
          style={inputRadius}
        />

        {result && (
          <Alert
            type={result.ok ? 'success' : 'error'}
            title={result.ok ? 'Admin password changed. All sessions invalidated.' : <CodeTokenText text={result.error || 'Failed'} />}
            showIcon
            style={{ borderRadius: 8 }}
          />
        )}

        <Button
          htmlType="submit"
          loading={changing}
          disabled={!currentPassword || !newPassword}
          block
          style={{ ...inputRadius, fontFamily: "var(--font-ui)", fontWeight: 600, background: ACCENT_AMBER, borderColor: ACCENT_AMBER, color: TEXT_PRIMARY }}
        >
          Change Admin Password
        </Button>
      </Space>
    </form>
  );
}
