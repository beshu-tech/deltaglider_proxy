// PricingCalculator.tsx — the dynamic savings calculator React island.
//
// Per v5 plan §5: inputs → live result. No email gate, math visible on
// demand, "copy this estimate" markdown export. Pure component:
// imports pricing.ts for math, brackets.ts for tier definitions.
//
// Hydrates lazily via Astro's client:visible directive (only when
// scrolled into view). Pages without this island ship zero JS.

import { useMemo, useState } from 'react';
import {
  calculate,
  formatUsd,
  formatTb,
  buildMarkdown,
  type CalculatorInputs,
  type CalculatorResult,
  type BreakdownLine,
} from '../lib/pricing';

// Default slider positions per v5 plan §5.3.
const DEFAULTS: CalculatorInputs = {
  sourceTb: 30,
  regions: 2,
  costPerGbMonthUsd: 0.023,
  compressionRatio: 10,
  annualGrowthRate: 0.30,
};

const COST_SHORTCUTS = [
  { label: 'AWS Standard', value: 0.023 },
  { label: 'GCS Standard', value: 0.020 },
  { label: 'Azure Hot', value: 0.0184 },
  { label: 'Backblaze B2', value: 0.006 },
];

export default function PricingCalculator() {
  const [inputs, setInputs] = useState<CalculatorInputs>(DEFAULTS);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [showFormula, setShowFormula] = useState(false);
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');

  const result = useMemo(() => calculate(inputs), [inputs]);

  const update = (patch: Partial<CalculatorInputs>) => {
    setInputs((prev) => ({ ...prev, ...patch }));
  };

  const handleCopy = async () => {
    const md = buildMarkdown(inputs, result);
    try {
      await navigator.clipboard.writeText(md);
      setCopyState('copied');
      setTimeout(() => setCopyState('idle'), 2000);
    } catch {
      setCopyState('failed');
      setTimeout(() => setCopyState('idle'), 2000);
    }
  };

  return (
    <div className="calc">
      <div className="calc-grid">
        {/* === INPUTS === */}
        <section className="calc-inputs" aria-labelledby="calc-inputs-heading">
          <h3 id="calc-inputs-heading">Your numbers</h3>

          <div className="field">
            <label htmlFor="calc-source">
              Current artifact footprint
              <span className="field-value">{formatTb(inputs.sourceTb)}</span>
            </label>
            <input
              id="calc-source"
              type="range"
              min={0}
              max={3.5}
              step={0.05}
              // log scale: 10^x where x is the slider position
              value={Math.log10(Math.max(1, inputs.sourceTb))}
              onChange={(e) =>
                update({ sourceTb: Math.round(Math.pow(10, parseFloat(e.target.value)) * 10) / 10 })
              }
              aria-label="Source TB (logarithmic slider, 1 TB to 3000 TB)"
            />
            <p className="field-help">
              What <code>aws s3 ls --summarize</code> shows today across your bucket.
            </p>
          </div>

          <div className="field">
            <label>
              Regions
              <span className="field-value">{inputs.regions}</span>
            </label>
            <div className="region-buttons" role="group" aria-label="Number of regions">
              {[1, 2, 3, 4].map((n) => (
                <button
                  key={n}
                  type="button"
                  className={inputs.regions === n ? 'btn-active' : ''}
                  onClick={() => update({ regions: n })}
                  aria-pressed={inputs.regions === n}
                >
                  {n}
                  {n === 4 ? '+' : ''}
                </button>
              ))}
            </div>
            <p className="field-help">
              How many regions you replicate this data to. Each adds full
              storage cost.
            </p>
          </div>

          <div className="field">
            <label htmlFor="calc-cost">
              Storage cost
              <span className="field-value">${inputs.costPerGbMonthUsd}/GB-mo</span>
            </label>
            <input
              id="calc-cost"
              type="number"
              min={0.001}
              max={0.5}
              step={0.001}
              value={inputs.costPerGbMonthUsd}
              onChange={(e) =>
                update({ costPerGbMonthUsd: parseFloat(e.target.value) || 0 })
              }
              aria-label="Storage cost per GB per month, in USD"
            />
            <div className="cost-shortcuts">
              {COST_SHORTCUTS.map((s) => (
                <button
                  key={s.label}
                  type="button"
                  onClick={() => update({ costPerGbMonthUsd: s.value })}
                >
                  {s.label} ${s.value}
                </button>
              ))}
            </div>
          </div>

          <button
            type="button"
            className="advanced-toggle"
            onClick={() => setShowAdvanced((v) => !v)}
            aria-expanded={showAdvanced}
          >
            {showAdvanced ? '− Hide advanced' : '+ Show advanced'}
          </button>

          {showAdvanced && (
            <>
              <div className="field">
                <label htmlFor="calc-ratio">
                  Compression ratio
                  <span className="field-value">{inputs.compressionRatio.toFixed(1)}×</span>
                </label>
                <input
                  id="calc-ratio"
                  type="range"
                  min={0.3}
                  max={2}
                  step={0.05}
                  value={Math.log10(inputs.compressionRatio)}
                  onChange={(e) =>
                    update({
                      compressionRatio:
                        Math.round(Math.pow(10, parseFloat(e.target.value)) * 10) / 10,
                    })
                  }
                  aria-label="Compression ratio (logarithmic, 2× to 100×)"
                />
                <p className="field-help">
                  Conservative default. Verified ReadonlyREST migration ratios so far:
                  3.8×, 4.1×, 88.5×. Run the OSS build's Delta Efficiency Panel on
                  your bucket for a real number.
                </p>
              </div>

              <div className="field">
                <label htmlFor="calc-growth">
                  Annual data growth
                  <span className="field-value">{Math.round(inputs.annualGrowthRate * 100)}%</span>
                </label>
                <input
                  id="calc-growth"
                  type="range"
                  min={0}
                  max={2}
                  step={0.05}
                  value={inputs.annualGrowthRate}
                  onChange={(e) =>
                    update({ annualGrowthRate: parseFloat(e.target.value) })
                  }
                  aria-label="Annual data growth rate"
                />
              </div>
            </>
          )}
        </section>

        {/* === RESULT === */}
        <section className="calc-result" aria-labelledby="calc-result-heading" aria-live="polite">
          <ResultCard
            result={result}
            onCopy={handleCopy}
            copyState={copyState}
            showFormula={showFormula}
            onToggleFormula={() => setShowFormula((v) => !v)}
          />
        </section>
      </div>
    </div>
  );
}

