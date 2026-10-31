/** src/deltaSummary.ts objectStorageHeadline: the inspector's storage card (B9). */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { objectStorageHeadline, storageChip } from '../deltaSummary';

test('the baseline is "Stored whole" with a Baseline chip, never "0.0%" and "Delta"', () => {
  const h = objectStorageHeadline({ storageType: 'delta', isBaseline: true, savingsPct: 0, savedBytesText: '0 B' });
  assert.deepEqual(h, {
    kind: 'baseline',
    title: 'Stored whole',
    detail: "Reference for this folder's deltas",
    chip: 'Baseline',
  });
});

test('a delta shows its savings and the Delta chip', () => {
  const h = objectStorageHeadline({ storageType: 'delta', isBaseline: false, savingsPct: 98.25, savedBytesText: '1.8 MiB' });
  assert.deepEqual(h, { kind: 'savings', title: '98.3%', detail: '1.8 MiB saved', chip: 'Delta' });
  assert.equal(objectStorageHeadline({ storageType: undefined, isBaseline: false, savingsPct: 0, savedBytesText: '0 B' }).chip, 'Original');
});

// UX harvest C4: the table chip and the inspector chip come from one rule.
test('storageChip: a baseline is a neutral "Baseline", never "Delta"', () => {
  assert.deepEqual(
    [storageChip('delta', true), storageChip('delta', false), storageChip('passthrough', false), storageChip(undefined, false)]
      .map((c) => [c.label, c.tone]),
    [['Baseline', 'neutral'], ['Delta', 'type'], ['Original', 'type'], ['—', 'neutral']],
  );
});
