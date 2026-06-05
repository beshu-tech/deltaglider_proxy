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
  it('starter bracket at 50 TB source × 10× = 5 TB stored → Starter ($5k)', () => {
    // 50 TB × 2 regions produces enough savings to clear the $5k Starter
    // floor. (At 5 TB source, savings ~$2.8k < $5k → negativeNet, which
    // is the correct behaviour but not what we test for here.)
    const result = calculate(defaultInputs({ sourceTb: 50 }));
    expect(result.kind).toBe('ok');
    if (result.kind === 'ok') {
      expect(result.bracket.id).toBe('starter');
      expect(result.bracket.priceUsd).toBe(5_000);
    }
  });

  it('growth bracket at 300 TB source × 10× = 30 TB stored → Growth ($15k)', () => {
    const result = calculate(defaultInputs({ sourceTb: 300 }));
    expect(result.kind).toBe('ok');
    if (result.kind === 'ok') {
      expect(result.bracket.id).toBe('growth');
      expect(result.bracket.priceUsd).toBe(15_000);
    }
  });

  it('scale bracket at 1500 TB source × 10× = 150 TB stored → Scale ($30k)', () => {
    const result = calculate(defaultInputs({ sourceTb: 1500 }));
    expect(result.kind).toBe('ok');
    if (result.kind === 'ok') {
      expect(result.bracket.id).toBe('scale');
      expect(result.bracket.priceUsd).toBe(30_000);
    }
  });

  it('enterprise sentinel at 3000 TB source × 10× = 300 TB stored', () => {
    const result = calculate(defaultInputs({ sourceTb: 3000 }));
    expect(result.kind).toBe('enterprise');
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

  it('negativeNet when support cost exceeds savings (e.g. low compression + small footprint)', () => {
    // 2 TB × 2× compression × 1 region × $0.001/GB → ~$2.45k savings/yr
    // vs Starter $5k → negative.
    const result = calculate({
      sourceTb: 2,
      regions: 1,
      costPerGbMonthUsd: 0.001,
      compressionRatio: 2,
      annualGrowthRate: 0,
    });
    expect(result.kind).toBe('negativeNet');
    if (result.kind === 'negativeNet') {
      expect(result.supportCost).toBe(5_000);
      expect(result.savings).toBeLessThan(5_000);
    }
  });

  it('breakdown arithmetic matches (golden test for 300 TB source — the v5 §5.4 example case)', () => {
    // 300 TB source × 10× compression = 30 TB stored.
    // The result-card mockup in v5 §5.4 documents this exact case.
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

      // Bracket pick + savings
      expect(result.bracket.id).toBe('growth');
      expect(result.netSavings).toBeCloseTo(result.savings - 15_000, 1);
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
      if (result.kind !== 'ok' && result.kind !== 'enterprise') continue;
      const subtotalLine = result.lines.find((l) => l.label.startsWith('Subtotal'));
      expect(subtotalLine, `case ${JSON.stringify(tc)}`).toBeDefined();
      const computedSavings = subtotalLine!.today - subtotalLine!.dgp;
      expect(computedSavings, `case ${JSON.stringify(tc)}`).toBeCloseTo(result.savings, 0);
    }
  });

  it('warnings: low compression ratio (< 3×) emits lowCompressionRatio chip', () => {
    const result = calculate(defaultInputs({ sourceTb: 50, compressionRatio: 2 }));
    if (result.kind === 'ok') {
      expect(result.warnings).toContain('lowCompressionRatio');
    }
  });

  it('warnings: cheap backend (< $0.005/GB-month) emits cheapBackendAlready chip', () => {
    // Need to ensure the math still produces a positive bracket result with cheap storage.
    // Backblaze-cheap pricing with a lot of data still produces savings.
    const result = calculate(defaultInputs({ sourceTb: 100, costPerGbMonthUsd: 0.003 }));
    if (result.kind === 'ok') {
      expect(result.warnings).toContain('cheapBackendAlready');
    }
  });

  it('regions = 1 should not multiply egress (no replicas)', () => {
    // 50 TB / 10× = 5 TB stored → Starter ($5k). Single-region savings
    // at 50 TB × $0.023 × 1024 × 12 = $14.1k today vs $1.4k post = $12.7k
    // savings, which clears the $5k floor.
    const result = calculate(defaultInputs({ sourceTb: 50, regions: 1 }));
    expect(result.kind).toBe('ok');
    if (result.kind === 'ok') {
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
  it('produces a stable markdown snippet for the 300 TB / 2 region "Growth" case', () => {
    // 300 TB / 10× = 30 TB stored → Growth bracket per the v5 §5.4 mockup.
    const inputs = defaultInputs({ sourceTb: 300 });
    const result = calculate(inputs);
    const md = buildMarkdown(inputs, result);
    expect(md).toContain('# DeltaGlider savings estimate');
    expect(md).toContain('300 TB');    // source footprint in inputs
    expect(md).toContain('Growth');    // bracket name
    expect(md).toContain('$15k/year'); // bracket priceLabel
    expect(md).toContain('| Line |');  // breakdown table header
  });

  it('belowThreshold markdown is short and points at OSS', () => {
    const inputs = defaultInputs({ sourceTb: 0.5 });
    const result = calculate(inputs);
    const md = buildMarkdown(inputs, result);
    expect(md).toContain('Below 1 TB');
    expect(md).toContain('OSS build');
  });

  it('enterprise markdown defers to sales contact', () => {
    const inputs = defaultInputs({ sourceTb: 3000 });
    const result = calculate(inputs);
    const md = buildMarkdown(inputs, result);
    expect(md).toContain('Enterprise');
    expect(md).toContain('contact@beshu.tech');
  });
});
