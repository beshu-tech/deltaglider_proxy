import { GlobalOutlined } from '@ant-design/icons';
import { useAdminConfig } from '../queries/config';
import { publicMarkerAt, publicScope, publicScopeText } from '../bucketPublic';
import { useColors } from '../ThemeContext';

/** A globe beside a bucket that anonymous clients can read (all of it or
 *  some prefixes). The admin config says so, so it shows for admins only;
 *  without it the marker stays hidden rather than guess. */
export default function BucketPublicIcon({ bucket, canAdmin, label = false, prefix }: {
  bucket: string;
  canAdmin: boolean;
  /** Also show the word "Public" (the header); the sidebar row shows the icon only. */
  label?: boolean;
  /** The open folder (the header): the marker describes it, not the whole bucket. */
  prefix?: string;
}) {
  const { TEXT_MUTED } = useColors();
  const { data: config } = useAdminConfig({ enabled: canAdmin && !!bucket });
  const scope = canAdmin ? publicScope(config, bucket) : null;
  if (!scope) return null;
  const marker = prefix === undefined
    ? { label: scope.whole ? 'Public' : 'Partly public', text: publicScopeText(scope) }
    : publicMarkerAt(scope, prefix);
  if (!marker) return null;
  const { text } = marker;
  return (
    <span
      role="img"
      aria-label={text}
      title={text}
      style={{ display: 'inline-flex', alignItems: 'center', gap: 4, color: TEXT_MUTED, fontSize: 11, flex: 'none', lineHeight: 1 }}
    >
      <GlobalOutlined aria-hidden="true" />
      {label && <span style={{ fontSize: 12, fontFamily: 'var(--font-ui)', whiteSpace: 'nowrap' }}>{marker.label}</span>}
    </span>
  );
}
