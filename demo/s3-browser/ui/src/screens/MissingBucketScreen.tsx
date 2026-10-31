import { Button } from 'antd';
import { InboxOutlined } from '@ant-design/icons';
import TruncatedName from '../components/TruncatedName';
import { useColors } from '../ThemeContext';

interface Props {
  /** The bucket of the URL, which the bucket list does not have. */
  bucket: string;
  /** The buckets the user can open instead. */
  buckets: string[];
  onOpenBucket: (bucket: string) => void;
}

/**
 * A linked bucket that does not exist, or that the user cannot see. The URL
 * stays as it was linked: the page says so and offers the real buckets,
 * instead of opening another bucket in silence.
 */
export default function MissingBucketScreen({ bucket, buckets, onOpenBucket }: Props) {
  const { TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY } = useColors();
  return (
    <div
      role="status"
      style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 12, padding: '64px 16px', textAlign: 'center' }}
    >
      <InboxOutlined aria-hidden="true" style={{ fontSize: 32, color: TEXT_MUTED }} />
      <h1 style={{ margin: 0, fontSize: 18, fontWeight: 600, color: TEXT_PRIMARY, fontFamily: 'var(--font-ui)', overflowWrap: 'anywhere' }}>
        No bucket named <code>{bucket}</code>
      </h1>
      <p style={{ margin: 0, maxWidth: 520, fontSize: 14, color: TEXT_SECONDARY, fontFamily: 'var(--font-ui)', lineHeight: 1.5 }}>
        This bucket does not exist, or your account cannot see it. Check the link, or open one of your buckets.
      </p>
      {buckets.length > 0 && (
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8, justifyContent: 'center', maxWidth: 'min(640px, 100%)' }}>
          {buckets.map((b) => (
            // D12: a long bucket name stays inside the screen, cut in the middle.
            <Button
              key={b}
              onClick={() => onOpenBucket(b)}
              style={{ fontFamily: 'var(--font-mono)', maxWidth: '100%', minWidth: 0 }}
            >
              <TruncatedName text={b} />
            </Button>
          ))}
        </div>
      )}
    </div>
  );
}
