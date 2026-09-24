import { Alert, Button, Card, Space, Typography } from 'antd';
import { DownloadOutlined, UploadOutlined } from '@ant-design/icons';

import { contentColumn, CONTENT_FORM } from './shared-styles';

const { Text } = Typography;

interface RecoveryPanelProps {
  onExportBackup: () => void;
  onImportBackup: () => void;
}

export default function RecoveryPanel({ onExportBackup, onImportBackup }: RecoveryPanelProps) {
  // Same content column as the other System cards (was full-width).
  return (
    <div style={{ ...contentColumn(CONTENT_FORM), paddingTop: 0 }}>
    <Card style={{ borderRadius: 12 }}>
      <Space orientation="vertical" size={16} style={{ width: '100%' }}>
        <Text type="secondary">
          Download a full backup bundle (config + IAM/control-plane data), or restore from a previous export.
        </Text>
        {/* The zip is not encrypted: secrets.json and iam.json carry every
            access key, user secret and backend credential in plain text. */}
        <Text type="secondary">
          The file holds the access keys, user secrets and backend credentials in plain text. Store it like a password. Secrets that come from environment variables are not in the file.
        </Text>
        <Space wrap>
          <Button type="primary" icon={<DownloadOutlined />} onClick={onExportBackup}>
            Download backup
          </Button>
          <Button icon={<UploadOutlined />} onClick={onImportBackup}>
            Restore backup
          </Button>
        </Space>
        <Alert
          type="warning"
          showIcon
          title="Restores can replace IAM users/groups/providers and configuration state for this instance."
        />
      </Space>
    </Card>
    </div>
  );
}
