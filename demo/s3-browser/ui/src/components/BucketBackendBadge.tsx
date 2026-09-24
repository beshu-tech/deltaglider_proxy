import type { BucketBackendOrigin } from '../types';
import { useColors } from '../ThemeContext';
import { backendChipLabel, describeBackend } from '../bucketBackend';

interface Props {
  origin?: BucketBackendOrigin;
}

/** One neutral chip with the bucket's real backend name. Renders nothing
 *  when the origin data is not available. See `bucketBackend.ts`. */
export default function BucketBackendBadge({ origin }: Props) {
  const { BORDER, TEXT_MUTED } = useColors();
  const label = backendChipLabel(origin);
  if (!label) return null;
  return (
    <span
      aria-label={`Backend ${label}`}
      title={describeBackend(origin)}
      style={{
        display: 'inline-block',
        maxWidth: 96,
        height: 17,
        padding: '0 6px',
        borderRadius: 4,
        border: `1px solid ${BORDER}`,
        color: TEXT_MUTED,
        fontFamily: 'var(--font-mono)',
        fontSize: 10,
        lineHeight: '15px',
        overflow: 'hidden',
        textOverflow: 'ellipsis',
        whiteSpace: 'nowrap',
        // Gives way before the bucket name beside it: the name is what the
        // operator reads, the chip is secondary.
        flex: '0 1 auto',
        minWidth: 0,
      }}
    >
      {label}
    </span>
  );
}
