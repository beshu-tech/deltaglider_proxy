// pricing.test.ts — vitest unit tests for the calculator math module.
// Mirrors the Rust codebase's pure-function-first testing principle:
// no DOM, no React, just inputs → outputs verified at the function boundary.

import { describe, it, expect } from 'vitest';
import {
  calculate,
  formatUsd,
  formatTb,
  buildMarkdown,
  SOURCE_TB_LOWER_THRESHOLD,
  type CalculatorInputs,
} from './pricing';
import { FREE_GRANT_TB, COMMERCIAL_PRICE_USD } from './brackets';

/** Default inputs matching the calculator's default slider positions. */
const defaultInputs = (overrides: Partial<CalculatorInputs> = {}): CalculatorInputs => ({
  sourceTb: 30,
  regions: 2,
  costPerGbMonthUsd: 0.023,
  compressionRatio: 10,
  annualGrowthRate: 0.30,
  ...overrides,
});

describe('calculate', () => {
  it('free under the grant: 50 TB source × 10× = 5 TB stored → free', () => {
    const result = calculate(defaultInputs({ sourceTb: 50 }));
    expect(result.kind).toBe('free');
  });

  it('free at the grant boundary: 150 TB source × 10× = 15 TB stored → still free', () => {
    // The grant is inclusive: footprint ≤ 15 TB stays free.
    const result = calculate(defaultInputs({ sourceTb: 150 }));
    expect(result.kind).toBe('free');
  });

  it('commercial above the grant: 300 TB source × 10× = 30 TB stored → Commercial ($5k)', () => {
    const result = calculate(defaultInputs({ sourceTb: 300 }));
    expect(result.kind).toBe('ok');
    if (result.kind === 'ok') {
      expect(result.bracket.id).toBe('commercial');
      expect(result.bracket.priceUsd).toBe(COMMERCIAL_PRICE_USD);
    }
  });

  it('commercial stays flat at any scale: 3000 TB source × 10× = 300 TB stored → same $5k', () => {
    const result = calculate(defaultInputs({ sourceTb: 3000 }));
    expect(result.kind).toBe('ok');
    if (result.kind === 'ok') {
      expect(result.bracket.id).toBe('commercial');
      expect(result.bracket.priceUsd).toBe(COMMERCIAL_PRICE_USD);
    }
  });

  it('belowThreshold sentinel at 0.5 TB source', () => {
    const result = calculate(defaultInputs({ sourceTb: 0.5 }));
    expect(result.kind).toBe('belowThreshold');
  });

  it('belowThreshold lower bound is strict (< 1 TB)', () => {
    expect(calculate(defaultInputs({ sourceTb: 0.99 })).kind).toBe('belowThreshold');
    expect(calculate(defaultInputs({ sourceTb: 1.0 })).kind).not.toBe('belowThreshold');
  });

  it('SOURCE_TB_LOWER_THRESHOLD is exported and = 1', () => {
    expect(SOURCE_TB_LOWER_THRESHOLD).toBe(1);
  });

  it('FREE_GRANT_TB matches the license grant (15 TB)', () => {
    expect(FREE_GRANT_TB).toBe(15);
  });

  it('negativeNet when the license costs more than the savings (over grant, cheap storage, low ratio)', () => {
    // 40 TB × 2× compression = 20 TB stored (over the grant), 1 region,
    // $0.001/GB-month → savings ≈ $246/yr vs the $5k Commercial plan.
    const result = calculate({
      sourceTb: 40,
      regions: 1,
      costPerGbMonthUsd: 0.001,
      compressionRatio: 2,
      annualGrowthRate: 0,
    });
    expect(result.kind).toBe('negativeNet');
    if (result.kind === 'negativeNet') {
      expect(result.licenseCost).toBe(COMMERCIAL_PRICE_USD);
      expect(result.savings).toBeLessThan(COMMERCIAL_PRICE_USD);
    }
  });

  it('breakdown arithmetic matches (golden test for 300 TB source)', () => {
    // 300 TB source × 10× compression = 30 TB stored.
    // Storage: 30 TB × 1024 GB × 12 mo × $0.023/GB/mo = $8,478.72/yr per region post-DGP
    //          300 TB                                  = $84,787.20/yr per region today
    // ×2 regions:                                       = $16,957.44 (post) / $169,574.40 (today)
    // Egress: 1 replica region × 1024 GB × growth × $0.02/GB
    //         today: 300 × 0.30 × 1024 × 1 × $0.02 = $1,843.20
    //         dgp:    30 × 0.30 × 1024 × 1 × $0.02 = $184.32
    const result = calculate(defaultInputs({ sourceTb: 300 }));
    expect(result.kind).toBe('ok');
    if (result.kind === 'ok') {
      // First line: storage footprint in TB
      expect(result.lines[0].today).toBeCloseTo(300, 5);
      expect(result.lines[0].dgp).toBeCloseTo(30, 5);
      expect(result.lines[0].unit).toBe('tb');

      // Per-region storage cost
      expect(result.lines[1].today).toBeCloseTo(300 * 1024 * 12 * 0.023, 1);
      expect(result.lines[1].dgp).toBeCloseTo(30 * 1024 * 12 * 0.023, 1);

      // ×2 regions
      expect(result.lines[2].today).toBeCloseTo(300 * 1024 * 12 * 0.023 * 2, 1);
      expect(result.lines[2].dgp).toBeCloseTo(30 * 1024 * 12 * 0.023 * 2, 1);

      // Egress: 1 replica region × growth × $0.02/GB
      expect(result.lines[3].today).toBeCloseTo(300 * 0.30 * 1024 * 1 * 0.02, 1);
      expect(result.lines[3].dgp).toBeCloseTo(30 * 0.30 * 1024 * 1 * 0.02, 1);

      // Subtotal — storage + transfer
      const expectedTodaySubtotal = 300 * 1024 * 12 * 0.023 * 2 + 300 * 0.30 * 1024 * 0.02;
      expect(result.lines[4].today).toBeCloseTo(expectedTodaySubtotal, 1);

      // Flat license + net savings
      expect(result.bracket.id).toBe('commercial');
      expect(result.netSavings).toBeCloseTo(result.savings - COMMERCIAL_PRICE_USD, 1);
    }
  });

  it('property: savings = todaySubtotal - dgpSubtotal across diverse inputs (±$1)', () => {
    const testCases: Partial<CalculatorInputs>[] = [
      {}, // defaults
      { sourceTb: 50, regions: 1 },
      { sourceTb: 100, regions: 3 },
      { sourceTb: 200, regions: 4, costPerGbMonthUsd: 0.020 },
      { sourceTb: 25, compressionRatio: 15 },
      { sourceTb: 80, annualGrowthRate: 0.5 },
      { sourceTb: 600, regions: 2 },
    ];
    for (const tc of testCases) {
      const result = calculate(defaultInputs(tc));
      if (result.kind !== 'ok' && result.kind !== 'free') continue;
      const subtotalLine = result.lines.find((l) => l.label.startsWith('Subtotal'));
      expect(subtotalLine, `case ${JSON.stringify(tc)}`).toBeDefined();
      const computedSavings = subtotalLine!.today - subtotalLine!.dgp;
      expect(computedSavings, `case ${JSON.stringify(tc)}`).toBeCloseTo(result.savings, 0);
    }
  });

  it('warnings: low compression ratio (< 3×) emits lowCompressionRatio chip', () => {
    // 50 TB × 2× = 25 TB stored → over the grant → 'ok' with the warning.
    const result = calculate(defaultInputs({ sourceTb: 50, compressionRatio: 2 }));
    expect(result.kind).toBe('ok');
    if (result.kind === 'ok') {
      expect(result.warnings).toContain('lowCompressionRatio');
    }
  });

  it('warnings: cheap backend (< $0.005/GB-month) emits cheapBackendAlready chip (free kind too)', () => {
    // 100 TB × 10× = 10 TB stored → under the grant → 'free' carries warnings.
    const result = calculate(defaultInputs({ sourceTb: 100, costPerGbMonthUsd: 0.003 }));
    expect(result.kind).toBe('free');
    if (result.kind === 'free') {
      expect(result.warnings).toContain('cheapBackendAlready');
    }
  });

  it('regions = 1 should not multiply egress (no replicas)', () => {
    const result = calculate(defaultInputs({ sourceTb: 50, regions: 1 }));
    expect(result.kind).toBe('free');
    if (result.kind === 'free') {
      const egressLine = result.lines.find((l) => l.label.includes('egress'));
      expect(egressLine?.today).toBe(0);
      expect(egressLine?.dgp).toBe(0);
    }
  });
});

