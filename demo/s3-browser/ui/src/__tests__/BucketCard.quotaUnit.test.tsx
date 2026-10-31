/**
 * Docs-audit finding 5: the Quota field said "GB" but converted with 1024³
 * (GiB), and the quota chip said "GB" for the same GiB. Both now say GiB,
 * the unit the conversion uses (`quota_bytes` is plain bytes on the wire).
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { afterEach, expect, test, vi } from 'vitest';
import BucketCard from '../components/BucketCard';
import { DEFAULT_ROW_FIELDS, type BucketPolicyRow } from '../components/bucketPolicyPayload';
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
  expect(screen.getByText('Quota 10.0 GiB')).toBeInTheDocument();
  const input = screen.getByRole('textbox', { name: 'Quota' });
  expect(input).toHaveValue('10');
  expect(screen.getByText('GiB')).toBeInTheDocument();
  expect(screen.queryByText('GB')).not.toBeInTheDocument();

  const user = userEvent.setup();
  await user.clear(input);
  await user.type(input, '2');
  expect(onPatch).toHaveBeenLastCalledWith({ quota_bytes: 2 * GIB });
});

/**
 * UX harvest C20: the number field turned "-5" and "abc" into 0 (a quota
 * that refuses every upload) without a word, and took 999999999 GiB. The
 * field now keeps the text, says what is wrong, stages nothing, and tells
 * the parent (the dirty bar counts the error).
 */
test.each([
  ['-5', 'Enter a number of GiB, such as 50 or 0.5, or leave it empty for no quota.'],
  ['abc', 'Enter a number of GiB, such as 50 or 0.5, or leave it empty for no quota.'],
  ['0', 'Enter a size above 0, or leave it empty for no quota.'],
  ['999999999', 'Enter at most 1048576 GiB (1 PiB).'],
])('quota %s is refused with a reason, never staged', async (typed, reason) => {
  const onPatch = vi.fn();
  const onFieldError = vi.fn();
  renderWithQuery(
    <BucketCard
      name="db-archive"
      row={{ ...DEFAULT_ROW_FIELDS, public_prefixes: [], _id: 'r1', name: 'db-archive', quota_bytes: 2 * GIB }}
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
      onFieldError={onFieldError}
    />,
  );
  const user = userEvent.setup();
  const input = screen.getByRole('textbox', { name: 'Quota' });
  await user.clear(input);
  onPatch.mockClear();
  await user.type(input, typed);
  await user.tab();
  expect(input).toHaveValue(typed);
  expect(input).toHaveAttribute('aria-invalid', 'true');
  expect(screen.getByRole('alert')).toHaveTextContent(reason);
  expect(onPatch.mock.calls.some(([p]) => p.quota_bytes === 0)).toBe(false);
  expect(onFieldError).toHaveBeenLastCalledWith(reason);
});

test('an empty quota stages "no quota"', async () => {
  const onPatch = vi.fn();
  renderWithQuery(
    <BucketCard
      name="db-archive"
      row={{ ...DEFAULT_ROW_FIELDS, public_prefixes: [], _id: 'r1', name: 'db-archive', quota_bytes: 2 * GIB }}
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
  await userEvent.setup().clear(screen.getByRole('textbox', { name: 'Quota' }));
  expect(onPatch).toHaveBeenLastCalledWith({ quota_bytes: null });
  expect(screen.queryByRole('alert')).not.toBeInTheDocument();
});

/**
 * UX harvest D59: typing 999999999 staged 999999 GiB on the way (a valid
 * prefix), so the chip read "Quota 976.6 TiB" beside the error "Enter at
 * most 1048576 GiB". While the field is invalid, the chip says so.
 */
test('while the quota text is invalid, the chip says so instead of a size typed on the way', async () => {
  function Harness() {
    const [row, setRow] = useState<BucketPolicyRow>({ ...DEFAULT_ROW_FIELDS, public_prefixes: [], _id: 'r1', name: 'releases', quota_bytes: 10 * GIB });
    return (
      <BucketCard
        name="releases"
        row={row}
        real
        expanded
        onToggle={() => {}}
        backends={[]}
        defaultBackend={null}
        globalCompressionOn
        globalRatio={0.75}
        onPatch={(p) => setRow((r) => ({ ...r, ...p }))}
        onPrefixesChange={() => {}}
        inputRadius={{ borderRadius: 6 }}
      />
    );
  }
  renderWithQuery(<Harness />);
  const user = userEvent.setup();
  const input = screen.getByRole('textbox', { name: 'Quota' });
  await user.clear(input);
  await user.type(input, '999999999');
  expect(screen.getByText('Enter at most 1048576 GiB (1 PiB).')).toBeInTheDocument();
  expect(screen.queryByText(/^Quota \d/)).toBeNull();
  expect(screen.getByText('Quota: fix the value')).toBeInTheDocument();
  await user.clear(input);
  await user.type(input, '20');
  expect(screen.getByText('Quota 20.0 GiB')).toBeInTheDocument();
});

/** UX harvest D9 class: the delta size cutoff clamped "5" to 1 on blur. */
test('a delta size cutoff above 1 stays as typed, with a reason, and is not staged', async () => {
  const onPatch = vi.fn();
  const onFieldError = vi.fn();
  renderWithQuery(
    <BucketCard
      name="releases"
      row={{ ...DEFAULT_ROW_FIELDS, public_prefixes: [], _id: 'r1', name: 'releases', max_delta_ratio: 0.5 }}
      real
      expanded
      onToggle={() => {}}
      backends={[]}
      defaultBackend={null}
      globalCompressionOn
      globalRatio={0.75}
      onPatch={onPatch}
      onPrefixesChange={() => {}}
      onFieldError={onFieldError}
      inputRadius={{ borderRadius: 6 }}
    />,
  );
  const user = userEvent.setup();
  const input = screen.getByRole('textbox', { name: 'Delta size cutoff' });
  await user.clear(input);
  await user.type(input, '5');
  await user.tab();
  expect(input).toHaveValue('5');
  expect(screen.getByText('Enter a number from 0 to 1, such as 0.75, or leave it empty for the global default.')).toBeInTheDocument();
  expect(onPatch).not.toHaveBeenCalledWith({ max_delta_ratio: 5 });
  expect(onPatch).not.toHaveBeenCalledWith({ max_delta_ratio: 1 });
  expect(onFieldError).toHaveBeenLastCalledWith('Enter a number from 0 to 1, such as 0.75, or leave it empty for the global default.');
});
