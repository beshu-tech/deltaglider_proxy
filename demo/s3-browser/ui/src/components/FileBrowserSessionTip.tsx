import { useState, useEffect } from 'react';
import { Alert, Button, Space } from 'antd';
import { useColors } from '../ThemeContext';
import { useNavigation } from '../NavigationContext';
import { readStorage, writeStorage } from '../safeStorage';

const STORAGE_KEY = 'dg-file-browser-session-tip-dismissed';

interface Props {
  visible: boolean;
}

/** Dismissible tip when the user signed in with an access key but has not opened Admin. */
export default function FileBrowserSessionTip({ visible }: Props) {
  const colors = useColors();
  const { navigate } = useNavigation();
  const [dismissed, setDismissed] = useState(() => readStorage(STORAGE_KEY) === '1');

  useEffect(() => {
    if (!visible) return;
    setDismissed(readStorage(STORAGE_KEY) === '1');
  }, [visible]);

  if (!visible || dismissed) return null;

  return (
    <Alert
      type="info"
      showIcon
      closable={{
        onClose: () => {
          writeStorage(STORAGE_KEY, '1');
          setDismissed(true);
        },
      }}
      title="Signed in for files only"
      description={
        <Space orientation="vertical" size="small" style={{ width: '100%' }}>
          <span>
            You connected with an access key, so you can browse buckets and objects. For bulk actions, folder sizes,
            metrics, and full bucket details in the inspector, open Settings and sign in as an administrator (bootstrap
            password or an admin IAM account, depending on your setup).
          </span>
          <div>
            <Button type="primary" size="small" onClick={() => navigate('admin')}>
              Open Settings
            </Button>
          </div>
        </Space>
      }
      style={{
        margin: '0 20px 12px',
        borderRadius: 10,
        border: `1px solid ${colors.BORDER}`,
        background: `${colors.ACCENT_BLUE}08`,
      }}
    />
  );
}
