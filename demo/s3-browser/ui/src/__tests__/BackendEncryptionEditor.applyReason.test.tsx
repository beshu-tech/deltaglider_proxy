/**
 * UX harvest D50: a disabled control states its reason as text. Apply of a
 * new proxy key stays disabled until the operator confirms that the key is
 * stored safely; the line under it says so.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test } from 'vitest';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import { renderWithQuery } from '../test/render';

test('a disabled Apply says why, and the reason goes away once Apply is enabled', async () => {
  const user = userEvent.setup();
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="local-disk"
      current={{ mode: 'none', has_key: false, shim_active: false }}
      onApply={async () => {}}
    />,
  );
  await user.click(screen.getByRole('combobox', { name: 'Encryption mode' }));
  await user.click(await screen.findByText('AES-256-GCM (proxy-side)', { selector: '.ant-select-item-option-content' }));
  expect(screen.getByRole('button', { name: 'Apply encryption change' })).toBeDisabled();
  expect(screen.getByTestId('encryption-apply-reason')).toHaveTextContent('Confirm that you stored the key safely to apply.');
  await user.click(screen.getByRole('checkbox'));
  expect(screen.getByRole('button', { name: 'Apply encryption change' })).toBeEnabled();
  expect(screen.queryByTestId('encryption-apply-reason')).toBeNull();
});
