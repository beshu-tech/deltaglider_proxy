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
