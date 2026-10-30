/**
 * UX harvest B28/B29 + axe label-content-name-mismatch: the collapsed row's
 * chips say what they mean in plain words, keep the case of names and units,
 * and the row's accessible name is its visible text.
 */
import { screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import BucketCard from '../components/BucketCard';
import { DEFAULT_ROW_FIELDS } from '../components/bucketPolicyPayload';
import { GIB } from '../savings';
import { renderWithQuery } from '../test/render';

function renderCard(defaultBackend: string | null, originBackend: string | null) {
  renderWithQuery(
    <BucketCard
      name="releases"
      row={{
        ...DEFAULT_ROW_FIELDS, public_prefixes: [], _id: 'r1', name: 'releases',
        quota_bytes: GIB, delta_add: ['pkl', 'npz'], delta_remove: ['sql'],
      }}
      real
      expanded={false}
      onToggle={() => {}}
      backends={[]}
      defaultBackend={defaultBackend}
      originBackend={originBackend}
      globalCompressionOn
      globalRatio={0.75}
      onPatch={() => {}}
      onPrefixesChange={() => {}}
      inputRadius={{ borderRadius: 6 }}
    />,
  );
}

test('a bucket on the one unnamed backend says "Default backend", not "on default"', () => {
  renderCard(null, 'default');
  expect(screen.getByText('Default backend')).toBeInTheDocument();
  expect(screen.queryByText(/^On default/)).toBeNull();
});

test('chips are plain words with the unit in its own case', () => {
  renderCard('local-disk', 'local-disk');
  expect(screen.getByText('File types: 2 added, 1 removed')).toBeInTheDocument();
  const quota = screen.getByText('Quota 1.0 GiB');
  expect(quota.style.textTransform).not.toBe('uppercase');
});

test("the row's accessible name is its visible text", () => {
  renderCard('local-disk', 'local-disk');
  const row = screen.getByRole('button', { name: /^releases / });
  expect(row).not.toHaveAttribute('aria-label');
  const name = row.textContent ?? '';
  for (const chip of ['Default backend · local-disk', 'Private', 'Quota 1.0 GiB']) expect(name).toContain(chip);
});

test('on a phone the row shows two chips and "+N" with the others in its title', () => {
  const width = window.innerWidth;
  window.innerWidth = 375;
  try {
    renderCard('local-disk', 'local-disk');
    const more = screen.getByText('+3');
    // The title is on the chip itself (the text node sits in it).
    expect(more.closest('[title]')).toHaveAttribute('title', 'Private\nQuota 1.0 GiB\nFile types: 2 added, 1 removed');
    expect(screen.queryByText('Quota 1.0 GiB')).toBeNull();
    expect(screen.getByText('Default backend · local-disk')).toBeInTheDocument();
  } finally {
    window.innerWidth = width;
  }
});
