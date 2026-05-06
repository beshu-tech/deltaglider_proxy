import { useState, useEffect } from 'react';
import { Alert } from 'antd';
import { useColors } from '../ThemeContext';

const STORAGE_KEY = 'dg-browser-lift-hint-dismissed';

interface Props {
  visible: boolean;
}

/** One-time dismissible strip: explains browser-lift vs full admin GUI capabilities. */
export default function BrowserLiftBanner({ visible }: Props) {
  const colors = useColors();
  const [dismissed, setDismissed] = useState(
    () => typeof localStorage !== 'undefined' && localStorage.getItem(STORAGE_KEY) === '1',
  );

  useEffect(() => {
    if (!visible) return;
    setDismissed(localStorage.getItem(STORAGE_KEY) === '1');
  }, [visible]);

  if (!visible || dismissed) return null;

  return (
    <Alert
      type="info"
      showIcon
      closable
      message="Browser-only session"
      description="Bulk operations, folder size scans, metrics, bucket backend details, and full bucket policy in the inspector require a full admin session (bootstrap password, IAM admin via login-as, or OAuth)."
      style={{
        margin: '0 20px 12px',
        borderRadius: 10,
        border: `1px solid ${colors.BORDER}`,
        background: `${colors.ACCENT_BLUE}08`,
      }}
      onClose={() => {
        localStorage.setItem(STORAGE_KEY, '1');
        setDismissed(true);
      }}
    />
  );
}
