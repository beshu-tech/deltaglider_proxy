/**
 * UX audit P2.4: sidebar entries were <button>s, so middle-click and
 * "open in new tab" did nothing. They are links now; a plain click still
 * goes through the app router.
 */
import { fireEvent, render, screen, within } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import AdminSidebar from '../components/AdminSidebar';

function setup() {
  const onNavigate = vi.fn();
  render(<AdminSidebar activePath="access/users" onNavigate={onNavigate} />);
  const nav = screen.getByRole('navigation', { name: 'Admin navigation' });
  return { onNavigate, nav };
}

test('every entry is a link with the admin URL', () => {
  const { nav } = setup();
  const users = within(nav).getByRole('link', { name: /Users$/ });
  expect(users.getAttribute('href')).toBe('/_/admin/access/users');
  expect(users.getAttribute('aria-current')).toBe('page');
  expect(within(nav).queryAllByRole('button')).toHaveLength(0);
});

test('a plain click navigates in the app; a modified click is left to the browser', () => {
  const { onNavigate, nav } = setup();
  const groups = within(nav).getByRole('link', { name: /Groups$/ });
  const plain = fireEvent.click(groups);
  expect(plain).toBe(false); // default prevented
  expect(onNavigate).toHaveBeenCalledWith('access/groups');
  onNavigate.mockClear();
  expect(fireEvent.click(groups, { ctrlKey: true })).toBe(true);
  expect(fireEvent.click(groups, { metaKey: true })).toBe(true);
  expect(onNavigate).not.toHaveBeenCalled();
});
