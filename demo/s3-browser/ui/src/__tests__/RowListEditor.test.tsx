/**
 * RowListEditor: rows are keyed and addressed by their own stable id, never
 * by array index. The bug class this guards: remove a middle row and the
 * uncontrolled state of the rows below it (typed text, focus) slides onto the
 * wrong row.
 */
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { describe, expect, test, vi } from 'vitest';
import RowListEditor from '../components/RowListEditor';

interface Row {
  id: string;
  name: string;
}

let nextId = 0;
const newRow = (): Row => ({ id: `r${++nextId}`, name: '' });

function Harness({ initial, onRows }: { initial: Row[]; onRows?: (rows: Row[]) => void }) {
  const [rows, setRows] = useState(initial);
  return (
    <RowListEditor<Row>
      items={rows}
      onChange={(next) => {
        setRows(next);
        onRows?.(next);
      }}
      newItem={newRow}
      addLabel="Add row"
      emptyHint={<span>No rows yet</span>}
      renderRow={(item, update, remove) => (
        <div role="group" aria-label={`row ${item.name}`}>
          {/* Controlled by the item. */}
          <input aria-label={`name ${item.id}`} value={item.name} onChange={(e) => update({ name: e.target.value })} />
          {/* UNcontrolled: its text lives in the DOM node, so it follows the
              React key. With index keys it would stick to the slot. */}
          <input aria-label={`note for ${item.name}`} defaultValue="" />
          <button type="button" onClick={remove}>
            Remove {item.name}
          </button>
        </div>
      )}
    />
  );
}

const seed = (): Row[] => [
  { id: 'a', name: 'alpha' },
  { id: 'b', name: 'beta' },
  { id: 'c', name: 'gamma' },
];

test('empty list shows the hint and the add button', () => {
  render(<Harness initial={[]} />);
  expect(screen.getByText('No rows yet')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: /Add row/ })).toBeInTheDocument();
});

test('add appends a fresh row from newItem()', async () => {
  const onRows = vi.fn();
  render(<Harness initial={seed()} onRows={onRows} />);
  await userEvent.setup().click(screen.getByRole('button', { name: /Add row/ }));
  const next = onRows.mock.lastCall?.[0] as Row[];
  expect(next).toHaveLength(4);
  expect(next.slice(0, 3).map((r) => r.id)).toEqual(['a', 'b', 'c']);
  expect(next[3].name).toBe('');
});

test('update patches the row by id and leaves the others untouched', async () => {
  const onRows = vi.fn();
  render(<Harness initial={seed()} onRows={onRows} />);
  const user = userEvent.setup();
  await user.type(screen.getByRole('textbox', { name: 'name b' }), '!');
  expect(onRows).toHaveBeenLastCalledWith([
    { id: 'a', name: 'alpha' },
    { id: 'b', name: 'beta!' },
    { id: 'c', name: 'gamma' },
  ]);
});

describe('removing a middle row', () => {
  test('drops exactly that row by id', async () => {
    const onRows = vi.fn();
    render(<Harness initial={seed()} onRows={onRows} />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Remove beta' }));
    expect(onRows).toHaveBeenLastCalledWith([
      { id: 'a', name: 'alpha' },
      { id: 'c', name: 'gamma' },
    ]);
    expect(screen.queryByRole('group', { name: 'row beta' })).not.toBeInTheDocument();
  });

  test('keeps the typed text of the other rows attached to the right rows', async () => {
    render(<Harness initial={seed()} />);
    const user = userEvent.setup();
    await user.type(screen.getByRole('textbox', { name: 'note for alpha' }), 'note-A');
    await user.type(screen.getByRole('textbox', { name: 'note for beta' }), 'note-B');
    await user.type(screen.getByRole('textbox', { name: 'note for gamma' }), 'note-C');

    await user.click(screen.getByRole('button', { name: 'Remove beta' }));

    const alpha = screen.getByRole('group', { name: 'row alpha' });
    const gamma = screen.getByRole('group', { name: 'row gamma' });
    expect(within(alpha).getByRole('textbox', { name: 'note for alpha' })).toHaveValue('note-A');
    // With index keys gamma would inherit beta's DOM node and show "note-B".
    expect(within(gamma).getByRole('textbox', { name: 'note for gamma' })).toHaveValue('note-C');
    expect(screen.queryByDisplayValue('note-B')).not.toBeInTheDocument();
  });
});
