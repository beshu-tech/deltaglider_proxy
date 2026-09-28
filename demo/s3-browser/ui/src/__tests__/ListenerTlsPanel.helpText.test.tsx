/**
 * Batch-D audit: the Enable TLS help said it "Requires a cert + key path".
 * With both paths empty the proxy makes a self-signed certificate
 * (tls.rs build_rustls_config); only ONE path set is an error.
 */
import { screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { ListenerTlsPanel } from '../components/advancedPanels';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  mockFetch()
    .on('GET', '/_/api/admin/config/section/advanced', json({}))
    .on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('the Enable TLS help says that empty paths give a self-signed certificate', async () => {
  renderWithQuery(<ListenerTlsPanel />);
  const help = await screen.findByText(/^Serve HTTPS directly\./);
  expect(help.textContent).not.toMatch(/Requires a cert/);
  expect(help.textContent).toMatch(/self-signed/);
});
