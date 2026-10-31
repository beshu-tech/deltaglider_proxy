import { useState } from 'react';
import { Button, Select, Space, Typography } from 'antd';
import { CopyOutlined, FileTextOutlined, ImportOutlined, SafetyCertificateOutlined, TeamOutlined } from '@ant-design/icons';
import type { SectionName } from '../adminApi';
import SectionHeader from './SectionHeader';
import { SectionYamlModal } from './CopySectionYamlButton';
import { SPACE, useCardStyles } from './shared-styles';

const { Text } = Typography;

const SECTIONS: Array<{ value: SectionName; label: string }> = [
  { value: 'admission', label: 'Admission' },
  { value: 'access', label: 'Access' },
  { value: 'storage', label: 'Storage' },
  { value: 'advanced', label: 'Advanced' },
];

export interface ConfigYamlActions {
  onExportSettingsYaml: () => void;
  onImportSettingsYaml: () => void;
  onExportFullIam: () => void;
  onImportFullIam: () => void;
  /** Why the full-IAM import is refused (declarative mode); the button is then disabled. */
  importFullIamBlocked?: string;
}

/**
 * The System page's "Configuration as YAML" card (UX harvest D72): the
 * import and export of one section, of the settings and of the full IAM.
 * These lived in the account menu, which keeps only account things; the
 * warning about live secrets belongs next to the button that exports them.
 */
export default function ConfigYamlCard({
  onExportSettingsYaml,
  onImportSettingsYaml,
  onExportFullIam,
  onImportFullIam,
  importFullIamBlocked,
}: ConfigYamlActions) {
  const { cardStyle } = useCardStyles();
  const [section, setSection] = useState<SectionName>('storage');
  const [sectionOpen, setSectionOpen] = useState(false);
  return (
    <div style={{ ...cardStyle, marginBottom: 0 }} data-testid="config-yaml-card">
      <SectionHeader
        icon={<FileTextOutlined />}
        title="Configuration as YAML"
        description="Copy one section, or export and import the settings or the full IAM as YAML."
      />
      <Space orientation="vertical" size={SPACE.md} style={{ width: '100%' }}>
        <div>
          <Text strong style={{ display: 'block', marginBottom: SPACE.xxs }}>Settings</Text>
          <Text type="secondary" style={{ display: 'block', marginBottom: SPACE.xs }}>
            The settings do not include users, groups or providers.
          </Text>
          <Space wrap>
            <Select
              aria-label="Section"
              size="middle"
              value={section}
              onChange={setSection}
              options={SECTIONS}
              style={{ width: 140 }}
            />
            <Button icon={<CopyOutlined />} onClick={() => setSectionOpen(true)}>
              Show section YAML
            </Button>
            <Button icon={<FileTextOutlined />} onClick={onExportSettingsYaml}>
              Export settings YAML
            </Button>
            <Button icon={<ImportOutlined />} onClick={onImportSettingsYaml}>
              Import settings YAML
            </Button>
          </Space>
        </div>
        <div>
          <Text strong style={{ display: 'block', marginBottom: SPACE.xxs }}>Full IAM</Text>
          <Text type="secondary" style={{ display: 'block', marginBottom: SPACE.xs }}>
            Users, groups, providers and mapping rules. The export holds the live secrets: store the file like a password.
          </Text>
          <Space wrap>
            <Button icon={<SafetyCertificateOutlined />} onClick={onExportFullIam}>
              Export full IAM (YAML)
            </Button>
            <Button icon={<TeamOutlined />} onClick={onImportFullIam} disabled={importFullIamBlocked !== undefined}>
              Import full IAM (YAML)
            </Button>
          </Space>
          {importFullIamBlocked && (
            <Text type="secondary" data-testid="iam-import-blocked" style={{ display: 'block', marginTop: SPACE.xs, fontSize: 12 }}>
              {importFullIamBlocked}
            </Text>
          )}
        </div>
      </Space>
      <SectionYamlModal section={section} open={sectionOpen} onClose={() => setSectionOpen(false)} />
    </div>
  );
}
