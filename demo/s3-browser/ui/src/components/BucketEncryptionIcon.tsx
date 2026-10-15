import { LockOutlined } from '@ant-design/icons';
import type { BucketBackendOrigin } from '../types';
import { useColors } from '../ThemeContext';

/** A padlock beside a bucket whose new writes are encrypted at rest. The
 *  hover text says how. Neutral like the backend chip: red and amber are
 *  for real problems only (see `bucketBackend.ts`). */
export default function BucketEncryptionIcon({ origin }: { origin?: BucketBackendOrigin }) {
  const { TEXT_MUTED } = useColors();
  const text = origin?.encryptedAtRest;
  if (!text) return null;
  return (
    <LockOutlined
      role="img"
      aria-label="Encrypted"
      title={text}
      style={{ color: TEXT_MUTED, fontSize: 11, flex: 'none' }}
    />
  );
}
