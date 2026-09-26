/**
 * PermissionEditor WHERE field: the resource suggestion chips.
 *
 * Pins 9c765d77: the chips used to render only while the WHERE input had
 * focus. On blur they vanished and CAN DO moved up about 60 px, so a click
 * aimed at "Write" right after typing landed on nothing. Now the chips stay
 * rendered after blur, with the same content, and act on the last focused row.
 *
 * Bucket names come from the S3 SDK (not admin fetch), so `s3client` is
 * stubbed at its two listing calls.
 */
import { act, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import PermissionEditor from '../components/PermissionEditor';
import type { PermissionRow } from '../components/permissionRows';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    listBuckets: vi.fn(async () => [{ name: 'releases' }, { name: 'db-archive' }]),
    listCommonPrefixes: vi.fn(async (bucket: string) => (bucket === 'releases' ? ['builds/', 'nightly/'] : [])),
  };
});

function Harness({ initial, onRows }: { initial: PermissionRow[]; onRows: (rows: PermissionRow[]) => void }) {
  const [rows, setRows] = useState(initial);
  return (
    <PermissionEditor
      permissions={rows}
      onChange={(next) => {
        setRows(next);
        onRows(next);
      }}
    />
  );
}

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
});
afterEach(() => {
  vi.useRealTimers();
});

function setup(initial: PermissionRow[] = [{ _uiId: 'p1', effect: 'Allow', actions: [], resources: [] }]) {
  const onRows = vi.fn();
  renderWithQuery(<Harness initial={initial} onRows={onRows} />);
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  return { onRows, user };
}

/** Let the 150 ms blur timer and the 200 ms prefix-listing debounce fire. */
async function settle() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(400);
  });
}

const whereInput = () => screen.getByPlaceholderText('my-bucket/builds/*');
const chip = (pattern: string) => screen.queryByRole('button', { name: pattern });

describe('WHERE suggestion chips', () => {
  test('are offered for the listed buckets before the field was ever focused', async () => {
    setup();
    expect(await screen.findByRole('button', { name: 'releases/*' })).toBeInTheDocument();
    expect(chip('db-archive/*')).toBeInTheDocument();
    expect(chip('*')).toBeInTheDocument();
  });

  test('stay rendered with the same content after the field loses focus', async () => {
    const { user } = setup();
    await screen.findByRole('button', { name: 'releases/*' });
    await user.type(whereInput(), 'releases/');
    await settle();
    // Typing a known bucket lists its prefixes as chips.
    expect(chip('releases/builds/*')).toBeInTheDocument();
    const chipsFocused = chipLabels();
    expect(chipsFocused).toContain('releases/nightly/*');

    // Leave the field, then wait past the 150 ms blur timer.
    await user.click(screen.getByText('WHERE'));
    await settle();
    expect(whereInput()).not.toHaveFocus();

    expect(chipLabels()).toEqual(chipsFocused);
    // CAN DO is still there, next to the chips.
    expect(screen.getByRole('checkbox', { name: 'Write' })).toBeInTheDocument();
  });

  test('a chip clicked after blur fills the last focused row', async () => {
    const { user, onRows } = setup();
    await screen.findByRole('button', { name: 'releases/*' });
    await user.type(whereInput(), 'releases/');
    await settle();
    await user.click(screen.getByText('WHERE'));
    await settle();

    await user.click(screen.getByRole('button', { name: 'releases/nightly/*' }));
    expect(whereInput()).toHaveValue('releases/nightly/*');
    expect(onRows.mock.lastCall?.[0]).toEqual([
      expect.objectContaining({ _uiId: 'p1', resources: ['releases/nightly/*'] }),
    ]);
  });

  test('typing a resource then switching Write on records both on the same row', async () => {
    const { user, onRows } = setup();
    await screen.findByRole('button', { name: 'releases/*' });
    await user.type(whereInput(), 'releases/*');
    await user.click(screen.getByRole('checkbox', { name: 'Write' }));
    await settle();
    const last = onRows.mock.lastCall?.[0] as PermissionRow[];
    expect(last[0].resources).toEqual(['releases/*']);
    expect(last[0].actions).toContain('write');
  });
});

/** Visible chip texts, in order (the suggestion strip under Add resource). */
function chipLabels(): string[] {
  const add = screen.getByRole('button', { name: /Add resource/ });
  const strip = add.nextElementSibling as HTMLElement | null;
  if (!strip || strip.tagName !== 'DIV' || within(strip).queryAllByRole('button').length === 0) return [];
  return within(strip)
    .getAllByRole('button')
    .map((b) => b.textContent ?? '');
}
