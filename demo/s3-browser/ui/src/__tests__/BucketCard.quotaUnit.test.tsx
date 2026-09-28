/**
 * Docs-audit finding 5: the Quota field said "GB" but converted with 1024³
 * (GiB), and the quota chip said "GB" for the same GiB. Both now say GiB,
 * the unit the conversion uses (`quota_bytes` is plain bytes on the wire).
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import BucketCard from '../components/BucketCard';
import { DEFAULT_ROW_FIELDS } from '../components/bucketPolicyPayload';
import { GIB } from '../savings';
import { renderWithQuery } from '../test/render';

afterEach(() => vi.unstubAllGlobals());

test('the quota field and chip label GiB and write GiB × 1024³ bytes', async () => {
  const onPatch = vi.fn();
  renderWithQuery(
    <BucketCard
      name="releases"
      row={{ ...DEFAULT_ROW_FIELDS, public_prefixes: [], _id: 'r1', name: 'releases', quota_bytes: 10 * GIB }}
      real
      expanded
      onToggle={() => {}}
      backends={[]}
      defaultBackend={null}
      globalCompressionOn
      globalRatio={0.75}
      onPatch={onPatch}
      onPrefixesChange={() => {}}
      inputRadius={{ borderRadius: 6 }}
    />,
  );
  expect(screen.getByText('≤ 10.0 GiB')).toBeInTheDocument();
  const input = screen.getByRole('spinbutton', { name: 'Quota' });
  expect(input).toHaveValue('10.0'); // step 0.1 shows one decimal
  expect(screen.getByText('GiB')).toBeInTheDocument();
  expect(screen.queryByText('GB')).not.toBeInTheDocument();

  const user = userEvent.setup();
  await user.clear(input);
  await user.type(input, '2');
  expect(onPatch).toHaveBeenLastCalledWith({ quota_bytes: 2 * GIB });
});
