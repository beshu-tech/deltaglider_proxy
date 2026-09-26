/**
 * Browser-review item 23: window.confirm blocks the page, ignores the theme,
 * cannot be styled or focus-managed, and some browsers suppress it. Every
 * confirmation is an AntD dialog now (confirmDialog); noWindowConfirm.test.ts
 * keeps window.confirm out.
 */
import { renderHook, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test } from 'vitest';
import { confirmDialog } from '../confirmDialog';
import { confirmDiscardEdits, useDirtyFlag } from '../useDirtyFlag';

test('confirmDialog resolves true on OK and false on Cancel', async () => {
  const user = userEvent.setup();
  const yes = confirmDialog({ title: 'Delete group "Engineering"?', okText: 'Delete', danger: true });
  await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Delete' }));
  expect(await yes).toBe(true);
  await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  const no = confirmDialog({ title: 'Sign out?' });
  await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Cancel' }));
  expect(await no).toBe(false);
});

test('confirmDiscardEdits asks in a dialog only when the key is dirty', async () => {
  expect(await confirmDiscardEdits('iam/users-test')).toBe(true);
  const dirty = renderHook(() => useDirtyFlag('iam/users-test', true));
  const user = userEvent.setup();
  const answer = confirmDiscardEdits('iam/users-test');
  const dialog = await screen.findByRole('dialog');
  expect(within(dialog).getAllByText('Discard your unsaved changes?').length).toBeGreaterThan(0);
  await user.click(within(dialog).getByRole('button', { name: 'Discard changes' }));
  expect(await answer).toBe(true);
  dirty.unmount();
});
