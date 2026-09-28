/**
 * A document apply (`POST /config/apply`) that works in memory but cannot
 * write a read-only config file answers 500 with `applied: true`,
 * `persisted: false` and `persist_error`. The import modal says that the
 * change is lost at the next restart, not a generic error.
 */
import { fireEvent, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import { YamlImportExportModal } from '../components/YamlImportExportModal';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

afterEach(() => vi.unstubAllGlobals());

test('a document apply that did not persist says the change is lost at restart', async () => {
  const http = mockFetch();
  http.on('POST', '/_/api/admin/config/validate', json({ ok: true, warnings: [] }));
  http.on('POST', '/_/api/admin/config/apply', json({
    applied: true,
    persisted: false,
    requires_restart: false,
    warnings: ['Applied in memory but FAILED to persist'],
    persist_error: '/etc/deltaglider_proxy/config.yaml: Read-only file system (os error 30)',
  }, 500));
  const user = userEvent.setup();
  renderWithQuery(<YamlImportExportModal open mode="import" onClose={() => {}} />);
  fireEvent.change(screen.getByRole('textbox', { name: 'YAML to import' }), { target: { value: 'advanced:\n  cache_size_mb: 2048\n' } });
  await user.click(screen.getByRole('button', { name: 'Validate' }));
  await user.click(await screen.findByRole('button', { name: 'Apply and Persist' }));
  expect(
    await screen.findByText(/Applied to the running proxy, but the config file is read-only, so this change is lost at the next restart\. Export the YAML and update your deployment\./),
  ).toBeInTheDocument();
});
