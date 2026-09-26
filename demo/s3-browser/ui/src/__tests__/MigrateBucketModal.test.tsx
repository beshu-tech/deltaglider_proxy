/**
 * The migrate modal exposes the destination mode. By default the server
 * refuses a destination that already holds objects (an old safety copy
 * would bring deleted objects back); "exact mirror" sends `target: mirror`
 * and warns that destination extras are deleted.
 */
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { describe, expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';

const createMigrateJob = vi.fn(async () => ({
  job_id: 1,
  id: 'maintenance:1',
  bucket: 'releases',
  from_backend: 'hetzner-fsn1',
  to_backend: 'local-disk',
}));
vi.mock('../adminApi', () => ({
  getBackends: async () => ({
    default_backend: 'hetzner-fsn1',
    backends: [
      { name: 'hetzner-fsn1', backend_type: 's3' },
      { name: 'local-disk', backend_type: 'filesystem' },
    ],
  }),
  getBucketOrigins: async () => ({ buckets: [{ name: 'releases', backend_name: 'hetzner-fsn1' }] }),
  createMigrateJob: (...args: unknown[]) => createMigrateJob(...(args as [])),
}));

import MigrateBucketModal from '../components/MigrateBucketModal';

async function pickTarget() {
  const combo = await screen.findByRole('combobox');
  await waitFor(() => expect(combo.closest('.ant-select-disabled')).toBeNull());
  fireEvent.mouseDown(combo);
  fireEvent.click(await screen.findByText('local-disk (filesystem)'));
}

describe('MigrateBucketModal destination mode', () => {
  test('defaults to an empty destination and says so', async () => {
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    expect(await screen.findByText(/already holds objects, the job refuses/i)).toBeTruthy();
    await pickTarget();
    fireEvent.click(screen.getByRole('button', { name: 'Start migration' }));
    await waitFor(() => expect(createMigrateJob).toHaveBeenCalled());
    expect(createMigrateJob).toHaveBeenLastCalledWith('releases', 'local-disk', false, 'empty');
  });

  test('exact mirror warns and sends target: mirror', async () => {
    createMigrateJob.mockClear();
    renderWithQuery(<MigrateBucketModal open bucket="releases" onClose={() => {}} />);
    await pickTarget();
    expect(screen.queryByText(/are deleted before the switch-over/i)).toBeNull();
    fireEvent.click(screen.getByRole('checkbox', { name: /exact mirror/i }));
    expect(await screen.findByText(/are deleted before the switch-over/i)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Start migration' }));
    await waitFor(() => expect(createMigrateJob).toHaveBeenCalled());
    expect(createMigrateJob).toHaveBeenLastCalledWith('releases', 'local-disk', false, 'mirror');
  });
});
