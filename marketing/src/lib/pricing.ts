// pricing.ts — pure math for the savings calculator.
//
// Per v5 plan §5.6: zero DOM, zero React, fully unit-testable in isolation.
// All exports here are pure functions of their arguments. The UI layer
// (PricingCalculator.tsx) imports and renders the results.

import {
  bracketForFootprintTb,
  type Bracket,
} from './brackets';

/** Inputs the calculator UI gathers from the visitor. */
export interface CalculatorInputs {
  /** Current artifact footprint on the visitor's existing storage, in TB. */
  sourceTb: number;
  /** Number of regions the visitor replicates to today. */
  regions: number;
  /** Storage cost per GB per month, in USD. */
  costPerGbMonthUsd: number;
  /**
   * Compression ratio. Default 10×. Slider lets the visitor override.
   * Conservative — geometric mean of verified ReadonlyREST migration
   * data points (4.1× and 88.5×) is ~19×; we round down to 10× as a
   * defensible underpromise per v5 plan §1.
   */
  compressionRatio: number;
  /** Annual data growth as a decimal (0.30 = 30%/yr). */
  annualGrowthRate: number;
}

/** A line item shown in the breakdown table. */
export interface BreakdownLine {
  /** Label shown in the leftmost column. */
  label: string;
  /** Today value as USD/yr (already-formatted as number for arithmetic). */
  today: number;
  /** Post-DeltaGlider value as USD/yr. */
  dgp: number;
  /** Optional unit override; defaults to USD/yr. Used by "Storage footprint". */
  unit?: 'tb' | 'usd';
}

/** What the calculator produces — either a full result OR a sentinel
 * telling the UI to swap to a "self-disqualify" card. */
export type CalculatorResult =
  | { kind: 'belowThreshold' }
  | { kind: 'negativeNet'; savings: number; supportCost: number }
  | { kind: 'enterprise'; savings: number; lines: BreakdownLine[] }
  | { kind: 'ok'; bracket: Bracket; savings: number; netSavings: number; lines: BreakdownLine[]; warnings: Warning[] };

/** Soft warnings rendered as warning chips next to the result card.
 * Don't suppress the result — surface a caveat. */
export type Warning =
  | 'lowCompressionRatio'   // < 3× — DGP may not be the right fit
  | 'cheapBackendAlready';  // < $0.005/GB-month — savings will be small

/** USD/TB/year given a per-GB-month rate. 1 TB = 1024 GB, 12 months. */
function tbPerYearCost(tb: number, costPerGbMonthUsd: number): number {
  return tb * 1024 * 12 * costPerGbMonthUsd;
}

/** AWS S3 inter-region replication egress is $0.02/GB. */
const REPLICATION_EGRESS_USD_PER_GB = 0.02;

/** Below this source footprint the math is too small to matter — the
 * calculator swaps to a "come back later" message. */
export const SOURCE_TB_LOWER_THRESHOLD = 1;

/** Above this stored footprint the visitor goes to Enterprise — talk to sales. */
export const SCALE_TB_UPPER = 250;

/** Compress everything into the result.
 *
 * Algorithm:
 *   storedFootprint = sourceTb / compressionRatio
 *   today storage   = sourceTb * 12 * 1024 * costPerGbMonth  (per region, × regions)
 *   dgp storage     = storedFootprint * 12 * 1024 * costPerGbMonth (per region, × regions)
 *   today egress    = sourceTb * growthRate * 1024 * (regions - 1 replicas) * $0.02
 *   dgp egress      = storedFootprint * growthRate * 1024 * (regions - 1 replicas) * $0.02
 *   savings         = (todayStorage + todayEgress) - (dgpStorage + dgpEgress)
 *   supportCost     = bracket.priceUsd
 *   netSavings      = savings - supportCost
 *
 * Then bracket-selection happens against `storedFootprint`.
 */
export function calculate(inputs: CalculatorInputs): CalculatorResult {
  const { sourceTb, regions, costPerGbMonthUsd, compressionRatio, annualGrowthRate } = inputs;

  // Below threshold: don't bother.
  if (sourceTb < SOURCE_TB_LOWER_THRESHOLD) {
    return { kind: 'belowThreshold' };
  }

  const storedFootprintTb = sourceTb / compressionRatio;

  // Storage costs — per region × number of regions.
  const todayStoragePerRegion = tbPerYearCost(sourceTb, costPerGbMonthUsd);
  const todayStorageTotal = todayStoragePerRegion * regions;
  const dgpStoragePerRegion = tbPerYearCost(storedFootprintTb, costPerGbMonthUsd);
  const dgpStorageTotal = dgpStoragePerRegion * regions;

  // Replication egress: only the replica regions (regions - 1) get charged,
  // and only for the newly-uploaded bytes per year (growth × footprint).
  const replicaRegions = Math.max(0, regions - 1);
  const todayEgress =
    sourceTb * annualGrowthRate * 1024 * replicaRegions * REPLICATION_EGRESS_USD_PER_GB;
  const dgpEgress =
    storedFootprintTb * annualGrowthRate * 1024 * replicaRegions * REPLICATION_EGRESS_USD_PER_GB;

  const todaySubtotal = todayStorageTotal + todayEgress;
  const dgpSubtotal = dgpStorageTotal + dgpEgress;
  const savings = todaySubtotal - dgpSubtotal;

  // Breakdown table rows.
  const lines: BreakdownLine[] = [
    { label: 'Storage footprint', today: sourceTb, dgp: storedFootprintTb, unit: 'tb' },
    { label: 'Storage cost (per region)', today: todayStoragePerRegion, dgp: dgpStoragePerRegion },
    {
      label: regions === 1 ? 'Single region (no replicas)' : `Replicated across ${regions} regions`,
      today: todayStorageTotal,
      dgp: dgpStorageTotal,
    },
    { label: 'Replication egress (new artifacts)', today: todayEgress, dgp: dgpEgress },
    { label: 'Subtotal — storage + transfer', today: todaySubtotal, dgp: dgpSubtotal },
  ];

  // Bracket selection based on stored footprint.
  const bracket = bracketForFootprintTb(storedFootprintTb);

  // Warnings: surfaced as chips alongside the result; don't override.
  const warnings: Warning[] = [];
  if (compressionRatio < 3) warnings.push('lowCompressionRatio');
  if (costPerGbMonthUsd < 0.005) warnings.push('cheapBackendAlready');

  // Enterprise: no fixed support price; show savings, defer to sales.
  if (bracket.id === 'enterprise' || bracket.priceUsd === null) {
    return { kind: 'enterprise', savings, lines };
  }

  const supportCost = bracket.priceUsd;
  const netSavings = savings - supportCost;

  // Bad-case: support costs more than savings.
  if (netSavings < 0) {
    return { kind: 'negativeNet', savings, supportCost };
  }

  // Add the support-cost line + total-annual-cost line to the breakdown.
  lines.push({
    label: 'DeltaGlider production support',
    today: 0,
    dgp: supportCost,
  });
  lines.push({
    label: 'Total annual cost',
    today: todaySubtotal,
    dgp: dgpSubtotal + supportCost,
  });

  return { kind: 'ok', bracket, savings, netSavings, lines, warnings };
}

