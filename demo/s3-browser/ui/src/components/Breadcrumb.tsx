import { HomeFilled } from '@ant-design/icons';
import { prefixSegments } from '../utils';
import { getBucket } from '../s3client';
import { useColors } from '../ThemeContext';

interface Props {
  prefix: string;
  onNavigate: (prefix: string) => void;
}

const segmentBase: React.CSSProperties = {
  fontSize: 13,
  fontWeight: 500,
  whiteSpace: 'nowrap',
  overflow: 'hidden',
  textOverflow: 'ellipsis',
  fontFamily: "var(--font-ui)",
};

export default function Breadcrumb({ prefix, onNavigate }: Props) {
  const { TEXT_PRIMARY, TEXT_SECONDARY, TEXT_FAINT, ACCENT_BLUE } = useColors();
  const separatorStyle: React.CSSProperties = { color: TEXT_FAINT, margin: '0 6px', fontSize: 12, flexShrink: 0, userSelect: 'none' };
  const segments = prefixSegments(prefix);
  const bucket = getBucket();

  return (
    <nav aria-label="Breadcrumb">
      <ol style={{ display: 'flex', alignItems: 'center', minWidth: 0, overflow: 'hidden', listStyle: 'none', margin: 0, padding: 0 }}>
        {/* Home */}
        <li>
          <button
            className="btn-reset"
            onClick={() => onNavigate('')}
            aria-label="Home"
            style={{ color: prefix ? TEXT_SECONDARY : ACCENT_BLUE, fontSize: 14, flexShrink: 0, transition: 'color 0.15s' }}
          >
            <HomeFilled aria-hidden="true" />
          </button>
        </li>

        <li aria-hidden="true" style={separatorStyle}>&rsaquo;</li>

        {/* Bucket name */}
        <li>
          {prefix ? (
            <button
              className="btn-reset"
              onClick={() => onNavigate('')}
              style={{ ...segmentBase, color: TEXT_SECONDARY, maxWidth: 140, transition: 'color 0.15s' }}
            >
              {bucket}
            </button>
          ) : (
            <span style={{ ...segmentBase, color: TEXT_PRIMARY, maxWidth: 140, fontWeight: 600 }} aria-current="location">
              {bucket}
            </span>
          )}
        </li>

        {/* Prefix segments */}
        {segments.map((seg, i) => {
          const isLast = i === segments.length - 1;
          return (
            <li key={seg.prefix} style={{ display: 'flex', alignItems: 'center', minWidth: 0 }}>
              <span aria-hidden="true" style={separatorStyle}>&rsaquo;</span>
              {isLast ? (
                <span
                  style={{ ...segmentBase, color: TEXT_PRIMARY, fontWeight: 600 }}
                  aria-current="location"
                >
                  {seg.label}
                </span>
              ) : (
                <button
                  className="btn-reset"
                  onClick={() => onNavigate(seg.prefix)}
                  style={{ ...segmentBase, color: TEXT_SECONDARY, maxWidth: 140, transition: 'color 0.15s' }}
                >
                  {seg.label}
                </button>
              )}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
