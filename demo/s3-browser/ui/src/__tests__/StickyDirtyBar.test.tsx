/**
 * A hidden dirty bar stays mounted for its fade, but must not leave a ghost
 * "Review & apply" on the page: on Buckets the object-size card's hidden bar
 * sat over the content, a click reached it, and it opened a 0-change dialog
 * for the `advanced` section (UX harvest B1/B2). A floating bar reserves its
 * height while it shows, so it never covers the last card.
 */
import { render, screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import StickyDirtyBar from '../components/StickyDirtyBar';

const noop = () => {};

test.each([
  ['sticky', {}],
  ['floating', { floating: true }],
  ['inline', { inline: true }],
])('%s: a hidden bar is visibility:hidden, so nothing can hit its buttons', (_mode, props) => {
  render(<StickyDirtyBar visible={false} applying={false} onDiscard={noop} onApply={noop} {...props} />);
  const button = screen.getByText('Review & apply').closest('button')!;
  expect(getComputedStyle(button).visibility).toBe('hidden');
});

test('a shown bar is visible', () => {
  render(<StickyDirtyBar visible applying={false} onDiscard={noop} onApply={noop} floating />);
  const button = screen.getByRole('button', { name: 'Review & apply' });
  expect(getComputedStyle(button).visibility).toBe('visible');
});

test('a floating bar reserves its height only while it shows', () => {
  const { rerender } = render(
    <StickyDirtyBar visible={false} applying={false} onDiscard={noop} onApply={noop} floating />,
  );
  expect(screen.queryByTestId('dirty-bar-spacer')).toBeNull();
  rerender(<StickyDirtyBar visible applying={false} onDiscard={noop} onApply={noop} floating />);
  expect(screen.getByTestId('dirty-bar-spacer')).toBeInTheDocument();
});
