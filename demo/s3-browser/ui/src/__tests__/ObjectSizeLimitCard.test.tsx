/**
 * Browser-review item 14: max_object_size (advanced.max_object_size) had no
 * field anywhere in the UI. The Buckets page now edits it; the value is MB
 * in the form and bytes on the wire.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { ObjectSizeLimitCard } from '../components/advancedPanels';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/advanced';
let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', SECTION, json({ max_object_size: 100 * 1024 * 1024 }));
  http.on('GET', '/_/api/admin/config', json({}));
  http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: {} }));
  http.on('PUT', SECTION, json({ ok: true }));
});
afterEach(() => vi.unstubAllGlobals());

test('edits max_object_size in MB and PUTs bytes', async () => {
  const user = userEvent.setup();
  renderWithQuery(<ObjectSizeLimitCard />);
  const input = await screen.findByRole('spinbutton', { name: /Maximum object size/ });
  await waitFor(() => expect(input).toHaveValue('100'));
  await user.clear(input);
  await user.type(input, '250');
  await user.click(screen.getByRole('button', { name: /Review & apply/ }));
  await user.click(await screen.findByRole('button', { name: 'Apply and persist changes' }));
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  expect(http.callsTo('PUT', SECTION)[0].body).toEqual({ max_object_size: 250 * 1024 * 1024 });
});