// ===========================================================================

interface ResultCardProps {
  result: CalculatorResult;
  onCopy: () => void;
  copyState: 'idle' | 'copied' | 'failed';
  showFormula: boolean;
  onToggleFormula: () => void;
}

function ResultCard({ result, onCopy, copyState, showFormula, onToggleFormula }: ResultCardProps) {
  if (result.kind === 'belowThreshold') {
    return (
      <div className="card card-disqualify">
        <h3 id="calc-result-heading">DeltaGlider isn't worth it for you yet</h3>
        <p>
          At under 1 TB of source artifacts, the savings won't cover the
          support contract. Come back at 5 TB+, or use the OSS build for
          free.
        </p>
        <p>
          → <a href="https://github.com/beshu-tech/deltaglider_proxy">Try the OSS build</a>
        </p>
      </div>
    );
  }

  if (result.kind === 'negativeNet') {
    return (
      <div className="card card-disqualify">
        <h3 id="calc-result-heading">Savings would not cover the support contract</h3>
        <p>
          At this scale (savings ≈ <strong>{formatUsd(result.savings)}/yr</strong>),
          the Starter support contract (<strong>{formatUsd(result.supportCost)}/yr</strong>)
          would cost more than the storage you'd save. Use the OSS build (free),
          or talk to us about a different fit.
        </p>
        <div className="card-ctas">
          <a className="btn" href="https://github.com/beshu-tech/deltaglider_proxy">
            Try the OSS build
          </a>
          <a className="btn" href="mailto:contact@beshu.tech?subject=DeltaGlider%20-%20Different%20fit">
            Email us
          </a>
        </div>
      </div>
    );
  }

  if (result.kind === 'enterprise') {
    return (
      <div className="card card-enterprise">
        <h3 id="calc-result-heading">Enterprise — talk to sales</h3>
        <p>
          Approximate annual savings: <strong>{formatUsd(result.savings)}</strong>.
        </p>
        <p>
          At 250 TB+ stored footprint, you need a multi-region SLA, dedicated
          channel, and custom terms — not a one-size-fits-all bracket.
        </p>
        <div className="card-ctas">
          <a
            className="btn btn-primary"
            href="mailto:contact@beshu.tech?subject=DeltaGlider%20-%20Enterprise%20sales%20inquiry"
          >
            Schedule a sales call
          </a>
          <button type="button" className="btn" onClick={onCopy}>
            {copyState === 'copied' ? 'Copied!' : copyState === 'failed' ? 'Copy failed' : 'Copy this estimate'}
          </button>
        </div>
        <BreakdownTable lines={result.lines} />
      </div>
    );
  }

  // kind === 'ok'
  return (
    <div className="card card-ok">
      <h3 id="calc-result-heading">
        You'd save approximately <strong>{formatUsd(result.savings, { compact: true })}/year</strong>.
      </h3>
      <p>
        After subscribing to <strong>{result.bracket.name}</strong> at{' '}
        <strong>{result.bracket.priceLabel}</strong>,
        net annual savings: <strong>{formatUsd(result.netSavings)}</strong>.
      </p>

      {result.warnings.length > 0 && (
        <ul className="warnings">
          {result.warnings.map((w) => (
            <li key={w} className="warning-chip">
              {w === 'lowCompressionRatio' &&
                'Your data compresses below 3× — DeltaGlider may not be the right fit. Run the OSS build + Delta Efficiency Panel on your own data to verify.'}
              {w === 'cheapBackendAlready' &&
                "You're already on cheap object storage — savings will be smaller, but data sovereignty and lock-in benefits still apply."}
            </li>
          ))}
        </ul>
      )}

      <div className="card-ctas">
        <a className="btn btn-primary" href="/trial">
          Start a 30-day trial
        </a>
        <a className="btn" href="https://github.com/beshu-tech/deltaglider_proxy">
          Run the OSS build
        </a>
        <button type="button" className="btn btn-tertiary" onClick={onCopy}>
          {copyState === 'copied' ? 'Copied!' : copyState === 'failed' ? 'Copy failed' : 'Copy this estimate'}
        </button>
      </div>

      <BreakdownTable lines={result.lines} />

      <details className="formula" open={showFormula} onToggle={onToggleFormula}>
        <summary>Show your work</summary>
        <Formula />
      </details>
    </div>
  );
}

