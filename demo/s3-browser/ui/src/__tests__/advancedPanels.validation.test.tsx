/**
 * UX harvest D9/D10 on the System page: cache size "-1" + Tab became 16 and
 * the page turned dirty with no message; listen address "not an address" was
 * accepted. Now the typed text stays, the field says what is wrong
 * (aria-invalid), the bar counts it, and Apply sends nothing.
 */
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { CachesPanel, ListenerTlsPanel } from '../components/advancedPanels';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch()
    .on('GET', '/_/api/admin/config/section/advanced', json({ cache_size_mb: 1024 }))
    .on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('a negative cache size keeps its text, shows the rule on blur, and blocks Apply', async () => {
  renderWithQuery(<CachesPanel />);
  const input = await screen.findByRole('textbox', { name: 'Reference cache size (MiB)' });
  expect(input).toHaveValue('1024');
  expect(input).toHaveAttribute('inputmode', 'numeric');
  fireEvent.change(input, { target: { value: '-1' } });
  // No error before the field loses focus.
  expect(screen.queryByText(/Enter a whole number/)).toBeNull();
  fireEvent.blur(input);
  expect(input).toHaveValue('-1');
  expect(await screen.findByText('Enter a whole number between 16 and 65,536.')).toBeInTheDocument();
  expect(input).toHaveAttribute('aria-invalid', 'true');
  expect(screen.getByText(/1 issue to fix/)).toBeInTheDocument();
  expect(http.callsTo('POST', '/_/api/admin/config/section/advanced/validate')).toHaveLength(0);
});

test('a valid typed value is stored as a number and the error goes away', async () => {
  renderWithQuery(<CachesPanel />);
  const input = await screen.findByRole('textbox', { name: 'Reference cache size (MiB)' });
  fireEvent.change(input, { target: { value: 'abc' } });
  fireEvent.blur(input);
  await screen.findByText(/Enter a whole number/);
  fireEvent.change(input, { target: { value: '2048' } });
  await waitFor(() => expect(screen.queryByText(/Enter a whole number/)).toBeNull());
  expect(input).not.toHaveAttribute('aria-invalid');
  expect(input).toHaveValue('2048');
});

test('a listen address that is not host:port is refused on the field', async () => {
  renderWithQuery(<ListenerTlsPanel />);
  const input = await screen.findByRole('textbox', { name: /Listen address/ });
  fireEvent.change(input, { target: { value: 'not an address' } });
  fireEvent.blur(input);
  expect(await screen.findByText(/Enter host:port/)).toBeInTheDocument();
  expect(input).toHaveAttribute('aria-invalid', 'true');
});
