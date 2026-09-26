/**
 * Browser-review item 18: the session expired while an admin edited Buckets;
 * Review & apply answered "Validate failed: … (401): unauthorized", and the
 * only way on was to sign in again from scratch, losing the edits.
 *
 * Now a 401 on validate or apply asks for the sign-in in a modal, on the same
 * page, and retries the step with the same edits.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import ReloginModal from '../components/ReloginModal';
import { requestRelogin } from '../sessionRelogin';
import { useSectionEditor } from '../useSectionEditor';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery, renderWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/advanced';
let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

test('without a sign-in handler, requestRelogin answers false', async () => {
  expect(await requestRelogin()).toBe(false);
});

test('the modal signs in with the admin password and resolves true', async () => {
  http.on('GET', '/_/api/whoami', json({ mode: 'bootstrap', user: null }));
  http.on('POST', '/_/api/admin/login', json({ ok: true }));
  const user = userEvent.setup();
  renderWithQuery(<ReloginModal />);
  let answer: Promise<boolean> = Promise.resolve(false);
  act(() => { answer = requestRelogin(); });
  const dialog = await screen.findByRole('dialog', { name: /Your session expired/ });
  await user.type(within(dialog).getByLabelText('Admin password'), 'testpass');
  await user.click(within(dialog).getByRole('button', { name: 'Sign in' }));
  expect(await answer).toBe(true);
  expect(http.callsTo('POST', '/_/api/admin/login')[0].body).toEqual({ password: 'testpass' });
  await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
});

test('the modal signs in an IAM admin with an access key', async () => {
  http.on('GET', '/_/api/whoami', json({ mode: 'iam', user: null }));
  http.on('POST', '/_/api/admin/login-as', json({ ok: true }));
  const user = userEvent.setup();
  renderWithQuery(<ReloginModal />);
  let answer: Promise<boolean> = Promise.resolve(false);
  act(() => { answer = requestRelogin(); });
  const dialog = await screen.findByRole('dialog', { name: /Your session expired/ });
  await user.type(await within(dialog).findByLabelText('Access key ID'), 'REVIEWADMIN');
  await user.type(within(dialog).getByLabelText('Secret access key'), 'secret-1234');
  await user.click(within(dialog).getByRole('button', { name: 'Sign in' }));
  expect(await answer).toBe(true);
  expect(http.callsTo('POST', '/_/api/admin/login-as')[0].body).toEqual({ access_key_id: 'REVIEWADMIN', secret_access_key: 'secret-1234' });
});

test('a 401 on validate asks for the sign-in, then validates the same edits again', async () => {
  http.on('GET', SECTION, json({ cache_size_mb: 100 }));
  let validates = 0;
  http.on('POST', `${SECTION}/validate`, () => (++validates === 1 ? json({ error: 'unauthorized' }, 401) : json({ ok: true, diff: {} })));
  http.on('GET', '/_/api/whoami', json({ mode: 'bootstrap', user: null }));
  http.on('POST', '/_/api/admin/login', json({ ok: true }));
  const user = userEvent.setup();
  renderWithQuery(<ReloginModal />);
  const { result } = renderHookWithQuery(() =>
    useSectionEditor<{ cache_size_mb: number }>({ section: 'advanced', dirtyKey: 'advanced/caches', initial: { cache_size_mb: 100 } }),
  );
  await waitFor(() => expect(result.current.loading).toBe(false));
  act(() => result.current.setValue({ cache_size_mb: 512 }));
  let applying: Promise<void> = Promise.resolve();
  act(() => { applying = result.current.runApply(); });
  const dialog = await screen.findByRole('dialog', { name: /Your session expired/ });
  await user.type(within(dialog).getByLabelText('Admin password'), 'testpass');
  await user.click(within(dialog).getByRole('button', { name: 'Sign in' }));
  await act(() => applying);
  expect(http.callsTo('POST', `${SECTION}/validate`).map((c) => c.body)).toEqual([{ cache_size_mb: 512 }, { cache_size_mb: 512 }]);
  expect(result.current.applyOpen).toBe(true);
  expect(result.current.isDirty).toBe(true);
});
