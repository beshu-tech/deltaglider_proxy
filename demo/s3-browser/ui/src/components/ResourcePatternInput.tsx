import { DeleteOutlined, PlusOutlined } from '@ant-design/icons';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Button } from 'antd';
import { listCommonPrefixes } from '../s3client';
import {
  formatResourcePattern,
  parseResourcePattern,
} from '../storagePath';
import {
  freshResourceRowId,
  normalizeResourceRowPattern,
  parseResourceRows,
  serializeResourceRows,
  type ResourceRow,
} from '../resourcePatternRows';
import { useColors } from '../ThemeContext';
import SimpleAutoComplete, { type AutoCompleteEntry, type AutoCompleteGroup } from './SimpleAutoComplete';


interface ResourcePatternInputProps {
  /**
   * Resource patterns as a STRING ARRAY (the server model). A pattern may
   * contain a literal comma — it is NEVER used as a delimiter here.
   */
  value: string[];
  onChange: (value: string[]) => void;
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

export default function ResourcePatternInput({ value, onChange, buckets = [], style }: ResourcePatternInputProps) {
  const colors = useColors();
  const [prefixOptions, setPrefixOptions] = useState<string[]>([]);
  const [focusedId, setFocusedId] = useState<string | null>(null);

  // Local editing state is the single source of truth WHILE editing. The
  // `value` prop only seeds it, and only on a genuine external change (not our
  // own echo). Mirrors ConditionPrefixInput — the sanctioned row-editor model.
  const initial = useMemo(() => parseResourceRows(value), [value]);
  const [rows, setRows] = useState<ResourceRow[]>(() => initial);
  const rowsRef = useRef<ResourceRow[]>(rows);
  rowsRef.current = rows;
  const lastEmitted = useRef<string>(JSON.stringify(serializeResourceRows(rows)));

  useEffect(() => {
    const incoming = JSON.stringify(value);
    if (incoming === lastEmitted.current) return; // our own echo — ignore
    lastEmitted.current = incoming;
    const seeded = parseResourceRows(value);
    rowsRef.current = seeded;
    setRows(seeded);
  }, [value]);

  // Emit the current rows upward as a string[]. Reads the live ref, never a
  // closed-over snapshot (the staleness class we're killing).
  const emitNow = () => {
    const serialized = serializeResourceRows(rowsRef.current);
    const key = JSON.stringify(serialized);
    if (key !== lastEmitted.current) {
      lastEmitted.current = key;
      onChange(serialized);
    }
  };

  const emit = (mutate: (current: ResourceRow[]) => ResourceRow[]) => {
    const next = mutate(rowsRef.current);
    rowsRef.current = next;
    setRows(next);
    emitNow();
  };

  const focusedRow = focusedId === null ? null : rows.find((r) => r.id === focusedId) || null;
  const activeValue = focusedRow?.text || '';
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

  const updateRow = (id: string, nextValue: string) => {
    emit((current) =>
      current.map((row) => (row.id === id ? { ...row, text: nextValue.replace(/\r?\n/g, ' ') } : row)),
    );
  };

  const addRow = () => {
    // New empty row lives in LOCAL state only (an empty row contributes nothing
    // to the persisted array); it becomes persistable once the user types.
    const next = [...rowsRef.current, { id: freshResourceRowId(), text: '' }];
    rowsRef.current = next;
    setRows(next);
  };

  const deleteRow = (id: string) => {
    emit((current) => {
      const remaining = current.filter((row) => row.id !== id);
      return remaining.length > 0 ? remaining : [{ id: freshResourceRowId(), text: '' }];
    });
    setFocusedId((current) => (current === id ? null : current));
  };

  const applySuggestion = (pattern: string) => {
    if (focusedId === null) return;
    updateRow(focusedId, pattern);
  };

  // Normalize ONLY the blurred row's text, in local state — no reparse of a
  // comma string, no stale closure over the prop (mirrors ConditionPrefixInput).
  const normalizeRowOnBlur = (id: string) => {
    setFocusedId(null);
    emit((current) =>
      current.map((row) => {
        if (row.id !== id) return row;
        if (!row.text.trim()) return row; // empty stays empty
        return { ...row, text: normalizeResourceRowPattern(row.text) };
      }),
    );
  };

  return (
    <div style={{ width: '100%' }}>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 6, marginTop: style?.marginTop }}>
        {rows.map((row) => (
          <div key={row.id} style={{ display: 'flex', gap: 6, alignItems: 'center', width: '100%' }}>
            <div style={{ flex: 1, minWidth: 0 }} onFocusCapture={() => setFocusedId(row.id)}>
              <SimpleAutoComplete
                value={row.text}
                filterText={row.text}
                autoComplete={`dgp-resource-${row.id}`}
                onChange={(v) => updateRow(row.id, v)}
                onBlur={() => normalizeRowOnBlur(row.id)}
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
                onClick={() => deleteRow(row.id)}
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
      {focusedId !== null && (
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
