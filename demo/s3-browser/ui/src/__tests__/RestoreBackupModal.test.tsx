/**
 * The restore dialog: an IAM-writing restore is point-in-time (replace) by
 * default, and the admin can pick merge instead. Config only never asks.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';

vi.mock('../queries/config', () => ({
  useIamMode: () => ({ iamMode: 'gui', readOnly: false, loadError: null }),
}));

import RestoreBackupModal from '../components/admin/RestoreBackupModal';

const file = new File(['zip'], 'dgp-backup.zip', { type: 'application/zip' });

describe('RestoreBackupModal', () => {
  test('replace is the default, and merge can be picked', async () => {
    const user = userEvent.setup();
    const onRestore = vi.fn();
    renderWithQuery(<RestoreBackupModal file={file} onCancel={() => {}} onRestore={onRestore} />);
    expect(screen.getByText(/Entries that the backup does not hold are deleted/)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Restore' }));
    expect(onRestore).toHaveBeenLastCalledWith(file, 'preserve-bootstrap', 'replace');

    await user.click(screen.getByRole('radio', { name: /Merge \(keep existing\)/ }));
    await user.click(screen.getByRole('button', { name: 'Restore' }));
    expect(onRestore).toHaveBeenLastCalledWith(file, 'preserve-bootstrap', 'merge');
  });

  test('config only does not offer the IAM choice', async () => {
    const user = userEvent.setup();
    renderWithQuery(<RestoreBackupModal file={file} onCancel={() => {}} onRestore={() => {}} />);
    await user.click(screen.getByRole('radio', { name: /Config only/ }));
    expect(screen.queryByRole('radio', { name: /Merge/ })).not.toBeInTheDocument();
  });
});
