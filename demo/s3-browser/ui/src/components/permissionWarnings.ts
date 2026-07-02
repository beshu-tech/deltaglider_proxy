import { parseResourcePattern } from '../storagePath';

/**
 * Pure, inline correctness warnings for a single permission rule.
 *
 * The S3 authorization model prefixes every request resource with
 * `<bucket>/…`, so a resource pattern whose bucket segment doesn't name a
 * real bucket can NEVER match — it silently grants nothing. This is the
 * single most common "my policy doesn't work" footgun (e.g. writing
 * `ror/lib/*` — bucket `ror` — when the data lives in `beshu/ror/libs/*`).
 *
 * These helpers surface that inline in the rule card so the operator sees
 * the mistake while editing, instead of discovering it via a 403 later.
 * No React/antd imports — pure and unit-testable.
 */

interface ResourceWarning {
  /** The offending resource pattern, verbatim. */
  resource: string;
  /** The bucket segment we parsed out of it. */
  bucket: string;
  /** A closest-match bucket suggestion, if one is obvious. */
  suggestion?: string;
}

/** Levenshtein distance, capped — only used to suggest a near-miss bucket. */
function editDistance(a: string, b: string): number {
  const m = a.length;
  const n = b.length;
  if (Math.abs(m - n) > 3) return 99;
  const dp = Array.from({ length: m + 1 }, () => new Array<number>(n + 1).fill(0));
  for (let i = 0; i <= m; i++) dp[i][0] = i;
  for (let j = 0; j <= n; j++) dp[0][j] = j;
  for (let i = 1; i <= m; i++) {
    for (let j = 1; j <= n; j++) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1;
      dp[i][j] = Math.min(dp[i - 1][j] + 1, dp[i][j - 1] + 1, dp[i - 1][j - 1] + cost);
    }
  }
  return dp[m][n];
}

/** Closest known bucket within a small edit distance, or undefined. */
function nearestBucket(bucket: string, knownBuckets: string[]): string | undefined {
  let best: string | undefined;
  let bestDist = 3; // only suggest a genuinely-close miss
  for (const known of knownBuckets) {
    const d = editDistance(bucket, known);
    if (d < bestDist) {
      bestDist = d;
      best = known;
    }
  }
  return best;
}

/**
 * Return one warning per resource pattern that targets a bucket NOT in
 * `knownBuckets`. Wildcard-only (`*`) and template-bucket (`${…}`) patterns
 * are skipped — they're intentionally not a concrete bucket. When the known
 * list is empty (buckets still loading / unavailable) no warnings are emitted,
 * to avoid false positives.
 */
export function unknownBucketWarnings(
  resources: string[],
  knownBuckets: string[],
): ResourceWarning[] {
  if (knownBuckets.length === 0) return [];
  const known = new Set(knownBuckets);
  const out: ResourceWarning[] = [];
  const seen = new Set<string>();
  for (const part of resources) {
    const trimmed = part.trim();
    if (!trimmed || trimmed === '*') continue;
    const { bucket, global } = parseResourcePattern(trimmed);
    if (global || !bucket) continue; // `*` / unparseable — not a concrete bucket
    if (bucket.includes('${')) continue; // template bucket — can't validate
    if (known.has(bucket)) continue;
    if (seen.has(bucket)) continue;
    seen.add(bucket);
    out.push({ resource: trimmed, bucket, suggestion: nearestBucket(bucket, knownBuckets) });
  }
  return out;
}

/** True if `s` contains any whitespace or control character. */
function hasWhitespaceOrControl(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    const code = s.charCodeAt(i);
    // C0 controls (incl. tab/newline), space, and DEL.
    if (code <= 0x20 || code === 0x7f) return true;
  }
  return false;
}

/**
 * Flag resource patterns the BACKEND `validate_permissions` rejects, so the
 * operator sees the problem inline instead of a bare HTTP 400 on Apply. Mirrors
 * the server rules (src/iam/permissions.rs): only a TRAILING `*` is allowed, and
 * a pattern must not contain whitespace or control characters. Hyphens, dots,
 * and slashes are fine. Returns one human-readable reason per offending pattern
 * (empty list = all valid).
 */
export function invalidPatternWarnings(resources: string[]): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const part of resources) {
    const trimmed = part.trim();
    if (!trimmed || seen.has(trimmed)) continue;
    seen.add(trimmed);
    if (hasWhitespaceOrControl(trimmed)) {
      out.push(`"${trimmed}" contains a space or control character — patterns can't have them.`);
      continue;
    }
    const star = trimmed.indexOf('*');
    if (star !== -1 && star !== trimmed.length - 1) {
      out.push(`"${trimmed}" has "*" mid-pattern — only a trailing "*" is allowed (e.g. bucket/prefix/*).`);
    }
  }
  return out;
}
