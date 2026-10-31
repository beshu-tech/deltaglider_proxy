/**
 * UX harvest D16/D17/D36 on the Users page: a failed list showed the error
 * AND "No users declared — Your YAML config has no users" (declarative copy
 * on a GUI-mode instance); a loading list showed "Create the first user".
 * The page's "New user" is in the page header (PageActions).
 */
import { render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import UsersPanel from '../components/UsersPanel';
import TabHeader from '../components/TabHeader';
import { PageActionsSlot } from '../pageActionsSlot';
import { useState } from 'react';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui', env_overrides: [] }));
  http.on('GET', '/_/api/admin/policies', json([]));
  http.on('GET', '/_/api/admin/groups', json([]));
});
afterEach(() => vi.unstubAllGlobals());

test('a failed list is ONE error state with Retry, and never the empty state', async () => {
  http.on('GET', '/_/api/admin/users', json({ error: 'internal', message: 'boom' }, 500));
  renderWithQuery(<UsersPanel />);
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('Could not load the users');
  expect(alert).not.toHaveTextContent('boom');
  expect(screen.getByRole('button', { name: /Retry/ })).toBeInTheDocument();
  expect(screen.queryByText(/No users declared|Create the first user/)).toBeNull();
});

test('an empty list in GUI mode offers the first user, with the GUI copy', async () => {
  http.on('GET', '/_/api/admin/users', json([]));
  renderWithQuery(<UsersPanel />);
  expect(await screen.findByText('Create the first user')).toBeInTheDocument();
  expect(screen.queryByText(/YAML config/)).toBeNull();
});

function WithHeader() {
  const [slot, setSlot] = useState<HTMLElement | null>(null);
  return (
    <PageActionsSlot.Provider value={slot}>
      <TabHeader icon={null} title="Users" description="People and keys." actionsRef={setSlot} />
      <UsersPanel />
    </PageActionsSlot.Provider>
  );
}

test('"New user" is the primary action in the page header', async () => {
  http.on('GET', '/_/api/admin/users', json([]));
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(<QueryClientProvider client={qc}><WithHeader /></QueryClientProvider>);
  const button = await screen.findByTestId('md-new-users');
  expect(screen.getByTestId('page-actions')).toContainElement(button);
  expect(button).toHaveTextContent('New user');
  expect(button.className).toMatch(/ant-btn-primary/);
});
