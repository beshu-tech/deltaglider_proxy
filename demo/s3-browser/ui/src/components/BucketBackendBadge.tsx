import type { BucketBackendOrigin } from '../types';

type ProviderKind = 'local' | 'hetzner' | 'aws' | 's3';

interface ProviderBadge {
  kind: ProviderKind;
  label: string;
  bg: string;
  fg: string;
  border: string;
}

const BADGES: Record<ProviderKind, ProviderBadge> = {
  local: {
    kind: 'local',
    label: 'LOC',
    bg: 'rgba(45, 212, 191, 0.12)',
    fg: '#2dd4bf',
    border: 'rgba(45, 212, 191, 0.34)',
  },
  hetzner: {
    kind: 'hetzner',
    label: 'HZ',
    bg: 'rgba(213, 43, 30, 0.13)',
    fg: '#ff5a4f',
    border: 'rgba(213, 43, 30, 0.38)',
  },
  aws: {
    kind: 'aws',
    label: 'AWS',
    bg: 'rgba(255, 153, 0, 0.14)',
    fg: '#ffb020',
    border: 'rgba(255, 153, 0, 0.38)',
  },
  s3: {
    kind: 's3',
    label: 'S3',
    bg: 'rgba(148, 163, 184, 0.12)',
    fg: '#94a3b8',
    border: 'rgba(148, 163, 184, 0.32)',
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
  const badge = BADGES[classifyBackend(origin)];
  return (
    <span
      aria-label={`${badge.label} backend`}
      title={describeBackend(origin)}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        justifyContent: 'center',
        width: badge.kind === 'aws' ? 28 : 24,
        height: 16,
        borderRadius: badge.kind === 'hetzner' ? 4 : 999,
        border: `1px solid ${badge.border}`,
        background: badge.bg,
        color: badge.fg,
        fontFamily: 'var(--font-mono)',
        fontSize: badge.kind === 'aws' ? 8 : 9,
        fontWeight: 800,
        letterSpacing: badge.kind === 'aws' ? 0 : 0.4,
        lineHeight: 1,
        flexShrink: 0,
      }}
    >
      {badge.label}
    </span>
  );
}
