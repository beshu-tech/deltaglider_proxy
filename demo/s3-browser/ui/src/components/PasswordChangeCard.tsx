import { useState } from 'react';
import { Button, Space, Alert } from 'antd';
import { LockOutlined } from '@ant-design/icons';
import { changeAdminPassword } from '../adminApi';
import { plainError, type PlainError } from '../plainError';
import PlainErrorAlert from './PlainErrorAlert';
import { useCardStyles, SPACE } from './shared-styles';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import MaskedSecretInput from './MaskedSecretInput';
import { CodeTokenText } from './CodeTokenText';
import { useColors } from '../ThemeContext';

export default function PasswordChangeCard() {
  const { cardStyle, inputRadius } = useCardStyles();
  const { TEXT_MUTED } = useColors();

  const [currentPassword, setCurrentPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [changing, setChanging] = useState(false);
  const [result, setResult] = useState<{ ok: true } | { ok: false; error: PlainError } | null>(null);
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
      setResult(res.ok ? { ok: true } : { ok: false, error: { text: res.error || 'The proxy refused the password change.' } });
      if (res.ok) {
        setCurrentPassword('');
        setNewPassword('');
        setShowFieldErrors(false);
      }
    } catch (e) {
      setResult({ ok: false, error: plainError(e, 'The proxy could not change the password.') });
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

        {/* D64: what the password does and how to reset it fold away; the
            card shows only the fields and the one fact that matters now. */}
        <details style={{ fontSize: 13, color: TEXT_MUTED, lineHeight: 1.6 }}>
          <summary style={{ cursor: 'pointer', width: 'fit-content' }}>How the admin password works</summary>
          <ul style={{ margin: `${SPACE.xs}px 0`, paddingLeft: SPACE.md }}>
            <li>It signs the admin session cookies.</li>
            <li>Before IAM users exist, it is the only way to open the admin settings.</li>
            <li>
              It does not encrypt the IAM database. The database key is <code>DGP_CONFIG_DB_KEY</code>, or the
              file <code>deltaglider_config.db.key</code> next to the database.
            </li>
            <li>
              <CodeTokenText text="If you forget it, reset it with the CLI flag --set-bootstrap-password. The reset keeps the IAM database." />
            </li>
          </ul>
        </details>

        <input type="text" autoComplete="username" defaultValue="admin" aria-hidden="true" style={{ display: 'none' }} />
        <FormField label="Current admin password" error={showFieldErrors ? currentError : undefined}>
          <MaskedSecretInput
            mode="new"
            placeholder=""
            value={currentPassword}
            onChange={(v) => setCurrentPassword(v)}
            autoComplete="current-password"
            style={inputRadius}
          />
        </FormField>
        <FormField
          label="New admin password"
          helpText="12 to 128 characters."
          error={showFieldErrors ? newError : undefined}
        >
          <MaskedSecretInput
            mode="new"
            placeholder=""
            value={newPassword}
            onChange={(v) => setNewPassword(v)}
            autoComplete="new-password"
            style={inputRadius}
          />
        </FormField>

        {result?.ok && <Alert type="success" showIcon title="The admin password is changed. Every admin session is signed out." />}
        {result && !result.ok && <PlainErrorAlert error={result.error} />}

        <Button
          htmlType="submit"
          type="primary"
          loading={changing}
          style={{ ...inputRadius, fontFamily: "var(--font-ui)", fontWeight: 600 }}
        >
          Change admin password
        </Button>
        <div style={{ fontSize: 12.5, color: TEXT_MUTED, marginTop: -SPACE.xs }}>
          This signs out every admin, you included. IAM users and their keys do not change.
        </div>
      </Space>
    </form>
  );
}
