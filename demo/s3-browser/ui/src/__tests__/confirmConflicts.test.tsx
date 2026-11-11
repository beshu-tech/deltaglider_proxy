import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Modal } from 'antd';
import { afterEach, expect, test } from 'vitest';
import { renderWithQuery } from '../test/render';
import { confirmConflicts } from '../components/confirmConflicts';
import type { TransferConflicts } from '../bulkConflicts';

const c: TransferConflicts = {
  action: 'copy',
  destBucket: 'archive',
  total: 2,
  conflicts: [{ source: 'a/x', relative: 'x', dest: 'out/x' }],
};

afterEach(() => Modal.destroyAll());

test.each([
  ['Skip existing', 'skip'],
  ['Overwrite', 'overwrite'],
  ['Cancel', 'cancel'],
] as const)('the %s button answers %s', async (name, answer) => {
  renderWithQuery(<div />);
  const asked = confirmConflicts(c);
  expect((await screen.findAllByText('1 object already exists at the destination')).length).toBeGreaterThan(0);
  await userEvent.click(screen.getByRole('button', { name }));
  await expect(asked).resolves.toBe(answer);
});

test('Escape answers cancel', async () => {
  renderWithQuery(<div />);
  const asked = confirmConflicts(c);
  await screen.findAllByText('1 object already exists at the destination');
  await userEvent.keyboard('{Escape}');
  await expect(asked).resolves.toBe('cancel');
});