// ===========================================================================

function BreakdownTable({ lines }: { lines: BreakdownLine[] }) {
  return (
    <table className="breakdown" aria-label="Cost breakdown: today vs with DeltaGlider">
      <thead>
        <tr>
          <th></th>
          <th>Today</th>
          <th>With DeltaGlider</th>
        </tr>
      </thead>
      <tbody>
        {lines.map((line) => {
          const fmt = line.unit === 'tb' ? formatTb : (n: number) => formatUsd(n);
          const isSubtotal = line.label.startsWith('Subtotal') || line.label.startsWith('Total');
          return (
            <tr key={line.label} className={isSubtotal ? 'subtotal' : ''}>
              <th scope="row">{line.label}</th>
              <td>{fmt(line.today)}</td>
              <td>{fmt(line.dgp)}</td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

// ===========================================================================

function Formula() {
  return (
    <div className="formula-body">
      <p>
        All math in USD. AWS S3 inter-region replication egress is $0.02/GB.
        1 TB = 1024 GB.
      </p>
      <pre>
{`stored_footprint   = source_tb / compression_ratio
today_storage      = source_tb × 1024 × 12 × cost_per_gb_month × regions
dgp_storage        = stored_footprint × 1024 × 12 × cost_per_gb_month × regions
today_egress       = source_tb × growth × 1024 × (regions - 1) × $0.02
dgp_egress         = stored_footprint × growth × 1024 × (regions - 1) × $0.02
savings            = (today_storage + today_egress)
                   − (dgp_storage  + dgp_egress)
support_cost       = bracket_lookup(stored_footprint)
net_savings        = savings − support_cost`}
      </pre>
      <p>
        Bracket lookup: 0–10 TB → Starter $10k · 10–50 TB → Growth $30k ·
        50–250 TB → Scale $60k · 250 TB+ → Enterprise, talk to sales.
      </p>
    </div>
  );
}
