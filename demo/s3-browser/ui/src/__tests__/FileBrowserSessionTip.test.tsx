/**
 * Browser-review item 19: the "Signed in for files only" banner. Its
 * dismissal is remembered PER USER (a shared browser keeps showing it to the
 * next user), and on a phone it is one short line, not a screen-high card.
 */
import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test } from 'vitest';
import FileBrowserSessionTip from '../components/FileBrowserSessionTip';

const setWidth = (w: number) => {
  Object.defineProperty(window, 'innerWidth', { configurable: true, value: w });
  window.dispatchEvent(new Event('resize'));
};
afterEach(() => {
  setWidth(1024);
  window.localStorage.clear();
});

test('dismissal is remembered for that user only', async () => {
  const user = userEvent.setup();
  const { rerender } = render(<FileBrowserSessionTip visible userKey="dana" />);
  expect(screen.getByText('Signed in for files only')).toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: 'Dismiss this tip' }));
  expect(screen.queryByText('Signed in for files only')).not.toBeInTheDocument();
  rerender(<FileBrowserSessionTip visible userKey="ci-uploader" />);
  expect(screen.getByText('Signed in for files only')).toBeInTheDocument();
  rerender(<FileBrowserSessionTip visible userKey="dana" />);
  expect(screen.queryByText('Signed in for files only')).not.toBeInTheDocument();
});

test('on a phone the banner is compact', () => {
  act(() => setWidth(390));
  render(<FileBrowserSessionTip visible userKey="dana" />);
  expect(screen.getByText('Signed in for files only')).toBeInTheDocument();
  expect(screen.queryByText(/For bulk actions, folder sizes/)).not.toBeInTheDocument();
});
