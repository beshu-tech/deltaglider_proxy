/**
 * UX harvest C38: the Log level card promised "takes effect after Apply"
 * while the only field was read-only "from env RUST_LOG".
 */
import { screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { LoggingPanel } from '../components/advancedPanels';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

afterEach(() => vi.unstubAllGlobals());

function serve(envOverrides: unknown[]) {
  mockFetch()
    .on('GET', '/_/api/admin/config/section/advanced', json({ log_level: 'warn' }))
    .on('GET', '/_/api/admin/config', json({ env_overrides: envOverrides }));
}

test('an env-owned log level makes no promise about Apply', async () => {
  serve([{ env: 'RUST_LOG', yaml_path: 'advanced.log_level', secret: false, value: 'warn', set: true }]);
  renderWithQuery(<LoggingPanel />);
  expect(await screen.findByText(/from env RUST_LOG/)).toBeInTheDocument();
  expect(screen.queryByText(/after Apply/)).toBeNull();
});

test('a file-owned log level says a change applies with no restart', async () => {
  serve([]);
  renderWithQuery(<LoggingPanel />);
  expect(await screen.findByText(/takes effect on the next request after Apply/)).toBeInTheDocument();
});
