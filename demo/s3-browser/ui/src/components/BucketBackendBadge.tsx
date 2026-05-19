import type { BucketBackendOrigin } from '../types';
import { useTheme } from '../ThemeContext';

type ProviderKind = 'local' | 'hetzner' | 'aws' | 's3';

interface ProviderPalette {
  bg: string;
  fg: string;
  border: string;
}

interface ProviderBadge {
  kind: ProviderKind;
  label: string;
  squared: boolean;
  light: ProviderPalette;
  dark: ProviderPalette;
}

const BADGES: Record<ProviderKind, ProviderBadge> = {
  local: {
    kind: 'local',
    label: 'LOC',
    squared: false,
    light: { bg: '#ccfbf1', fg: '#115e59', border: '#5eead4' },
    dark:  { bg: 'rgba(45, 212, 191, 0.18)', fg: '#5eead4', border: 'rgba(45, 212, 191, 0.45)' },
  },
  hetzner: {
    kind: 'hetzner',
    label: 'HZ',
    squared: true,
    light: { bg: '#fee2e2', fg: '#991b1b', border: '#fca5a5' },
    dark:  { bg: 'rgba(248, 113, 113, 0.18)', fg: '#fca5a5', border: 'rgba(248, 113, 113, 0.50)' },
  },
  aws: {
    kind: 'aws',
    label: 'AWS',
    squared: false,
    light: { bg: '#fef3c7', fg: '#92400e', border: '#fcd34d' },
    dark:  { bg: 'rgba(251, 191, 36, 0.18)', fg: '#fcd34d', border: 'rgba(251, 191, 36, 0.50)' },
  },
  s3: {
    kind: 's3',
    label: 'S3',
    squared: false,
    light: { bg: '#e2e8f0', fg: '#334155', border: '#94a3b8' },
    dark:  { bg: 'rgba(148, 163, 184, 0.18)', fg: '#cbd5e1', border: 'rgba(148, 163, 184, 0.45)' },
  },
};

function classifyBackend(origin?: BucketBackendOrigin): ProviderKind {
  if (!origin) return 's3';
  const haystack = [
    origin.backendName,
    origin.backendType,
    origin.backendEndpoint,
    origin.backendRegion,
    origin.backendPath,
  ]
    .filter(Boolean)
    .join(' ')
    .toLowerCase();

  if (origin.backendType === 'filesystem' || /\blocal\b|filesystem|file:|\/data\b/.test(haystack)) {
    return 'local';
  }
  if (/hetzner|hel1|fsn1|nbg1|your-objectstorage\.com/.test(haystack)) {
    return 'hetzner';
  }
  if (/amazonaws\.com|\baws\b|s3[.-][a-z0-9-]+\.amazonaws\.com/.test(haystack)) {
    return 'aws';
  }
  return 's3';
}

function describeBackend(origin?: BucketBackendOrigin): string {
  if (!origin) return 'Backend origin unknown';
  const parts = [
    origin.backendName ? `backend: ${origin.backendName}` : null,
    origin.backendType ? `type: ${origin.backendType}` : null,
    origin.backendEndpoint ? `endpoint: ${origin.backendEndpoint}` : null,
    origin.backendRegion ? `region: ${origin.backendRegion}` : null,
    origin.backendPath ? `path: ${origin.backendPath}` : null,
    origin.realBucket ? `real bucket: ${origin.realBucket}` : null,
  ].filter(Boolean);
  return parts.length > 0 ? parts.join(' | ') : 'Backend origin unknown';
}

interface Props {
  origin?: BucketBackendOrigin;
}

export default function BucketBackendBadge({ origin }: Props) {
  const { isDark } = useTheme();
  const badge = BADGES[classifyBackend(origin)];
  const palette = isDark ? badge.dark : badge.light;
  return (
    <span
      aria-label={`${badge.label} backend`}
      title={describeBackend(origin)}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        justifyContent: 'center',
        minWidth: 26,
        height: 17,
        padding: '0 6px',
        borderRadius: badge.squared ? 4 : 999,
        border: `1px solid ${palette.border}`,
        background: palette.bg,
        color: palette.fg,
        fontFamily: 'var(--font-mono)',
        fontSize: 10,
        fontWeight: 700,
        letterSpacing: 0.3,
        lineHeight: 1,
        flexShrink: 0,
      }}
    >
      {badge.label}
    </span>
  );
}
