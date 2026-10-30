import { Alert, Button, Space, Typography } from 'antd';
import { CloudDownloadOutlined, DownloadOutlined, UploadOutlined } from '@ant-design/icons';

import SectionHeader from './SectionHeader';
import { useCardStyles } from './shared-styles';

const { Text } = Typography;

interface RecoveryPanelProps {
  onExportBackup: () => void;
  onImportBackup: () => void;
}

/** The System page's Backup card: same chrome and header as the other System
 *  cards (SystemPanel spaces them). */
export default function RecoveryPanel({ onExportBackup, onImportBackup }: RecoveryPanelProps) {
  const { cardStyle } = useCardStyles();
  return (
    <div style={{ ...cardStyle, marginBottom: 0 }}>
      <SectionHeader
        icon={<CloudDownloadOutlined />}
        title="Backup"
        description="Download a full backup bundle (configuration, users, groups and providers), or restore from a previous export."
      />
      <Space orientation="vertical" size={16} style={{ width: '100%' }}>
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
          title="Restores can replace the users, groups, providers and configuration of this instance."
        />
      </Space>
    </div>
  );
}
