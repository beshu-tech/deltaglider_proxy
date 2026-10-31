/**
 * UX harvest C61: the path "not-a-path" was answered "continue — No rule
 * matched" (the server read it as a bucket). The panel refuses it first.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import TracePanel from '../components/TracePanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

afterEach(() => vi.unstubAllGlobals());

test('a path without a leading slash is a field error, not a decision', async () => {
  const http = mockFetch().on('POST', '/_/api/admin/config/trace', json({}));
  const user = userEvent.setup();
  renderWithQuery(<TracePanel />);
  const input = screen.getByRole('textbox', { name: 'Path' });
  await user.clear(input);
  await user.type(input, 'not-a-path');
  await user.click(screen.getByRole('button', { name: /Test request/ }));
  expect(screen.getByText('The path must start with /.')).toBeInTheDocument();
  expect(input).toHaveAttribute('aria-invalid', 'true');
  expect(http.callsTo('POST', '/_/api/admin/config/trace')).toHaveLength(0);
  expect(screen.queryByText(/No rule matched/)).toBeNull();
});

test('a source IP that is not an address is a field error on blur, and nothing is sent (UX harvest D10)', async () => {
  const http = mockFetch().on('POST', '/_/api/admin/config/trace', json({}));
  const user = userEvent.setup();
  renderWithQuery(<TracePanel />);
  const ip = screen.getByRole('textbox', { name: 'Source IP' });
  await user.type(ip, '999.1.1.1');
  await user.tab();
  expect(screen.getByText(/"999\.1\.1\.1" is not an IP address/)).toBeInTheDocument();
  expect(ip).toHaveAttribute('aria-invalid', 'true');
  await user.click(screen.getByRole('button', { name: /Test request/ }));
  expect(http.callsTo('POST', '/_/api/admin/config/trace')).toHaveLength(0);
});
