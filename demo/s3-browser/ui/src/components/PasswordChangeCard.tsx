import { useState } from 'react';
import { Button, Input, Space, Alert, Typography } from 'antd';
import { LockOutlined, WarningOutlined } from '@ant-design/icons';
import { changeAdminPassword } from '../adminApi';
import { normalizeUiError } from '../errorHandling';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import { CodeTokenText } from './CodeTokenText';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

export default function PasswordChangeCard() {
  const { cardStyle, inputRadius } = useCardStyles();
  const { TEXT_MUTED } = useColors();

  const [currentPassword, setCurrentPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [changing, setChanging] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; error?: string } | null>(null);
  // The button is always usable; a click checks the fields and marks the
  // empty or short one (the same rule as the server: 12 to 128 characters).
  // After the first click the errors follow each keystroke.
  const [showFieldErrors, setShowFieldErrors] = useState(false);
  const currentError = !currentPassword ? 'Enter the current password.' : undefined;
  const newError = !newPassword
    ? 'Enter a new password.'
    : newPassword.length < 12
      ? 'Use at least 12 characters.'
      : newPassword.length > 128
        ? 'Use 128 characters or fewer.'
        : undefined;

  const handleSubmit = async () => {
    setShowFieldErrors(true);
    if (currentError || newError) return;
    setChanging(true);
    setResult(null);
    try {
      const res = await changeAdminPassword(currentPassword, newPassword);
      setResult(res);
      if (res.ok) {
        setCurrentPassword('');
        setNewPassword('');
        setShowFieldErrors(false);
      }
    } catch (e) {
      setResult({ ok: false, error: normalizeUiError(e, 'The password change failed') });
    } finally {
      setChanging(false);
    }
  };

  return (
    <form onSubmit={(e) => { e.preventDefault(); handleSubmit(); }} style={cardStyle}>
      <Space orientation="vertical" size="middle" style={{ width: '100%' }}>
        <SectionHeader
          icon={<LockOutlined />}
          title="Admin password"
          description="Unlocks this admin GUI. It is not the S3 SigV4 secret above."
        />

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
        <FormField label="Current admin password" error={showFieldErrors ? currentError : undefined}>
          <Input.Password
            value={currentPassword}
            onChange={(e) => setCurrentPassword(e.target.value)}
            autoComplete="current-password"
            style={inputRadius}
          />
        </FormField>
        <FormField
          label="New admin password"
          helpText="12 to 128 characters."
          error={showFieldErrors ? newError : undefined}
        >
          <Input.Password
            value={newPassword}
            onChange={(e) => setNewPassword(e.target.value)}
            autoComplete="new-password"
            style={inputRadius}
          />
        </FormField>

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
          type="primary"
          loading={changing}
          style={{ ...inputRadius, fontFamily: "var(--font-ui)", fontWeight: 600 }}
        >
          Change admin password
        </Button>
      </Space>
    </form>
  );
}