/** Format a USD amount as "$1,234" or "$1.2M" depending on magnitude.
 * Pure formatting — keep arithmetic separate. */
export function formatUsd(amount: number, opts: { compact?: boolean } = {}): string {
  if (opts.compact && Math.abs(amount) >= 1_000_000) {
    return `$${(amount / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`;
  }
  if (opts.compact && Math.abs(amount) >= 1_000) {
    return `$${Math.round(amount / 1_000)}k`;
  }
  return `$${Math.round(amount).toLocaleString('en-US')}`;
}

/** Format a TB number — "300 TB" or "0.5 TB" or "1,500 TB". */
export function formatTb(tb: number): string {
  if (tb < 1) {
    return `${(tb * 1024).toFixed(0)} GB`;
  }
  if (tb < 10) {
    return `${tb.toFixed(1)} TB`;
  }
  return `${Math.round(tb).toLocaleString('en-US')} TB`;
}

/** Build a markdown snippet suitable for "Copy this estimate" → clipboard.
 * Visitor pastes it into a procurement doc or sends to their CFO. */
export function buildMarkdown(inputs: CalculatorInputs, result: CalculatorResult): string {
  const lines: string[] = [];
  lines.push('# DeltaGlider savings estimate');
  lines.push('');
  lines.push(`Source: https://deltaglider.com/pricing#calculator`);
  lines.push('');
  lines.push('## Inputs');
  lines.push('');
  lines.push(`- Current artifact footprint: ${formatTb(inputs.sourceTb)}`);
  lines.push(`- Regions: ${inputs.regions}`);
  lines.push(`- Storage cost: $${inputs.costPerGbMonthUsd}/GB/month`);
  lines.push(`- Compression ratio (assumed): ${inputs.compressionRatio}×`);
  lines.push(`- Annual growth: ${Math.round(inputs.annualGrowthRate * 100)}%`);
  lines.push('');

  if (result.kind === 'belowThreshold') {
    lines.push('## Verdict');
    lines.push('');
    lines.push('Below 1 TB — DeltaGlider isn\'t worth it at this scale. Use the OSS build for free.');
    return lines.join('\n');
  }
  if (result.kind === 'negativeNet') {
    lines.push('## Verdict');
    lines.push('');
    lines.push(`Storage savings (${formatUsd(result.savings)}) would be less than the support contract (${formatUsd(result.supportCost)}). Use the OSS build (free) or talk to us about a different fit.`);
    return lines.join('\n');
  }
  if (result.kind === 'enterprise') {
    lines.push('## Verdict');
    lines.push('');
    lines.push(`Approximate annual storage savings: **${formatUsd(result.savings)}**.`);
    lines.push('');
    lines.push('At this scale, you need an Enterprise contract — multi-region SLA, dedicated channel, custom terms. Email contact@beshu.tech.');
    return lines.join('\n');
  }
  // OK case
  lines.push('## Verdict');
  lines.push('');
  lines.push(`Approximate annual storage savings: **${formatUsd(result.savings)}**.`);
  lines.push(`Production support bracket: ${result.bracket.name} at ${result.bracket.priceLabel}.`);
  lines.push(`Net annual savings after support: **${formatUsd(result.netSavings)}**.`);
  lines.push('');
  lines.push('## Breakdown');
  lines.push('');
  lines.push('| Line | Today | With DeltaGlider |');
  lines.push('|---|---|---|');
  for (const line of result.lines) {
    const today = line.unit === 'tb' ? formatTb(line.today) : formatUsd(line.today);
    const dgp = line.unit === 'tb' ? formatTb(line.dgp) : formatUsd(line.dgp);
    lines.push(`| ${line.label} | ${today} | ${dgp} |`);
  }
  lines.push('');
  lines.push('---');
  lines.push('');
  lines.push('Conservative assumptions. Real numbers depend on your data — run `docker run deltaglider/proxy` against your bucket and read the Delta Efficiency Panel for a verified compression ratio.');
  return lines.join('\n');
}
