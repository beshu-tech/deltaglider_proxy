/**
 * Issue #116 P2: the key dialog said "DeltaGlider does not back up your
 * encryption key". The full backup DOES carry a key that is in the config
 * file (`harvest_config_secrets` walks every encryption site); it does not
 * carry a key that an environment variable supplies. The text now says so.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test } from 'vitest';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import { renderWithQuery } from '../test/render';

test('the key warning says what the backup holds', async () => {
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
  expect(screen.queryByText(/does not back up your encryption key/)).toBeNull();
  expect(screen.getByText(/includes this key while it is in the config file, but not a key that an environment variable supplies/)).toBeInTheDocument();
});