describe('formatUsd', () => {
  it('formats round numbers with thousands separators', () => {
    expect(formatUsd(1234)).toBe('$1,234');
    expect(formatUsd(123_456)).toBe('$123,456');
    expect(formatUsd(0)).toBe('$0');
  });

  it('compact mode collapses to k / M', () => {
    expect(formatUsd(10_000, { compact: true })).toBe('$10k');
    expect(formatUsd(30_000, { compact: true })).toBe('$30k');
    expect(formatUsd(1_200_000, { compact: true })).toBe('$1.2M');
    expect(formatUsd(5_000_000, { compact: true })).toBe('$5M');
  });

  it('rounds decimals to nearest dollar', () => {
    expect(formatUsd(1234.7)).toBe('$1,235');
    expect(formatUsd(1234.4)).toBe('$1,234');
  });
});

describe('formatTb', () => {
  it('renders sub-1-TB as GB', () => {
    expect(formatTb(0.5)).toBe('512 GB');
    expect(formatTb(0.1)).toBe('102 GB');
  });

  it('renders 1-9 TB with one decimal', () => {
    expect(formatTb(1)).toBe('1.0 TB');
    expect(formatTb(2.5)).toBe('2.5 TB');
  });

  it('renders 10+ TB as integer with separator', () => {
    expect(formatTb(30)).toBe('30 TB');
    expect(formatTb(1500)).toBe('1,500 TB');
  });
});

describe('buildMarkdown', () => {
  it('produces a stable markdown snippet for the 300 TB / 2 region Commercial case', () => {
    const inputs = defaultInputs({ sourceTb: 300 });
    const result = calculate(inputs);
    const md = buildMarkdown(inputs, result);
    expect(md).toContain('# DeltaGlider savings estimate');
    expect(md).toContain('300 TB');      // source footprint in inputs
    expect(md).toContain('Commercial');  // tier name
    expect(md).toContain('$5k/year');    // tier priceLabel
    expect(md).toContain('| Line |');    // breakdown table header
  });

  it('belowThreshold markdown is short and points at the free build', () => {
    const inputs = defaultInputs({ sourceTb: 0.5 });
    const result = calculate(inputs);
    const md = buildMarkdown(inputs, result);
    expect(md).toContain('Below 1 TB');
    expect(md).toContain('free build');
  });

  it('free-grant markdown says DeltaGlider costs nothing', () => {
    const inputs = defaultInputs({ sourceTb: 50 });
    const result = calculate(inputs);
    const md = buildMarkdown(inputs, result);
    expect(md).toContain('15 TB free grant');
    expect(md).toContain('costs you nothing');
  });
});
