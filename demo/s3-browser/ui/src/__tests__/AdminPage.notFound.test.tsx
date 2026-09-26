/**
 * Browser-review item 20: /_/admin/access/userz rendered the dashboard (and
 * rewrote the URL) with no word. It now says the page does not exist and
 * links the nearest real page.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import AdminPage from '../components/AdminPage';
import { NavigationContext } from '../NavigationContext';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  const http = mockFetch();
  for (const m of ['GET', 'POST']) http.on(m, /./, json({}, 404));
  http.on('GET', '/_/api/whoami', json({ mode: 'iam', user: { name: 'root', access_key_id: 'AK', is_admin: true, permissions: [] } }));
  http.on('GET', '/_/api/admin/session', json({ valid: true, admin_gui: true }));
  http.on('GET', '/_/api/admin/users', json([]));
});
afterEach(() => vi.unstubAllGlobals());

test('an unknown admin path shows a not-found view with the nearest page', async () => {
  const navigate = vi.fn();
  const user = userEvent.setup();
  renderWithQuery(
    <NavigationContext.Provider value={{ navigate, subPath: 'access/userz' }}>
      <AdminPage onBack={() => {}} onShowShortcuts={() => {}} subPath="access/userz" canAdmin />
    </NavigationContext.Provider>,
  );
  expect(await screen.findByText('This settings page does not exist')).toBeInTheDocument();
  // Explore finding 19: the sidebar still marked Dashboard as the current
  // page (the unknown path resolves to it). No entry is current now.
  expect(document.querySelector('[aria-current="page"]')).toBeNull();
  // The URL is left alone (no silent rewrite to the dashboard).
  expect(navigate).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: 'Go to Users' }));
  expect(navigate).toHaveBeenCalledWith('/_/admin/access/users');
});
