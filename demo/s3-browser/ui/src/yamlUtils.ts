/**
 * Pure helpers for reasoning about admin-API section YAML bodies.
 *
 * Extracted from CopySectionYamlButton so the logic can be unit-tested
 * without React (see src/__tests__/yamlUtils.test.ts).
 */

/** Strip full-line # comments (API bodies are comment-free; avoids double-detection if we re-fetch). */
function stripLeadingYamlCommentLines(s: string): string {
  return s
    .split('\n')
    .filter((line) => !/^\s*#/.test(line))
    .join('\n')
    .trim();
}

/**
 * True when the access section response is only an empty mapping.
 * Happens often: API responses use `redact_all_secrets()` (no SigV4 keys in YAML),
 * default `iam_mode: gui` is omitted by serde, and IAM directory state is DB-only.
 */
export function isRedactedEmptyAccessYaml(apiBody: string): boolean {
  const core = stripLeadingYamlCommentLines(apiBody);
  if (!core.startsWith('access:')) return false;
  const rest = core.slice('access:'.length).trim();
  if (rest === '') return true;
  if (rest === '{}' || /^\{\s*\}$/.test(rest)) return true;
  return false;
}
