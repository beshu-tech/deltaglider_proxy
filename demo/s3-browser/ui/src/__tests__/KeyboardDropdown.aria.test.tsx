/**
 * A dropdown trigger is a menu button: it says so (aria-haspopup) and says
 * whether the menu is open (aria-expanded). AntD sets neither.
 */
import { fireEvent, render, screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import KeyboardDropdown from '../components/KeyboardDropdown';

test('the trigger is announced as a menu button and tracks open state', () => {
  render(
    <KeyboardDropdown menu={{ items: [{ key: 'a', label: 'Rename' }] }}>
      <button type="button">Actions</button>
    </KeyboardDropdown>,
  );
  const trigger = screen.getByRole('button', { name: 'Actions' });
  expect(trigger).toHaveAttribute('aria-haspopup', 'menu');
  expect(trigger).toHaveAttribute('aria-expanded', 'false');
  fireEvent.click(trigger);
  expect(trigger).toHaveAttribute('aria-expanded', 'true');
});

test('a controlled dropdown reports its open prop', () => {
  render(
    <KeyboardDropdown open menu={{ items: [{ key: 'a', label: 'Rename' }] }}>
      <button type="button">New job</button>
    </KeyboardDropdown>,
  );
  expect(screen.getByRole('button', { name: 'New job' })).toHaveAttribute('aria-expanded', 'true');
});
