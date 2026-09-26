/**
 * U3: the decrypt-only shim banner offers "Clear legacy key". The button is
 * enabled only when the server's scan (`GET /backends/:name/legacy-key-usage`)
 * says that no object and no delta reference still carries the legacy key id
 * (`safe_to_clear`). The confirm dialog names the consequence.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const CURRENT = { mode: 'aes256-gcm-proxy' as const, has_key: true, key_id: 'k-2026', shim_active: true };
const USAGE = '/_/api/admin/backends/local-disk/legacy-key-usage';

function usage(over: Record<string, unknown>) {
  return {
    backend: 'local-disk',
    legacy_key_id: 'k-2025',
    buckets: ['releases', 'db-archive'],
    objects_scanned: 120,
    objects_under_legacy_key: 0,
    references_scanned: 3,
    references_under_legacy_key: 0,
    examples: [],
    errors: [],
    complete: true,
    limit: 10000,
    safe_to_clear: true,
    ...over,
  };
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

function renderEditor(onClearLegacy = vi.fn(async () => {})) {
  renderWithQuery(
    <BackendEncryptionEditor backendName="local-disk" current={CURRENT} onApply={async () => {}} onClearLegacy={onClearLegacy} />,
  );
  return onClearLegacy;
}

test('objects still under the legacy key keep the button disabled and say how many', async () => {
  http.on('GET', USAGE, json(usage({
    objects_under_legacy_key: 7,
    references_under_legacy_key: 1,
    examples: ['releases/builds/app.zip'],
    safe_to_clear: false,
  })));
  renderEditor();
  expect(await screen.findByText(/7 objects and 1 delta reference still use the legacy key id k-2025/)).toBeInTheDocument();
  expect(screen.getByText(/releases\/builds\/app\.zip/)).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Clear legacy key' })).toBeDisabled();
});

test('a scan stopped by its limit keeps the button disabled', async () => {
  http.on('GET', USAGE, json(usage({ complete: false, objects_scanned: 10000, safe_to_clear: false })));
  renderEditor();
  expect(await screen.findByText(/stopped after 10,000 objects/)).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Clear legacy key' })).toBeDisabled();
});

test('a clean scan enables the button; the confirm dialog explains the consequence', async () => {
  http.on('GET', USAGE, json(usage({})));
  const onClear = renderEditor();
  const user = userEvent.setup();
  expect(await screen.findByText(/No object uses the legacy key id k-2025/)).toBeInTheDocument();
  const button = screen.getByRole('button', { name: 'Clear legacy key' });
  await waitFor(() => expect(button).toBeEnabled());
  await user.click(button);
  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent(/cannot be read any more/);
  expect(dialog).toHaveTextContent(/keep a copy of the old key/i);
  await user.click(within(dialog).getByRole('button', { name: 'Clear' }));
  await waitFor(() => expect(onClear).toHaveBeenCalledTimes(1));
});

test('without an active shim the banner and the scan are absent', async () => {
  renderWithQuery(
    <BackendEncryptionEditor backendName="local-disk" current={{ ...CURRENT, shim_active: false }} onApply={async () => {}} onClearLegacy={async () => {}} />,
  );
  expect(screen.queryByRole('button', { name: 'Clear legacy key' })).toBeNull();
  expect(http.callsTo('GET', USAGE)).toHaveLength(0);
});
