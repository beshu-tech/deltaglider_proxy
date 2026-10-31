import type { ComponentProps } from 'react';
import { LockOutlined } from '@ant-design/icons';
import UploadPage from '../components/UploadPage';
import { EmptyState } from '../components/StatePlaceholders';

type Props = ComponentProps<typeof UploadPage> & {
  /** The user may upload into the active bucket (some writable prefix). */
  canUpload: boolean;
};

/** The upload page, or why the user cannot upload here. */
export default function UploadScreen({ canUpload, ...page }: Props) {
  if (!canUpload) {
    return (
      <EmptyState
        icon={<LockOutlined />}
        title="You cannot upload to this bucket"
        hint="Your account has no write permission here. Ask an administrator for access, or pick another bucket."
      />
    );
  }
  return <UploadPage {...page} />;
}
