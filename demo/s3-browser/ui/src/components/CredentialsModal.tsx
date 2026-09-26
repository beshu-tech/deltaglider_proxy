import { Button, Modal, Space, Typography } from 'antd';
import { CopyOutlined } from '@ant-design/icons';
import { useCopyToClipboard } from '../useCopyToClipboard';

const { Text } = Typography;

interface CredentialsModalProps {
  accessKey: string;
  secretKey: string;
  /** Dialog title, e.g. "User created: save these credentials". */
  title: string;
  onClose: () => void;
}

/**
 * Newly created or rotated IAM credentials. The server returns the secret
 * exactly once, so only the explicit "I have copied it" button closes this:
 * no Escape, no mask click, no close icon, and no parent re-render (a URL
 * change, a list refetch) can take it away.
 */
export default function CredentialsModal({ accessKey, secretKey, title, onClose }: CredentialsModalProps) {
  const { copy } = useCopyToClipboard();
  return (
    <Modal
      open
      title={title}
      closable={false}
      mask={{ closable: false }}
      keyboard={false}
      footer={<Button type="primary" onClick={onClose}>I have copied it</Button>}
    >
      <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase' }}>Access key</Text>
      <Space.Compact block style={{ marginBottom: 12, alignItems: 'center' }}>
        <Text code style={{ fontFamily: 'var(--font-mono)', flex: 1, wordBreak: 'break-all' }}>{accessKey}</Text>
        <Button
          icon={<CopyOutlined />}
          aria-label="Copy access key"
          onClick={() => void copy(accessKey, { successMessage: 'Access key copied' })}
        />
      </Space.Compact>
      <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase' }}>Secret key</Text>
      <Space.Compact block style={{ alignItems: 'center' }}>
        <Text code style={{ fontFamily: 'var(--font-mono)', flex: 1, wordBreak: 'break-all' }}>{secretKey}</Text>
        <Button
          icon={<CopyOutlined />}
          aria-label="Copy secret key"
          onClick={() => void copy(secretKey, { successMessage: 'Secret key copied' })}
        />
      </Space.Compact>
      <Text type="warning" style={{ fontSize: 12, marginTop: 12, display: 'block' }}>
        This is the only time the secret is shown. Store it now.
      </Text>
    </Modal>
  );
}
