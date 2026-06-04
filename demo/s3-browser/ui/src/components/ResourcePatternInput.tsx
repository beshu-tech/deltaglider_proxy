import { DeleteOutlined, PlusOutlined } from '@ant-design/icons';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Button } from 'antd';
import { listCommonPrefixes } from '../s3client';
import {
  formatResourcePattern,
  normalizeResourcePattern,
  parseResourcePattern,
} from '../storagePath';
import { useColors } from '../ThemeContext';
import SimpleAutoComplete, { type AutoCompleteEntry, type AutoCompleteGroup } from './SimpleAutoComplete';


interface ResourcePatternInputProps {
  value: string;
  onChange: (value: string) => void;
  buckets?: string[];
  style?: React.CSSProperties;
}

function unique(values: string[]): string[] {
  return Array.from(new Set(values.filter(Boolean)));
}

function uniqueEntries(entries: AutoCompleteEntry[]): AutoCompleteEntry[] {
  const seen = new Set<string>();
  const out: AutoCompleteEntry[] = [];
  for (const e of entries) {
    if (seen.has(e.value)) continue;
    seen.add(e.value);
    out.push(e);
  }
  return out;
}

function splitRows(value: string): string[] {
  const rows = value.split(',').map((part) => part.trim());
  return rows.length > 0 ? rows : [''];
}

let resourceRowIdCounter = 0;
/** Monotonic, collision-free row id for stable React keys (never reused). */
function freshResourceRowId(): string {
  resourceRowIdCounter += 1;
  return `res-${resourceRowIdCounter}`;
}

function serializeRows(rows: string[]): string {
  if (rows.every((row) => !row.trim())) return rows.length > 1 ? rows.map(() => '').join(', ') : '';
  return rows.map((row) => row.trim()).join(', ');
}

