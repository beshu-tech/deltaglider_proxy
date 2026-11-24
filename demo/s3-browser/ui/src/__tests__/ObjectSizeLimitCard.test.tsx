/**
 * The largest file (advanced.max_object_size) is ONE choice inside the
 * Storage card: Automatic, or a number of GB. The wire value is bytes, or
 * null for auto.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { StorageLimitEditor } from '../components/advancedPanels';
import { limitSizeError } from '../limitSize';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/advanced';
const GIB = 1024 * 1024 * 1024;
let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', SECTION, json({ max_object_size: 2 * GIB }));
  http.on('GET', '/_/api/admin/config', json({}));
  http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: { advanced: { max_object_size: { before: 2 * GIB, after: 50 * GIB } } } }));
  http.on('PUT', SECTION, json({ ok: true }));
});
afterEach(() => vi.unstubAllGlobals());

const apply = async (user: ReturnType<typeof userEvent.setup>) => {
  await user.click(screen.getByRole('button', { name: /Review & apply/ }));
  await user.click(await screen.findByRole('button', { name: 'Apply and persist changes' }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  return http.callsTo('PUT', SECTION)[0].body;
};

test('a set limit shows in GiB and a new number is sent in bytes', async () => {
  const user = userEvent.setup();
  const onApplied = vi.fn();
  renderWithQuery(<StorageLimitEditor onApplied={onApplied} />);
  const input = await screen.findByRole('textbox', { name: 'Limit in GiB' });
  await waitFor(() => expect(input).toHaveValue('2'));
  expect(screen.getByRole('radio', { name: /Set a limit/ })).toBeChecked();
  await user.clear(input);
  await user.type(input, '50');
  expect(await apply(user)).toEqual({ max_object_size: 50 * GIB });
  await waitFor(() => expect(onApplied).toHaveBeenCalled());
});

const LIMITS = {
  limits: { max_object_size: { bytes: 15 * GIB, source: 'auto', detail: 'auto: the per-upload limit' } },
};

test('auto is the default: it names the size, with no jargon', async () => {
  http.on('GET', SECTION, json({}));
  http.on('GET', '/_/api/admin/config', json(LIMITS));
  renderWithQuery(<StorageLimitEditor />);
  expect(await screen.findByRole('radio', { name: /^Automatic/ })).toBeChecked();
  await waitFor(() => expect(screen.getByRole('radiogroup', { name: 'Largest file' })).toHaveTextContent('Automatic (15 GiB, from the size of your disk)'));
  expect(screen.getByRole('textbox', { name: 'Limit in GiB' })).toBeDisabled();
  expect(document.body).not.toHaveTextContent(/MiB|max_object_size|principal|multipart/);
});

test('a literal "auto" in the section is Automatic, never NaN', async () => {
  http.on('GET', SECTION, json({ max_object_size: 'auto' }));
  http.on('GET', '/_/api/admin/config', json(LIMITS));
  renderWithQuery(<StorageLimitEditor />);
  expect(await screen.findByRole('radio', { name: /^Automatic/ })).toBeChecked();
  expect((screen.getByRole('textbox', { name: 'Limit in GiB' }) as HTMLInputElement).value).toBe('');
});

test('picking Automatic sends null', async () => {
  const user = userEvent.setup();
  renderWithQuery(<StorageLimitEditor />);
  await waitFor(() => expect(screen.getByRole('textbox', { name: 'Limit in GiB' })).toHaveValue('2'));
  await user.click(screen.getByRole('radio', { name: /^Automatic/ }));
  expect(await apply(user)).toEqual({ max_object_size: null });
});

test('"Set a limit" with nothing typed changes nothing; a bad number says why', async () => {
  http.on('GET', SECTION, json({}));
  const user = userEvent.setup();
  renderWithQuery(<StorageLimitEditor />);
  await user.click(await screen.findByRole('radio', { name: /Set a limit/ }));
  const input = screen.getByRole('textbox', { name: 'Limit in GiB' });
  expect(input).toBeEnabled();
  expect(screen.queryByRole('button', { name: /Review & apply/ })).not.toBeInTheDocument();
  await user.type(input, 'abc');
  await user.tab();
  expect(await screen.findByRole('alert')).toHaveTextContent('Enter a size between 0.01 and 5,120 GiB.');
});

test('the limit rule: 0.01 to 5120 GiB, two decimals', () => {
  for (const ok of ['', '1', '0.01', '37.5', '5120']) expect(limitSizeError(ok)).toBeNull();
  for (const bad of ['0', '0.001', '5121', '-1', '1e3', 'abc', '1.234']) expect(limitSizeError(bad)).not.toBeNull();
});
