import type { ComponentProps } from 'react';
import { Empty } from 'antd';
import UploadPage from '../components/UploadPage';

type Props = ComponentProps<typeof UploadPage> & {
  /** The user may upload into the active bucket (some writable prefix). */
  canUpload: boolean;
};

/** The upload page, or why the user cannot upload here. */
export default function UploadScreen({ canUpload, ...page }: Props) {
  if (!canUpload) {
    return (
      <Empty
        description="You do not have permission to upload to this bucket."
        style={{ padding: '64px 0' }}
      />
    );
  }
  return <UploadPage {...page} />;
}
