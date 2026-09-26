/**
 * Browser-review item 24: typing "My_Bucket" in Create bucket silently
 * became "mybucket". The input now keeps the text, says what is wrong and
 * disables Create.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({ createBucket: vi.fn() }));
import CreateBucketModal from '../components/CreateBucketModal';

test('an invalid name is kept and explained, not rewritten', async () => {
  const user = userEvent.setup();
  renderWithQuery(<CreateBucketModal open canAdmin={false} onClose={() => {}} onCreated={() => {}} />);
  const input = await screen.findByLabelText('Bucket name');
  await user.type(input, 'My_Bucket');
  expect(input).toHaveValue('My_Bucket');
  expect(screen.getByRole('alert')).toHaveTextContent('Use only lowercase letters, digits, dots and hyphens.');
  expect(screen.getByRole('button', { name: 'Create' })).toBeDisabled();
  await user.clear(input);
  await user.type(input, 'my-bucket');
  expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Create' })).toBeEnabled();
});
