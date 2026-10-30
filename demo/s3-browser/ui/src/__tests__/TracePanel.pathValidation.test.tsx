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
