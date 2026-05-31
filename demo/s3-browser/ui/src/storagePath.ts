export interface ResourcePatternParts {
  bucket: string;
  prefix: string;
  wildcard: boolean;
  global: boolean;
}

export interface CommaSegment {
  before: string;
  segment: string;
}

function cleanPathSegments(value: string): string[] {
  return value
    .replace(/\\/g, '/')
    .split('/')
    .map((part) => part.trim())
    .filter(Boolean);
}

export function normalizePrefix(value: string | null | undefined): string {
  const parts = cleanPathSegments(value || '');
  return parts.length === 0 ? '' : `${parts.join('/')}/`;
}

/**
 * Clean separators in a path-like value WITHOUT forcing a trailing slash.
 *
 * Collapses `\` → `/`, removes duplicate/empty segments, trims — but
 * preserves the caller's trailing-slash decision. Unlike
 * {@link normalizePrefix} (which always appends `/` to mark a folder),
 * this is for contexts where `foo/bar` and `foo/bar/` are SEMANTICALLY
 * DISTINCT — e.g. an `s3:prefix StringLike` condition, where the
 * trailing slash is the difference between matching only keys under
 * `foo/bar/` and also matching `foo/bar-baz`.
 */
export function normalizePrefixPreserveTrailingSlash(value: string | null | undefined): string {
  const raw = (value || '').replace(/\\/g, '/');
  const hadTrailingSlash = raw.trim().endsWith('/');
  const parts = cleanPathSegments(raw);
  if (parts.length === 0) return '';
  const joined = parts.join('/');
  return hadTrailingSlash ? `${joined}/` : joined;
}

export function getTrailingCommaSegment(value: string | null | undefined): CommaSegment {
  const raw = value || '';
  const commaIndex = raw.lastIndexOf(',');
  const beforeComma = commaIndex >= 0 ? raw.slice(0, commaIndex + 1) : '';
  const afterComma = commaIndex >= 0 ? raw.slice(commaIndex + 1) : raw;
  const leadingSpace = afterComma.match(/^\s*/)?.[0] || '';

  return {
    before: `${beforeComma}${leadingSpace}`,
    segment: afterComma.slice(leadingSpace.length),
  };
}

export function replaceTrailingCommaSegment(value: string | null | undefined, replacement: string): string {
  const { before } = getTrailingCommaSegment(value);
  return `${before}${replacement}`;
}

export function parseResourcePattern(value: string | null | undefined): ResourcePatternParts {
  const raw = (value || '').trim();
  if (!raw || raw === '*') {
    return { bucket: '', prefix: '', wildcard: raw === '*', global: raw === '*' };
  }

  const wildcard = raw.endsWith('*');
  const withoutWildcard = wildcard ? raw.slice(0, -1) : raw;
  const parts = cleanPathSegments(withoutWildcard);
  const [bucket = '', ...prefixParts] = parts;

  return {
    bucket,
    prefix: prefixParts.length === 0 ? '' : `${prefixParts.join('/')}/`,
    wildcard,
    global: false,
  };
}

export function formatResourcePattern(bucket: string, prefix: string, wildcard = true): string {
  const cleanBucket = bucket.trim();
  if (!cleanBucket) return wildcard ? '*' : '';

  const cleanPrefix = normalizePrefix(prefix);
  if (!cleanPrefix) return wildcard ? `${cleanBucket}/*` : cleanBucket;

  const base = `${cleanBucket}/${cleanPrefix.replace(/\/$/, '')}`;
  return wildcard ? `${base}/*` : base;
}

export function normalizeResourcePattern(value: string | null | undefined): string {
  const raw = (value || '').trim();
  if (!raw || raw === '*') return raw;

  const wildcard = raw.endsWith('*');
  const withoutWildcard = wildcard ? raw.slice(0, -1) : raw;
  const compactWildcard = wildcard && withoutWildcard.includes('/') && !withoutWildcard.endsWith('/');
  const parts = parseResourcePattern(raw);
  if (!parts.bucket) return raw;
  if (compactWildcard) {
    const prefix = parts.prefix.replace(/\/$/, '');
    return prefix ? `${parts.bucket}/${prefix}*` : `${parts.bucket}/*`;
  }
  return formatResourcePattern(parts.bucket, parts.prefix, parts.wildcard);
}
