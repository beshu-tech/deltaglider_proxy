/**
 * Browser-review item 19: an IAM user without admin rights (a files-only
 * session) who opened Settings got the bootstrap "Admin password" prompt.
 * That password is not theirs to know. They now read that their account
 * has no admin rights.
 */
import { screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import AdminPage from '../components/AdminPage';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  const http = mockFetch();
  for (const m of ['GET', 'POST']) http.on(m, /./, json({}, 404));
  http.on('GET', '/_/api/whoami', json({
    mode: 'iam',
    user: { name: 'dana', access_key_id: 'AKDANA', is_admin: false, permissions: [] },
  }));
  http.on('GET', '/_/api/admin/session', json({ valid: true, admin_gui: false }));
});
afterEach(() => vi.unstubAllGlobals());

test('a files-only IAM session without admin rights is told so, not asked for the admin password', async () => {
  renderWithQuery(<AdminPage onBack={() => {}} onShowShortcuts={() => {}} subPath="dashboard" />);
  expect(await screen.findByText(/Your account has no admin rights/)).toBeInTheDocument();
  expect(screen.queryByPlaceholderText('Admin password')).not.toBeInTheDocument();
});
