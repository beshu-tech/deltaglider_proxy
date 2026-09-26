/**
 * Browser-review item 23 (axe nested-interactive): a Users/Groups list row
 * was role="button" and held the Duplicate button, so screen readers could
 * not reach the inner button and the row's name swallowed it. Row actions
 * now sit next to the row button, not inside it, and each has a unique name.
 */
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import { Button } from 'antd';
import MasterDetailPanel from '../components/MasterDetailPanel';

test('row actions are not inside the row button', async () => {
  const onSelect = vi.fn();
  const onDup = vi.fn();
  const user = userEvent.setup();
  render(
    <MasterDetailPanel<{ id: number; name: string }>
      title="Users" searchPlaceholder="Search" items={[{ id: 1, name: 'dana' }, { id: 2, name: 'backup-bot' }]}
      getId={(u) => u.id} isSelected={() => false} onSelect={onSelect} rowPadding="12px"
      onCreate={() => {}} search="" onSearchChange={() => {}} loading={false} error=""
      listEmptyState={null} detail={null}
      renderRowBody={(u) => <span>{u.name}</span>}
      renderRowActions={(u) => <Button aria-label={`Duplicate ${u.name}`} onClick={() => onDup(u.id)}>dup</Button>}
    />,
  );
  const row = screen.getByRole('button', { name: 'dana' });
  expect(within(row).queryByRole('button')).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Duplicate dana' }));
  expect(onDup).toHaveBeenCalledWith(1);
  expect(onSelect).not.toHaveBeenCalled();
  await user.click(row);
  expect(onSelect).toHaveBeenCalledTimes(1);
});
