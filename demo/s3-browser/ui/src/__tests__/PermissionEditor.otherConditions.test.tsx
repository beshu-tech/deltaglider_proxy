/**
 * The editor renders only StringLike s3:prefix and IpAddress aws:SourceIp.
 * A rule set through the API or YAML can carry other conditions (the server
 * evaluates StringEquals, StringNotLike, NotIpAddress, ...). They used to be
 * invisible: empty filters, with the Conditions button disabled. They now
 * show read-only, and an edit keeps them.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import PermissionEditor from '../components/PermissionEditor';
import { otherConditions } from '../components/permissionConditions';
import type { PermissionRow } from '../components/permissionRows';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return { ...real, listBuckets: vi.fn(async () => []), listCommonPrefixes: vi.fn(async () => []) };
});

const conditions = {
  StringLike: { 's3:prefix': ['releases/'] },
  StringNotLike: { 's3:prefix': ['releases/tmp/*'] },
  NotIpAddress: { 'aws:SourceIp': ['10.0.0.0/8', '192.168.0.0/16'] },
};

test('otherConditions lists every operator/key the editor cannot render', () => {
  expect(otherConditions(conditions)).toEqual([
    { operator: 'StringNotLike', key: 's3:prefix', values: ['releases/tmp/*'] },
    { operator: 'NotIpAddress', key: 'aws:SourceIp', values: ['10.0.0.0/8', '192.168.0.0/16'] },
  ]);
  expect(otherConditions({ StringLike: { 's3:prefix': 'a/' }, IpAddress: { 'aws:SourceIp': '1.2.3.4' } })).toEqual([]);
  expect(otherConditions(undefined)).toEqual([]);
});

test('the editor shows the other conditions read-only and keeps them on edit', async () => {
  const row: PermissionRow = {
    _uiId: 'r1',
    effect: 'Allow',
    actions: ['read', 'list'],
    resources: ['releases/*'],
    conditions,
  } as PermissionRow;
  const onChange = vi.fn();
  renderWithQuery(<PermissionEditor permissions={[row]} onChange={onChange} />);
  expect(screen.getByText(/set by the API or YAML/i)).toBeInTheDocument();
  expect(screen.getByText('releases/tmp/*')).toBeInTheDocument();
  expect(screen.getByText('10.0.0.0/8, 192.168.0.0/16')).toBeInTheDocument();

  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText(/192\.168\.0\.0\/16, 10/), '1.2.3.4/32');
  const last = onChange.mock.calls[onChange.mock.calls.length - 1][0][0] as PermissionRow;
  expect(last.conditions?.StringNotLike).toEqual(conditions.StringNotLike);
  expect(last.conditions?.NotIpAddress).toEqual(conditions.NotIpAddress);
});
