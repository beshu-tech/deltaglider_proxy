/**
 * The trace response carries `anonymous_grant`: what an `allow-anonymous`
 * decision lets the anonymous caller do (one object read, one listing, or
 * the bucket's public prefixes), or `null` (a write, or another decision).
 * The panel shows it, so "allow-anonymous" on a PUT no longer reads as
 * "the upload is allowed".
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import TracePanel from '../components/TracePanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

function reply(method: string, key: string | null, grant: unknown, decision = 'allow-anonymous') {
  http.on(
    'POST',
    '/_/api/admin/config/trace',
    json({
      resolved: { method, bucket: 'releases', key, list_prefix: null, authenticated: false },
      admission: { decision, matched: decision === 'continue' ? null : 'public-builds' },
      anonymous_grant: grant,
    }),
  );
}

async function runTrace() {
  const user = userEvent.setup();
  renderWithQuery(<TracePanel />);
  await user.click(screen.getByRole('button', { name: /Test request/ }));
  return screen.findByText('Anonymous access');
}

test('a read grant names the one object the caller may download', async () => {
  reply('GET', 'builds/app.zip', { action: 'read', bucket: 'releases', key: 'builds/app.zip' });
  await runTrace();
  expect(screen.getByText(/may read the object releases\/builds\/app\.zip/)).toBeInTheDocument();
});

test('a list grant names the bucket and the prefix', async () => {
  reply('GET', null, { action: 'list', bucket: 'releases', prefix: 'builds/' });
  await runTrace();
  expect(screen.getByText(/may list the bucket releases with the prefix builds\//)).toBeInTheDocument();
});

test('a public-prefixes grant points at the bucket public prefixes', async () => {
  reply('GET', null, { action: 'public-prefixes', bucket: 'releases' });
  await runTrace();
  expect(screen.getByText(/public prefixes of the bucket releases/)).toBeInTheDocument();
});

test('allow-anonymous on a write says that no anonymous access is granted', async () => {
  reply('PUT', 'builds/app.zip', null);
  await runTrace();
  expect(screen.getByText(/grants nothing to an anonymous caller/)).toBeInTheDocument();
  expect(screen.getByText(/write: no grant, 403 AccessDenied/)).toBeInTheDocument();
});

test('a decision other than allow-anonymous shows no grant section', async () => {
  reply('GET', 'builds/app.zip', null, 'continue');
  const user = userEvent.setup();
  renderWithQuery(<TracePanel />);
  await user.click(screen.getByRole('button', { name: /Test request/ }));
  await screen.findByText('Reason path');
  expect(screen.queryByText('Anonymous access')).toBeNull();
});
