import { useState } from 'react';
import { Alert, Button, Modal, Radio, Space, Typography } from 'antd';
import type { ImportBackupMode } from '../../adminApi';
import { useIamMode } from '../../queries/config';

const { Text } = Typography;

interface Props {
  /** The picked zip; the modal is open while it is set. */
  file: File | null;
  onCancel: () => void;
  onRestore: (file: File, mode: ImportBackupMode) => void;
}

/** Every restore mode, in the order shown; `iam` marks modes that write IAM. */
const MODES: { mode: ImportBackupMode; label: string; help: string; iam: boolean }[] = [
  {
    mode: 'preserve-bootstrap',
    label: 'Everything except the admin password',
    help: "Config, backends, bucket policies, users, groups, OIDC providers and secrets. Keeps this instance's admin password.",
    iam: true,
  },
  {
    mode: 'config-only',
    label: 'Config only',
    help: 'Config, backends and bucket policies. Users, groups and OIDC providers stay as they are.',
    iam: false,
  },
  {
    mode: 'iam-only',
    label: 'Users and groups only',
    help: 'Users, groups and OIDC providers. Storage settings stay as they are.',
    iam: true,
  },
  {
    mode: 'full',
    label: 'Everything, including the admin password',
    help: "Fails if the backup's admin password does not match the one this instance was set up with.",
    iam: true,
  },
];

/**
 * Restore-mode chooser for a Full Backup zip: one list of choices, each with
 * its own description, and a single Restore button. (It used to be five
 * footer buttons — they overflowed the dialog — with three of the four
 * options explained in separate alerts above.)
 */
export default function RestoreBackupModal({ file, onCancel, onRestore }: Props) {
  // Declarative IAM: the IAM-writing restore modes would 403 server-side, so
  // only Config only is offered. An unknown mode (config not loaded) gets the
  // same treatment. No expiry callback here: the page's own panels already
  // route a 401 to sign-in.
  const { iamMode, readOnly: iamWritesOff, loadError: iamLoadError } = useIamMode();
  const iamDeclarative = iamMode === 'declarative';
  const modes = MODES.filter((m) => !m.iam || !iamWritesOff);
  // The pick belongs to one file: a newly chosen zip (or a pick that is no
  // longer offered) starts again from the first, safest option.
  const [pick, setPick] = useState<{ file: File | null; mode: ImportBackupMode } | null>(null);
  const mode =
    pick && pick.file === file && modes.some((m) => m.mode === pick.mode) ? pick.mode : modes[0].mode;

  return (
    <Modal
      title="Restore backup"
      open={file !== null}
      onCancel={onCancel}
      footer={[
        <Button key="cancel" onClick={onCancel}>
          Cancel
        </Button>,
        <Button
          key="restore"
          type="primary"
          danger={mode === 'full'}
          onClick={() => file && onRestore(file, mode)}
        >
          Restore
        </Button>,
      ]}
    >
      <Space direction="vertical" size={12} style={{ width: '100%' }}>
        <Text>
          Choose what to restore from <Text code>{file?.name}</Text>.
        </Text>
        {iamDeclarative && (
          <Alert
            type="warning"
            showIcon
            message="IAM is managed by YAML (declarative mode), so only Config only is available. Restore users, groups and OIDC providers by editing access.iam_* in your YAML config and applying it."
          />
        )}
        {iamLoadError && (
          <Alert
            type="warning"
            showIcon
            message={`Could not load the IAM mode, so only Config only is offered. Reload the page to see every restore option. ${iamLoadError}`}
          />
        )}
        <Radio.Group
          value={mode}
          onChange={(e) => setPick({ file, mode: e.target.value })}
          style={{ width: '100%' }}
        >
          <Space direction="vertical" size={10} style={{ width: '100%' }}>
            {modes.map((m) => (
              <Radio key={m.mode} value={m.mode}>
                <Text strong>{m.label}</Text>
                <br />
                <Text type="secondary" style={{ fontSize: 12 }}>
                  {m.help}
                </Text>
              </Radio>
            ))}
          </Space>
        </Radio.Group>
      </Space>
    </Modal>
  );
}
