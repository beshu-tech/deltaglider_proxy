/**
 * PrefixListEditor — the specific-prefixes editor surfaced inside a
 * {@link BucketCard} when its anonymous-read mode is `prefixes`.
 *
 * Originally extracted verbatim from BucketsPanel's inline `prefixes` block;
 * now a thin wrapper over the shared {@link RowListEditor} so the id discipline
 * (stable id, mutate-by-id, never array index) lives in one place. The
 * trailing-slash normalisation on blur is unchanged.
 *
 * Public API still takes a FUNCTIONAL `onPrefixesChange` (the parent applies the
 * transform via its functional setRows, never a stale closure). Internally that
 * is bridged to RowListEditor's `onChange(next)` by ignoring the previous value
 * and committing `next` directly — RowListEditor already computed it by id.
 */
import { Button, Input } from 'antd';
import { formRow } from './ruleEditorHelpers';
import type { PrefixEntry } from './bucketPolicyPayload';
import { freshId } from './bucketPolicyPayload';
import RowListEditor from './RowListEditor';

interface Props {
  prefixes: PrefixEntry[];
  /** Apply a functional transform to the prefix list, by id, via the
   *  parent's functional setRows — never a stale closure. */
  onPrefixesChange: (fn: (prev: PrefixEntry[]) => PrefixEntry[]) => void;
  inputRadius: { borderRadius: number };
}

export default function PrefixListEditor({
  prefixes,
  onPrefixesChange,
  inputRadius,
}: Props) {
  return (
    <RowListEditor<PrefixEntry>
      items={prefixes}
      onChange={(next) => onPrefixesChange(() => next)}
      newItem={() => ({ id: freshId(), value: '' })}
      addLabel="Add prefix"
      addButtonType="text"
      style={{ marginTop: 8, paddingLeft: 16, gap: 4 }}
      gap={4}
      addButtonStyle={{ padding: '0 8px', fontSize: 11 }}
      renderRow={(prefix, update, remove) => (
        <div style={formRow(4)}>
          <Input
            aria-label="Public prefix"
            value={prefix.value}
            onChange={(e) => update({ value: e.target.value })}
            onBlur={(e) => {
              const v = e.target.value.trim();
              if (v && !v.endsWith('/')) {
                update({ value: v + '/' });
              }
            }}
            placeholder="e.g. builds/"
            style={{
              flex: 1,
              ...inputRadius,
              fontFamily: 'var(--font-mono)',
              fontSize: 11,
            }}
            size="small"
          />
          <Button
            type="text"
            size="small"
            danger
            onClick={remove}
            style={{ padding: '0 8px', minWidth: 0 }}
          >
            ×
          </Button>
        </div>
      )}
    />
  );
}
