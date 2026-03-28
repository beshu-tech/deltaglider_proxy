import { Alert, Typography } from 'antd';

const { Text } = Typography;

interface CredentialsBannerProps {
  accessKey: string;
  secretKey: string;
  message: string;
  onClose: () => void;
}

/** Reusable alert banner for displaying newly-created or rotated IAM credentials. */
export default function CredentialsBanner({ accessKey, secretKey, message, onClose }: CredentialsBannerProps) {
  return (
    <Alert
      type="success"
      showIcon
      closable
      onClose={onClose}
      message={message}
      description={
        <div style={{ marginTop: 8 }}>
          <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase' }}>Access Key</Text>
          <div><Text code copyable style={{ fontFamily: 'var(--font-mono)' }}>{accessKey}</Text></div>
          <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase', marginTop: 8, display: 'block' }}>Secret Key</Text>
          <div><Text code copyable style={{ fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>{secretKey}</Text></div>
          <Text type="warning" style={{ fontSize: 11, marginTop: 8, display: 'block' }}>The secret will not be shown again.</Text>
        </div>
      }
      style={{ borderRadius: 8 }}
    />
  );
}
