/**
 * The delete-replication switch can delete data on the destination, so it
 * has an accessible name and turning it on asks for confirmation first.
 */
import { screen } from '@testing-library/react';
import { renderWithQuery } from '../test/render';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import type { ReplicationRuleConfig } from '../adminApi';
import ReplicationRuleFields from '../components/ReplicationRuleFields';

const rule = {
  name: 'releases-to-dr',
  enabled: true,
  source: { bucket: 'releases', prefix: '' },
  destination: { bucket: 'downloads', prefix: 'mirror/' },
  interval: '24h',
  batch_size: 100,
  conflict: 'newer-wins',
  replicate_deletes: false,
  include_globs: [],
  exclude_globs: [],
} as unknown as ReplicationRuleConfig;

function setup(r = rule) {
  const onChange = vi.fn();
  renderWithQuery(
    <ReplicationRuleFields rule={r} buckets={[]} inputRadius={{ borderRadius: 8 }} onChange={onChange} onRename={() => {}} />,
  );
  return onChange;
}

const NAME = 'Delete objects on the destination that are not on the source';

test('turning delete replication on asks first, and only the confirmation turns it on', async () => {
  const onChange = setup();
  const user = userEvent.setup();
  await user.click(screen.getByRole('switch', { name: NAME }));
  expect(onChange).not.toHaveBeenCalled();
  expect((await screen.findAllByText('Delete objects on the destination?')).length).toBeGreaterThan(0);
  expect(screen.getByText(/under downloads\/mirror\/ that is not on the source/)).toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: 'Turn on delete replication' }));
  expect(onChange).toHaveBeenCalledWith({ replicate_deletes: true });
});

test('turning it off needs no confirmation', async () => {
  const onChange = setup({ ...rule, replicate_deletes: true } as ReplicationRuleConfig);
  await userEvent.setup().click(screen.getByRole('switch', { name: NAME }));
  expect(onChange).toHaveBeenCalledWith({ replicate_deletes: false });
});
