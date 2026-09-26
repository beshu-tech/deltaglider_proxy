/**
 * Browser-review item 23 (focus management): closing the object inspector
 * or the ⌘K palette dropped keyboard focus on <body>, so the next Tab
 * started from the top of the page. Focus now goes back to the element that
 * had it before the overlay opened, else to the main content region.
 */
import { act, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { expect, test } from 'vitest';
import { useRestoreFocus } from '../hooks/useRestoreFocus';

function Overlay({ open }: { open: boolean }) {
  useRestoreFocus(open);
  return open ? <input aria-label="inside" autoFocus /> : null;
}

function Harness() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <main id="main-content" tabIndex={-1} />
      <button onClick={() => setOpen(true)}>open</button>
      <button onClick={() => setOpen(false)}>close</button>
      <Overlay open={open} />
    </>
  );
}

const tick = () => act(async () => { await new Promise((r) => setTimeout(r, 20)); });

test('focus returns to the opener when the overlay closes', async () => {
  render(<Harness />);
  const opener = screen.getByRole('button', { name: 'open' });
  opener.focus();
  act(() => opener.click());
  await tick();
  expect(screen.getByLabelText('inside')).toHaveFocus();
  act(() => screen.getByRole('button', { name: 'close' }).click());
  await tick();
  expect(opener).toHaveFocus();
});

test('with no focused opener, focus goes to the main region', async () => {
  render(<Harness />);
  (document.activeElement as HTMLElement | null)?.blur();
  act(() => screen.getByRole('button', { name: 'open' }).click());
  await tick();
  act(() => screen.getByRole('button', { name: 'close' }).click());
  await tick();
  expect(document.getElementById('main-content')).toHaveFocus();
});
