// The scan banner uses the labels of the analytics page: "Scan all buckets"
// or "Scan N more buckets" while some bucket has no scan, and "Re-scan all"
// only once a scan exists. It said "Scan missing (N)" and "Re-scan all (N)".
import { screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import ScanStatusBanner from '../components/ScanStatusBanner';
import { useColors } from '../ThemeContext';
import { renderWithQuery } from '../test/render';

function Banner({ cached, total }: { cached: number; total: number }) {
  const colors = useColors();
  return (
    <ScanStatusBanner
      colors={colors}
      liveProgress={null}
      queue={[]}
      cachedCount={cached}
      bucketCount={total}
      newestCompletedAt={null}
      oldestCompletedAt={null}
      unscannedCount={total - cached}
      staleCount={0}
      allBucketsCount={total}
      scansLoaded
      isScanning={false}
      onStop={() => {}}
      onScanMissing={() => {}}
      onRescanAll={() => {}}
    />
  );
}

test('no scan yet: "Scan all buckets", no "Re-scan all"', () => {
  renderWithQuery(<Banner cached={0} total={4} />);
  expect(screen.getByRole('button', { name: /Scan all buckets/ })).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: /Re-scan all/ })).toBeNull();
  expect(screen.queryByText(/Scan missing|Run full scan/)).toBeNull();
});

test('some scanned: "Scan 2 more buckets" and "Re-scan all"', () => {
  renderWithQuery(<Banner cached={2} total={4} />);
  expect(screen.getByRole('button', { name: /Scan 2 more buckets/ })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: /^reload Re-scan all$/ })).toBeInTheDocument();
});

test('all scanned: only "Re-scan all"', () => {
  renderWithQuery(<Banner cached={4} total={4} />);
  expect(screen.getAllByRole('button')).toHaveLength(1);
  expect(screen.getByRole('button', { name: /Re-scan all/ })).toBeInTheDocument();
});
