/**
 * UX harvest D57: a tap on the avatar kept a focus ring, because opening the
 * menu moved focus by script. A pointer opens the menu and leaves focus
 * where it is; the keyboard opens it with focus on the first item (arrows).
 * D72: the menu carries no caveat paragraphs.
 */
import { fireEvent, render, screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import AccountMenu from '../components/AccountMenu';

function renderMenu() {
  render(
    <AccountMenu
      identityLabel="admin"
      onDocsClick={() => {}}
      onLogout={() => {}}
    />,
  );
  return screen.getByRole('button', { name: /account menu/i });
}

test('a pointer opens the menu without moving focus to an item', () => {
  const trigger = renderMenu();
  fireEvent.pointerDown(trigger);
  fireEvent.click(trigger);
  expect(screen.getByRole('menu')).toBeInTheDocument();
  expect(screen.getAllByRole('menuitem')).not.toContain(document.activeElement);
});

test('the keyboard opens the menu with focus on the first item', () => {
  const trigger = renderMenu();
  trigger.focus();
  fireEvent.keyDown(trigger, { key: 'ArrowDown' });
  expect(document.activeElement).toBe(screen.getAllByRole('menuitem')[0]);
});

test('the menu has no caveat prose', () => {
  const trigger = renderMenu();
  fireEvent.click(trigger);
  expect(screen.queryByText(/LIVE secrets|password file|does not include users/i)).toBeNull();
  expect(screen.getByRole('menuitem', { name: 'Sign out' })).not.toHaveClass('account-menu-item--danger');
});
