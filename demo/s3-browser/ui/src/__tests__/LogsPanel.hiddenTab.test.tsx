/**
 * The logs live tail closes its SSE stream while the tab is hidden and, on
 * return, reloads the backlog (the gap) and opens the stream again (P8.8c).
 */
import { act, fireEvent, screen, waitFor } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';

const api = vi.hoisted(() => ({
  opened: 0,
  closed: 0,
  backlogs: 0,
}));
vi.mock('../adminApi', () => ({
  fetchLogs: async () => {
    api.backlogs++;
    return { entries: [], limit: 1000 };
  },
  streamLogs: () => {
    api.opened++;
    return () => {
      api.closed++;
    };
  },
}));

import LogsPanel from '../components/LogsPanel';

function setHidden(hidden: boolean) {
  Object.defineProperty(document, 'hidden', { configurable: true, value: hidden });
  act(() => {
    document.dispatchEvent(new Event('visibilitychange'));
  });
}

afterEach(() => {
  Object.defineProperty(document, 'hidden', { configurable: true, value: false });
});

test('a hidden tab holds no stream; on return the backlog reloads and the tail resumes', async () => {
  setHidden(false);
  renderWithQuery(<LogsPanel />);
  await waitFor(() => expect(api.backlogs).toBe(1));
  fireEvent.click(screen.getByRole('switch', { name: 'Follow' }));
  await waitFor(() => expect(api.opened).toBe(1));
  await waitFor(() => expect(api.backlogs).toBe(1));

  setHidden(true);
  expect(api.closed).toBe(1);
  expect(api.opened).toBe(1);

  setHidden(false);
  await waitFor(() => expect(api.opened).toBe(2));
  await waitFor(() => expect(api.backlogs).toBe(2));
});
