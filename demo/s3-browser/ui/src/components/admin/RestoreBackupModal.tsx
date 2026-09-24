import { Alert, Button, Modal, Space, Typography } from 'antd';
import type { ImportBackupMode } from '../../adminApi';
import { useIamMode } from '../../queries/config';

const { Text } = Typography;

interface Props {
  /** The picked zip; the modal is open while it is set. */
  file: File | null;
  onCancel: () => void;
  onRestore: (file: File, mode: ImportBackupMode) => void;
}

/** Restore-mode chooser for a Full Backup zip. */
export default function RestoreBackupModal({ file, onCancel, onRestore }: Props) {
  // Declarative IAM: the IAM-writing restore modes (full / iam-only /
  // preserve-bootstrap) would 403 server-side, so only Config Only is offered.
  // An unknown mode (config not loaded) gets the same treatment. No expiry
  // callback here: the page's own panels already route a 401 to sign-in.
  const { iamMode, readOnly: iamWritesOff, loadError: iamLoadError } = useIamMode();
  const iamDeclarative = iamMode === 'declarative';
  const restore = (mode: ImportBackupMode) => () => file && onRestore(file, mode);

  return (
    <Modal
      title="Restore Backup"
      open={file !== null}
      onCancel={onCancel}
      footer={[
        <Button key="cancel" onClick={onCancel}>
          Cancel
        </Button>,
        <Button key="config" onClick={restore('config-only')}>
          Config Only
        </Button>,
        ...(iamWritesOff ? [] : [
          <Button key="preserve-bootstrap" type="primary" onClick={restore('preserve-bootstrap')}>
            Everything Except Admin Password
          </Button>,
          <Button key="full" danger onClick={restore('full')}>
            Full Restore
          </Button>,
          <Button key="iam" onClick={restore('iam-only')}>
            IAM Only
          </Button>,
        ]),
      ]}
    >
      <Space direction="vertical" size={10}>
        <Text>
          Choose what to restore from <Text code>{file?.name}</Text>.
        </Text>
        {iamDeclarative && (
          <Alert
            type="warning"
            showIcon
            message="IAM is managed by YAML (declarative mode). Only Config Only is available — restore users, groups, and OIDC providers by editing access.iam_* in your YAML config and applying."
          />
        )}
        {iamLoadError && (
          <Alert
            type="warning"
            showIcon
            message={`Could not load the IAM mode, so only Config Only is offered. Reload the page to see every restore option. ${iamLoadError}`}
          />
        )}
        {!iamWritesOff && (
          <Alert
            type="info"
            showIcon
            message="Everything Except Admin Password restores config, backends, bucket policies, users, groups, OIDC providers, and secrets, while keeping this instance's local admin password."
          />
        )}
        {!iamWritesOff && (
          <Alert
            type="info"
            showIcon
            message="IAM Only skips config and backend changes; use it only when you want users/groups/OIDC without restoring storage settings."
          />
        )}
        {!iamWritesOff && (
          <Alert
            type="warning"
            showIcon
            message="Full Restore also tries to restore the backup's admin password and will fail if it doesn't match the admin password this instance was set up with."
          />
        )}
      </Space>
    </Modal>
  );
}