export default function ResourcePatternInput({ value, onChange, buckets = [], style }: ResourcePatternInputProps) {
  const colors = useColors();
  const [prefixOptions, setPrefixOptions] = useState<string[]>([]);
  const [focusedIndex, setFocusedIndex] = useState<number | null>(null);
  const rows = useMemo(() => splitRows(value), [value]);

  // Stable per-row ids for React keys. Rows are derived from `value` (positional),
  // so we keep a parallel id list sized to the row count. The list is reconciled
  // by COUNT here (append/truncate), and the row-mutating handlers below splice it
  // in lockstep with the value mutation so a surviving row keeps its id through a
  // middle-row delete — avoiding the key={index} class of focus/IME misplacement.
  const idsRef = useRef<string[]>([]);
  if (idsRef.current.length !== rows.length) {
    const next = idsRef.current.slice(0, rows.length);
    while (next.length < rows.length) next.push(freshResourceRowId());
    idsRef.current = next;
  }
  const rowIds = idsRef.current;
  const activeValue = focusedIndex === null ? '' : rows[focusedIndex] || '';
  const activePattern = useMemo(() => parseResourcePattern(activeValue), [activeValue]);
  const knownBucket = activePattern.bucket && (buckets.includes(activePattern.bucket) || activeValue.includes('/'))
    ? activePattern.bucket
    : '';
  const variableSuggestions = useMemo(
    () =>
      knownBucket
        ? [
            formatResourcePattern(knownBucket, 'home/${iam:username}', true),
            formatResourcePattern(knownBucket, 'keys/${iam:access_key_id}', true),
          ]
        : ['my-bucket/home/${iam:username}/*', 'my-bucket/keys/${iam:access_key_id}/*'],
    [knownBucket],
  );

  const optionGroups = useMemo((): AutoCompleteGroup[] => {
    const groups: AutoCompleteGroup[] = [];

    if (knownBucket) {
      const inBucket: AutoCompleteEntry[] = [
        { value: formatResourcePattern(knownBucket, '', true), source: 'listed' },
        ...prefixOptions.map((prefix) => ({
          value: formatResourcePattern(knownBucket, prefix, true),
          source: 'listed' as const,
        })),
      ];
      const deduped = uniqueEntries(inBucket);
      if (deduped.length > 0) {
        groups.push({
          label: `Prefixes in “${knownBucket}”`,
          entries: deduped,
        });
      }
    }

    const otherBuckets = knownBucket ? buckets.filter((b) => b !== knownBucket) : buckets;
    if (otherBuckets.length > 0) {
      groups.push({
        label: knownBucket ? 'Other buckets' : 'Buckets',
        subtitle: knownBucket ? 'Root patterns for buckets other than the one in this field.' : undefined,
        entries: uniqueEntries(
          otherBuckets.map((b) => ({
            value: formatResourcePattern(b, '', true),
            source: 'listed' as const,
          })),
        ),
      });
    }

    groups.push({
      label: 'Variable patterns',
      subtitle:
        'Dimmed text is a placeholder filled in for each user (for example their name or access key). Those paths are not real folders—you will not see them when browsing the bucket.',
      entries: variableSuggestions.map((v) => ({
        value: v,
        source: 'template' as const,
        realPrefix: knownBucket ? `${knownBucket}/` : 'my-bucket/',
      })),
    });

    groups.push({
      label: 'Wildcard',
      entries: [{ value: '*', source: 'listed' }],
    });

    return groups;
  }, [buckets, knownBucket, prefixOptions, variableSuggestions]);

  const chipSuggestions = useMemo(
    () =>
      unique([
        ...prefixOptions.slice(0, 4).map((prefix) => formatResourcePattern(knownBucket, prefix, true)),
        ...buckets.slice(0, 4).map((bucket) => formatResourcePattern(bucket, '', true)),
        ...variableSuggestions,
        '*',
      ]).slice(0, 8),
    [buckets, knownBucket, prefixOptions, variableSuggestions],
  );
  const inputStyle: React.CSSProperties = {
    ...style,
    width: '100%',
    fontFamily: 'var(--font-mono)',
    fontSize: 12,
  };
  const chipStyle: React.CSSProperties = {
    minHeight: 24,
    height: 'auto',
    padding: '2px 8px',
    border: `1px solid ${colors.BORDER}`,
    borderRadius: 6,
    background: colors.BG_ELEVATED,
    color: colors.ACCENT_BLUE,
    fontFamily: 'var(--font-mono)',
    fontSize: 11,
    cursor: 'pointer',
    whiteSpace: 'normal',
    textAlign: 'left',
    lineHeight: 1.35,
  };

  useEffect(() => {
    let cancelled = false;
    if (!knownBucket) {
      setPrefixOptions([]);
      return;
    }

    const timer = window.setTimeout(() => {
      listCommonPrefixes(knownBucket, activePattern.prefix)
        .then((prefixes) => {
          if (!cancelled) setPrefixOptions(prefixes);
        })
        .catch(() => {
          if (!cancelled) setPrefixOptions([]);
        });
    }, 200);

    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [activePattern.prefix, knownBucket]);

  const updateRow = (index: number, nextValue: string) => {
    const nextRows = [...rows];
    nextRows[index] = nextValue.replace(/\r?\n/g, ' ');
    onChange(serializeRows(nextRows));
  };

  const addRow = () => {
    idsRef.current = [...idsRef.current, freshResourceRowId()];
    onChange(serializeRows([...rows, '']));
  };

  const deleteRow = (index: number) => {
    // Remove the id at the same index so the remaining rows keep their ids.
    idsRef.current = idsRef.current.filter((_, i) => i !== index);
    const nextRows = rows.filter((_, rowIndex) => rowIndex !== index);
    if (nextRows.length === 0) idsRef.current = [freshResourceRowId()];
    onChange(serializeRows(nextRows.length > 0 ? nextRows : ['']));
    setFocusedIndex((current) => {
      if (current === null) return null;
      if (current === index) return null;
      return current > index ? current - 1 : current;
    });
  };

  const applySuggestion = (pattern: string) => {
    if (focusedIndex === null) return;
    updateRow(focusedIndex, pattern);
  };

  // Normalize ONLY the blurred row's text, in place. Crucially this does NOT
  // re-split + filter the whole comma string (the old `normalizeList(value)`
  // path), which dropped in-progress empty rows and desynced `idsRef` —
  // reassigning React keys to surviving rows. Mirrors ConditionPrefixInput's
  // per-row blur. An empty row stays empty (it's serialized away only when a
  // sibling is non-empty, same as before, but the row COUNT is untouched here).
  const normalizeRowOnBlur = (index: number) => {
    setFocusedIndex(null);
    const current = rows[index] ?? '';
    if (!current.trim()) return; // empty row: nothing to normalize, keep as-is
    const normalized = normalizeResourcePattern(current);
    if (normalized !== current) updateRow(index, normalized);
  };

  return (
    <div style={{ width: '100%' }}>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 6, marginTop: style?.marginTop }}>
        {rows.map((row, index) => (
          <div key={rowIds[index] ?? `pending-${index}`} style={{ display: 'flex', gap: 6, alignItems: 'center', width: '100%' }}>
            <div style={{ flex: 1, minWidth: 0 }} onFocusCapture={() => setFocusedIndex(index)}>
              <SimpleAutoComplete
                value={row}
                filterText={row}
                autoComplete={`dgp-resource-${rowIds[index] ?? index}`}
                onChange={(v) => updateRow(index, v)}
                onBlur={() => normalizeRowOnBlur(index)}
                optionGroups={optionGroups}
                placeholder="my-bucket/builds/*"
                style={{ ...inputStyle, marginTop: 0 }}
              />
            </div>
            {rows.length > 1 && (
              <Button
                type="text"
                danger
                size="small"
                icon={<DeleteOutlined />}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => deleteRow(index)}
                style={{ flex: '0 0 auto' }}
              />
            )}
          </div>
        ))}
      </div>
      <Button
        type="dashed"
        size="small"
        icon={<PlusOutlined />}
        onMouseDown={(e) => e.preventDefault()}
        onClick={addRow}
        block
        style={{ marginTop: 6, borderRadius: 8 }}
      >
        Add resource
      </Button>
      {focusedIndex !== null && (
        <div style={{ marginTop: 8, display: 'flex', flexWrap: 'wrap', gap: 6, alignItems: 'center' }}>
          {chipSuggestions.map((pattern) => (
            <Button
              key={pattern}
              type="text"
              size="small"
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => applySuggestion(pattern)}
              style={chipStyle}
            >
              {pattern}
            </Button>
          ))}
        </div>
      )}
      <div style={{ fontSize: 11, color: colors.TEXT_MUTED, marginTop: 6, lineHeight: 1.45 }}>
        {knownBucket ? `Browsing prefixes in ${knownBucket}.` : buckets.length > 0 ? `${buckets.length} buckets available.` : 'Enter one resource pattern per row.'}
      </div>
    </div>
  );
}
