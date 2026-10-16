/**
 * A draft rule (added with "New replication rule", never applied) can be removed on its
 * own, from the row's "⋯" menu or the drawer's. Before, a draft row had no
 * actions: the only way out was Discard, which also dropped every other
 * unsaved edit.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { ReplicationRulesPanel } from '../components/jobs/RulesPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
});
afterEach(() => vi.unstubAllGlobals());

/** Add a replication draft; returns its name as the table shows it. */
async function addDraft(user: ReturnType<typeof userEvent.setup>): Promise<string> {
  await user.click(await screen.findByRole('button', { name: /New replication rule/ }));
  const tag = await screen.findAllByText('draft');
  const row = tag.map((t) => t.closest('[role="row"]')).find(Boolean) as HTMLElement;
  return within(row).getAllByText(/\S/)[1].textContent ?? '';
}

function draftRows(): HTMLElement[] {
  return screen
    .queryAllByText('draft')
    .map((t) => t.closest('[role="row"]'))
    .filter(Boolean) as HTMLElement[];
}

/** The dirty bar's Apply button, while the bar shows. */
function applyButton(): HTMLElement | null {
  return screen.queryByRole('button', { name: 'Review & apply' });
}

async function confirmRemove(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole('menuitem', { name: /Remove draft/ }));
  // The menu's confirmation (Modal.confirm): its OK button names the action.
  await screen.findAllByText(/^Remove the draft /);
  const ok = await screen.findAllByRole('button', { name: 'Remove draft' });
  await user.click(ok[ok.length - 1]);
}

test('the row menu removes one draft and keeps the other unsaved edits', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ReplicationRulesPanel />);
  await addDraft(user);
  await user.keyboard('{Escape}'); // close the drawer the new rule opened
  await addDraft(user);
  await user.keyboard('{Escape}');
  await waitFor(() => expect(draftRows()).toHaveLength(2));

  const [first] = draftRows();
  await user.click(within(first).getByRole('button', { name: /^More actions for / }));
  await confirmRemove(user);

  await waitFor(() => expect(draftRows()).toHaveLength(1));
  // The other draft is still an unsaved edit.
  expect(applyButton()).not.toBeNull();
});

test('removing the only draft clears the unsaved-changes bar; the drawer offers it too', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ReplicationRulesPanel />);
  await addDraft(user);
  // The drawer of the new draft is open: its "⋯" removes it and closes.
  const drawer = await screen.findByRole('dialog');
  await user.click(within(drawer).getByRole('button', { name: /^More actions for / }));
  await confirmRemove(user);

  await waitFor(() => expect(draftRows()).toHaveLength(0));
  // The bar stays mounted for its fade, hidden from the accessibility tree.
  await waitFor(() => expect(applyButton()).toBeNull());
});
