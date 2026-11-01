/**
 * Review finding 11: a disabled "Rotate key" on a backend without proven
 * atomic writes says why, visibly and as the button's accessible
 * description (a disabled button gets no hover title on touch, and a
 * screen reader did not hear a reason).
 */
import { screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import BucketEncryptionGroup from '../components/BucketEncryptionGroup';
import { mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff } from '../test/encryption';

beforeEach(() => {
  mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

test('backend editor: Rotate key is disabled and described on a non-CAS backend', () => {
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="b2"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-2026', shim_active: false }}
      onApply={async () => {}}
      capability={{ verdict: 'non-cas' }}
    />,
  );
  const rotate = screen.getByRole('button', { name: /Rotate key/ });
  expect(rotate).toBeDisabled();
  expect(rotate).toHaveAccessibleDescription('Needs atomic writes, which b2 does not support.');
});

test('bucket group: Rotate key is disabled and described when the proxy could not verify the backend', () => {
  renderWithQuery(
    <BucketEncryptionGroup
      bucket="db-archive"
      eff={bucketKeyEff({ backend: 'b2', capability: { verdict: 'unknown', reason: 'timeout' } })}
      onApplied={vi.fn()}
    />,
  );
  const rotate = screen.getByRole('button', { name: /Rotate key/ });
  expect(rotate).toBeDisabled();
  expect(rotate).toHaveAccessibleDescription('Needs atomic writes, which the proxy could not verify on b2.');
});
