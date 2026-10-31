/**
 * The analytics hero never reads more than 100% smaller.
 *
 * Pins the browser-review finding: the lead figure was the compression
 * ratio × 100 ("2.7× smaller" rendered as "270% smaller"), next to a
 * "63.0% saved" pill. A thing cannot be more than 100% smaller. The lead
 * is now the saved share of the original bytes, the same number as the pill.
 */
import { render, screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import HeroSavingsPanel from '../components/HeroSavingsPanel';
import { summarizeScopeSavings } from '../savings';
import { writeStorage } from '../safeStorage';

test('lead percent is the saved share and agrees with the pill', () => {
  // Skip the count-up: the numbers render at their final value.
  writeStorage('dgp-hero-animated', '1', 'session');
  const original = 270 * 1024 ** 3;
  const stored = 100 * 1024 ** 3;
  render(
    <HeroSavingsPanel
      totalOriginal={original}
      totalStored={stored}
      savingsPercent={summarizeScopeSavings(original, stored).pctOneDecimal}
      monthlySavings={1}
      costRate={0.023}
      onChangeCostRate={() => {}}
      totalObjects={10}
      bucketCount={1}
      biggestSave={null}
      referenceShare={null}
      unscannedCount={0}
      onScanMissing={() => {}}
      liveScanning={false}
    />,
  );
  const lead = screen.getByLabelText(/percent smaller on disk/);
  expect(lead).toHaveAccessibleName('62.9 percent smaller on disk');
  expect(lead.textContent).toBe('62.9%');
  expect(screen.getByText('smaller on disk')).toBeInTheDocument();
  expect(document.body.textContent).not.toMatch(/270\s*%/);
});

function renderHero(unscannedCount: number, bucketCount = 4) {
  writeStorage('dgp-hero-animated', '1', 'session');
  const original = 28.6 * 1024 ** 2;
  const stored = 21 * 1024 ** 2;
  return render(
    <HeroSavingsPanel
      totalOriginal={original}
      totalStored={stored}
      savingsPercent={summarizeScopeSavings(original, stored).pctOneDecimal}
      monthlySavings={0}
      costRate={0.023}
      onChangeCostRate={() => {}}
      totalObjects={20}
      bucketCount={bucketCount}
      biggestSave={null}
      referenceShare={null}
      unscannedCount={unscannedCount}
      onScanMissing={() => {}}
      liveScanning={false}
    />,
  );
}

// UX harvest B35: figures from usage counters were shown as facts while
// every bucket read "not scanned".
test('the figures are labelled an estimate while a bucket is not scanned', () => {
  renderHero(4);
  expect(screen.getByText('Estimated saving')).toBeInTheDocument();
  expect(screen.getByText(/4 of 4 buckets are not scanned/)).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Scan all buckets' })).toBeInTheDocument();
});

// UX harvest C12: the hero is the one scan call to action; with some
// buckets scanned it names only the rest.
test('the call to action names only the buckets with no scan', () => {
  renderHero(2);
  expect(screen.getByRole('button', { name: 'Scan 2 more buckets' })).toBeInTheDocument();
});

test('the figures are not an estimate when every bucket is scanned', () => {
  renderHero(0);
  expect(screen.getByText('Storage saved')).toBeInTheDocument();
  expect(screen.queryByText('Estimated saving')).toBeNull();
});

// UX harvest D49: an estimate was sold in green at 96 px.
test('an estimate shows "≈" and no gradient ink; a scanned figure does not', () => {
  renderHero(4);
  const lead = screen.getByLabelText(/percent smaller on disk/);
  expect(lead.textContent?.startsWith('≈')).toBe(true);
  expect(lead.style.backgroundClip).toBe('');
  expect(screen.getByText(/^≈.* saved$/)).toBeInTheDocument();
});

// UX harvest D15: a saving had a minus sign.
test('the biggest saving reads as a saving', () => {
  writeStorage('dgp-hero-animated', '1', 'session');
  render(
    <HeroSavingsPanel
      totalOriginal={100 * 1024 ** 2}
      totalStored={50 * 1024 ** 2}
      savingsPercent={50}
      monthlySavings={0}
      costRate={0.023}
      onChangeCostRate={() => {}}
      totalObjects={2}
      bucketCount={1}
      biggestSave={{ bucket: 'releases', bytes: 7.6 * 1000 * 1000 }}
      referenceShare={null}
      unscannedCount={0}
      onScanMissing={() => {}}
      liveScanning={false}
    />,
  );
  expect(screen.getByText(/^releases saves the most: /)).toBeInTheDocument();
  expect(document.body.textContent).not.toMatch(/−\s*7/);
});
