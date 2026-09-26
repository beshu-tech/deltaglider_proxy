/**
 * Browser-review item 17: the setup wizard's Apply toasted "Applied with 2
 * warning(s)" and left the page at once, so nobody could read them. The
 * warnings now stay on the Review step until the admin continues.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import SetupWizard from '../components/SetupWizard';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  // Env answers every step, so Next is always allowed.
  http.on('GET', '/_/api/admin/config', json({
    backends: [], default_backend: null,
    env_overrides: [
      { env: 'DGP_BE_TYPE', yaml_path: 'storage.backend.type', secret: false, value: 'filesystem', active: true },
      { env: 'DGP_ACCESS_KEY_ID', yaml_path: 'access.access_key_id', secret: true, active: true },
      { env: 'DGP_SECRET_ACCESS_KEY', yaml_path: 'access.secret_access_key', secret: true, active: true },
    ],
  }));
  for (const s of ['admission', 'access', 'storage', 'advanced']) http.on('GET', `/_/api/admin/config/section/${s}`, json({}));
  http.on('POST', '/_/api/admin/config/apply', json({
    applied: true, persisted: true, requires_restart: false,
    warnings: ['bucket "releases" has no backend', 'listen_addr needs a restart'],
  }));
});
afterEach(() => vi.unstubAllGlobals());

test('apply warnings stay on screen until the admin continues', async () => {
  const onComplete = vi.fn();
  const user = userEvent.setup();
  renderWithQuery(<SetupWizard onComplete={onComplete} onCancel={() => {}} />);
  for (let i = 0; i < 4; i++) {
    const next = await screen.findByRole('button', { name: /Next/ });
    await waitFor(() => expect(next).toBeEnabled());
    await user.click(next);
  }
  await user.click(screen.getByRole('button', { name: /Save and start/ }));
  expect(await screen.findByText('bucket "releases" has no backend')).toBeInTheDocument();
  expect(screen.getByText('listen_addr needs a restart')).toBeInTheDocument();
  expect(onComplete).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: 'Continue' }));
  expect(onComplete).toHaveBeenCalled();
});
