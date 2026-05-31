/**
 * Pure condition (de)serialization for PermissionEditor.
 *
 * IAM permission conditions are stored as
 * `{ operator: { key: string | string[] } }`. Multi-value operators (e.g.
 * `IpAddress` on `aws:SourceIp`) accept a comma-separated input string. The
 * serializer must NEVER persist empty fragments into the array — a trailing
 * comma like "192.168.0.0/16, " previously produced `['192.168.0.0/16', '']`,
 * leaking a bogus empty CIDR into config. These helpers filter empties and
 * coalesce a single survivor to a scalar, mirroring conditionPrefixRows.ts's
 * serializeRows + rowsToPermissions cleanup. No React/antd imports — pure and
 * unit-testable.
 */

type Conditions = Record<string, Record<string, string | string[]>>;

/** Extract a simple condition value for UI display. */
export function getConditionValue(
  conditions: Conditions | undefined,
  operator: string,
  key: string,
): string {
  if (!conditions) return '';
  const opBlock = conditions[operator];
  if (!opBlock) return '';
  const val = opBlock[key];
  if (Array.isArray(val)) return val.join(', ');
  return val || '';
}

/**
 * Extract a condition value as a STRING ARRAY, preserving every entry —
 * including the empty string `""`, which for `s3:prefix` means "list from
 * the bucket root". Unlike {@link getConditionValue} (a comma-join that the
 * comma-split round-trip silently collapses), this is lossless: it's the
 * read side of the array contract used by the multi-row prefix editor.
 */
export function getConditionArray(
  conditions: Conditions | undefined,
  operator: string,
  key: string,
): string[] {
  if (!conditions) return [];
  const opBlock = conditions[operator];
  if (!opBlock) return [];
  const val = opBlock[key];
  if (Array.isArray(val)) return [...val];
  if (val === undefined || val === null) return [];
  // A scalar (incl. "") is a single-entry list.
  return [val];
}

/**
 * Set a condition from a STRING ARRAY, preserving `""` entries (root prefix).
 *
 * The only entries dropped are pure-whitespace ones that are NOT the empty
 * string — i.e. genuine blank rows the user never filled. `""` itself is
 * KEPT because it is a meaningful s3:prefix value. A single survivor is left
 * as an array (not coalesced to a scalar) so the wire shape is stable and an
 * all-empty list removes the condition entirely.
 */
export function setConditionArray(
  conditions: Conditions | undefined,
  operator: string,
  key: string,
  values: string[],
): Conditions {
  const result = conditions ? { ...conditions } : {};
  // Keep the literal empty string "" (root prefix). For everything else, keep
  // the trimmed value if it's non-empty — so a " " whitespace-only row is
  // dropped as noise but a real "" is preserved. De-dupe, order-preserving.
  const seen = new Set<string>();
  const cleaned: string[] = [];
  for (const raw of values) {
    const v = raw === '' ? '' : raw.trim();
    if (raw !== '' && v === '') continue; // whitespace-only row → noise
    if (seen.has(v)) continue;
    seen.add(v);
    cleaned.push(v);
  }
  if (cleaned.length === 0) {
    return removeConditionKey(result, operator, key);
  }
  result[operator] = { ...(result[operator] || {}), [key]: cleaned };
  return result;
}

/** Remove operator/key from the conditions map, pruning empty operator blocks. */
function removeConditionKey(conditions: Conditions, operator: string, key: string): Conditions {
  const result = { ...conditions };
  if (result[operator]) {
    const { [key]: _removed, ...rest } = result[operator];
    if (Object.keys(rest).length === 0) {
      delete result[operator];
    } else {
      result[operator] = rest;
    }
  }
  return result;
}

/** Set a condition value, creating operator/key structure as needed. */
export function setConditionValue(
  conditions: Conditions | undefined,
  operator: string,
  key: string,
  value: string,
): Conditions {
  const result = conditions ? { ...conditions } : {};
  if (!value.trim()) {
    return removeConditionKey(result, operator, key);
  }
  // Drop empty fragments so a trailing comma can never persist '' into the
  // array. A single survivor coalesces to a scalar string for shape-consistency
  // with the single-value path.
  const parts = value
    .split(',')
    .map(v => v.trim())
    .filter(Boolean);
  if (parts.length === 0) {
    return removeConditionKey(result, operator, key);
  }
  const parsedValue: string | string[] = parts.length === 1 ? parts[0] : parts;
  result[operator] = { ...(result[operator] || {}), [key]: parsedValue };
  return result;
}

/** Check if a rule has any conditions set. */
export function hasConditions(conditions?: Conditions): boolean {
  if (!conditions) return false;
  return Object.values(conditions).some(kv => Object.values(kv).some(v =>
    typeof v === 'string' ? v.trim() !== '' : v.length > 0
  ));
}
