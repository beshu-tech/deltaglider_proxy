/**
 * Review 2d, UI finding 1: the Jobs editor found a rule by its NAME. Typing
 * a new rule's name through an applied rule's name ("nightly" on the way to
 * "nightly-eu") moved the drawer onto the applied rule, and the next
 * keystrokes renamed IT, so Apply swapped the two definitions. A cleared
 * draft name hid the draft from the table and emptied the drawer. Each
 * editor rule now has a stable client-side uid: the drawer, the table and
 * "Remove draft" use it.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import { ReplicationRulesPanel } from '../components/jobs/RulesPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { qk } from '../queries/keys';

const row: JobRow = {
  id: 'replication:nightly', kind: 'replication', name: 'nightly',
  scope: { bucket: 'releases', target: 'downloads' }, trigger: 'continuous', enabled: true, paused: false,
  status: 'idle', status_raw: 'idle', progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 }, detail: {},
};
let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [row] }));
  http.on('GET', /\/_\/api\/admin\/jobs\/.*\/runs/, json({ runs: [] }));
  http.on('GET', /\/_\/api\/admin\/jobs\/.*\/failures/, json({ failures: [] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({
    replication: { enabled: true, rules: [{ name: 'nightly', enabled: true,
      source: { bucket: 'releases', prefix: '' }, destination: { bucket: 'downloads', prefix: '' } }] },
  }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
});
afterEach(() => vi.unstubAllGlobals());

/** The name field of the open drawer. */
async function nameInput(): Promise<HTMLInputElement> {
  return within(await screen.findByRole('dialog')).getAllByRole('textbox')[0] as HTMLInputElement;
}

/** Each table row as [name, tags] — tags are draft / edited / removing. */
function tableRows(): string[] {
  return screen
    .getAllByRole('row')
    .slice(1)
    .map((r) => r.textContent ?? '');
}

async function newReplicationDraft(user: ReturnType<typeof userEvent.setup>) {
  await screen.findAllByText('nightly');
  await user.click(await screen.findByRole('button', { name: /New replication rule/ }));
}

test('typing a new rule name through an applied name keeps the drawer on the new rule', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ReplicationRulesPanel />);
  await newReplicationDraft(user);
  // Select the default name and type over it, one key at a time (the name
  // never goes empty): after "nightly", the drawer must still hold the draft.
  await user.click(await nameInput());
  await user.keyboard('{Control>}a{/Control}n');
  for (const ch of 'ightly-eu') await user.type(await nameInput(), ch);

  expect((await nameInput()).value).toBe('nightly-eu');
  const rows = tableRows();
  expect(rows).toHaveLength(2);
  // The applied rule is untouched: not edited, not removed, same scope.
  expect(rows[0]).toMatch(/^nightlyreleases → downloads/);
  expect(rows[0]).not.toMatch(/edited|removing|draft/);
  expect(rows[1]).toMatch(/^nightly-eudraft/);
  // The drawer did not move onto the applied rule.
  expect(within(await screen.findByRole('dialog')).getByText('draft')).toBeInTheDocument();
});

test('a draft with an empty name keeps its drawer and shows as "(unnamed)"', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ReplicationRulesPanel />);
  await newReplicationDraft(user);
  await user.clear(await nameInput());
  expect((await nameInput()).value).toBe('');
  await waitFor(() => expect(tableRows().some((t) => t.startsWith('(unnamed)draft'))).toBe(true));
  // Typing a name again edits the same draft.
  await user.type(await nameInput(), 'fresh');
  await waitFor(() => expect(tableRows().some((t) => t.startsWith('freshdraft'))).toBe(true));
});

test('Remove draft removes the draft, not the applied rule with the same name', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ReplicationRulesPanel />);
  await newReplicationDraft(user);
  await user.click(await nameInput());
  await user.keyboard('{Control>}a{/Control}n');
  await user.type(await nameInput(), 'ightly');
  const drawer = await screen.findByRole('dialog');
  await user.click(within(drawer).getByRole('button', { name: 'More actions for nightly' }));
  await user.click(await screen.findByRole('menuitem', { name: /Remove draft/ }));
  const ok = await screen.findAllByRole('button', { name: 'Remove draft' });
  await user.click(ok[ok.length - 1]);

  await waitFor(() => expect(tableRows()).toHaveLength(1));
  expect(tableRows()[0]).not.toMatch(/edited|removing|draft/);
  // Nothing is left to apply.
  await waitFor(() => expect(screen.queryByRole('button', { name: 'Review & apply' })).toBeNull());
});

test('a rename applies as the new name, with no client id in the body; the drawer follows it', async () => {
  const SECTION = '/_/api/admin/config/section/storage';
  http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: { storage: { 'replication.rules': { before: 'nightly', after: 'nightly-eu' } } } }));
  let applied = false;
  http.on('PUT', SECTION, () => {
    applied = true;
    return json({ ok: true });
  });
  // After the apply the server knows the rule by its new name only.
  const renamedRow = { ...row, id: 'replication:nightly-eu', name: 'nightly-eu' };
  http.on('GET', '/_/api/admin/jobs', () => json({ jobs: [applied ? renamedRow : row] }));
  http.on('GET', SECTION, () => json({
    replication: { enabled: true, rules: [{ name: applied ? 'nightly-eu' : 'nightly', enabled: true,
      source: { bucket: 'releases', prefix: '' }, destination: { bucket: 'downloads', prefix: '' } }] },
  }));
  const user = userEvent.setup();
  const { client } = renderWithQuery(<ReplicationRulesPanel />);
  // A parity result cached for the old name must not outlive the rename.
  client.setQueryData(qk.jobs.verify('nightly'), { status: 'completed' });

  await user.click(await screen.findByText('nightly'));
  await user.click(await nameInput());
  await user.keyboard('{Control>}a{/Control}n');
  await user.type(await nameInput(), 'ightly-eu');
  // A rename is a delete and a create on the server: the table says so.
  await waitFor(() => expect(tableRows()).toHaveLength(2));
  expect(tableRows()[0]).toMatch(/^nightlyremoving/);
  expect(tableRows()[1]).toMatch(/^nightly-eudraft/);
  // The drawer still holds the same rule.
  expect((await nameInput()).value).toBe('nightly-eu');

  await user.click(screen.getByRole('button', { name: /Review & apply/ }));
  await user.click(await screen.findByRole('button', { name: 'Apply and persist changes' }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  const body = http.callsTo('PUT', SECTION)[0].body as { replication: { rules: Record<string, unknown>[] } };
  expect(body.replication.rules.map((r) => r.name)).toEqual(['nightly-eu']);
  expect(Object.keys(body.replication.rules[0]).sort()).toEqual(
    ['batch_size', 'conflict', 'destination', 'enabled', 'exclude_globs', 'include_globs', 'interval', 'name',
      'replicate_deletes', 'source'],
  );
  expect(client.getQueryData(qk.jobs.verify('nightly'))).toBeUndefined();

  // The drawer moves to the server id of the renamed rule: its Runs tab shows.
  await waitFor(() => expect(within(screen.getByRole('dialog')).getByRole('tab', { name: 'Runs' })).toBeInTheDocument());
  expect(within(screen.getByRole('dialog')).getAllByText('nightly-eu').length).toBeGreaterThan(0);
  expect((await nameInput()).value).toBe('nightly-eu');
});
