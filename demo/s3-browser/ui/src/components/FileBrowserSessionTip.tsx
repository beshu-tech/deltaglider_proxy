import { useState, useEffect } from 'react';
import { Alert, Button, Space } from 'antd';
import { CloseOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { useNavigation } from '../NavigationContext';
import { readStorage, writeStorage } from '../safeStorage';
import { useIsNarrow } from '../useIsNarrow';

const STORAGE_KEY = 'dg-file-browser-session-tip-dismissed';

interface Props {
  visible: boolean;
  /** Who is signed in (access key or name): the dismissal is remembered per user. */
  userKey?: string;
}

/** Dismissible tip when the user signed in with an access key but has not opened Admin. */
export default function FileBrowserSessionTip({ visible, userKey = '' }: Props) {
  const colors = useColors();
  const { navigate } = useNavigation();
  const narrow = useIsNarrow(600);
  const key = `${STORAGE_KEY}:${userKey}`;
  const [dismissed, setDismissed] = useState(() => readStorage(key) === '1');

  useEffect(() => {
    setDismissed(readStorage(key) === '1');
  }, [visible, key]);

  if (!visible || dismissed) return null;

  const dismiss = (
    <Button
      type="text"
      size="small"
      icon={<CloseOutlined />}
      aria-label="Dismiss this tip"
      onClick={() => {
        writeStorage(key, '1');
        setDismissed(true);
      }}
    />
  );

  return (
    <Alert
      type="info"
      showIcon={!narrow}
      action={dismiss}
      title="Signed in for files only"
      description={
        narrow ? undefined : (
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
        )
      }
      style={{
        margin: narrow ? '0 12px 8px' : '0 20px 12px',
        padding: narrow ? '6px 10px' : undefined,
        borderRadius: 10,
        border: `1px solid ${colors.BORDER}`,
        background: `${colors.ACCENT_BLUE}08`,
      }}
    />
  );
}
