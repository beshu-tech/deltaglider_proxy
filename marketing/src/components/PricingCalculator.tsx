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
import {
  PROVIDERS,
  getProvider,
  monthlyCostUsd,
  effectiveUsdPerGbMonth,
} from '../lib/providers';

// Default slider positions per v5 plan §5.3.
// compressionRatio stays a multiplier internally so the math module
// (pricing.ts) and its 22 tests don't change. The UI converts to/from
// "% bytes saved" at the input/display boundary only.
const DEFAULTS: CalculatorInputs = {
  sourceTb: 30,
  regions: 2,
  costPerGbMonthUsd: 0.023,
  compressionRatio: 10, // = 90% bytes saved
  annualGrowthRate: 0.30,
};

// % bytes saved ↔ compression ratio. Math is unchanged underneath; the
// UI just speaks the % language because it's easier for non-engineers.
//   50% saved → 2× ratio
//   90% saved → 10× ratio  (default)
//   99% saved → 100× ratio
const PCT_SAVED_MIN = 50;
const PCT_SAVED_MAX = 99;
const pctSavedToRatio = (pct: number): number => 1 / (1 - pct / 100);
const ratioToPctSaved = (ratio: number): number => (1 - 1 / ratio) * 100;

export default function PricingCalculator() {
  const [inputs, setInputs] = useState<CalculatorInputs>(DEFAULTS);
  const [providerId, setProviderId] = useState<string>('s3');
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [showFormula, setShowFormula] = useState(false);
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');

  // The picked provider is the source of truth for cost. Its EFFECTIVE $/GB at
  // the visitor's current footprint (which captures min-billing floors + free
  // tiers) feeds the existing, fully-tested `calculate()` unchanged.
  const sourceGb = inputs.sourceTb * 1024;
  const effectiveInputs: CalculatorInputs = {
    ...inputs,
    costPerGbMonthUsd: effectiveUsdPerGbMonth(providerId, sourceGb),
  };

  const result = useMemo(() => calculate(effectiveInputs), [effectiveInputs]);

  const update = (patch: Partial<CalculatorInputs>) => {
    setInputs((prev) => ({ ...prev, ...patch }));
  };

  const handleCopy = async () => {
    const md = buildMarkdown(effectiveInputs, result);
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
            <label htmlFor="calc-provider">
              Your storage provider
              <span className="field-value">
                ≈ ${effectiveInputs.costPerGbMonthUsd.toFixed(4)}/GB-mo
              </span>
            </label>
            <div className="provider-chips" role="group" aria-label="Storage provider">
              {PROVIDERS.filter((p) => !p.archive).map((p) => (
                <button
                  key={p.id}
                  type="button"
                  className={providerId === p.id ? 'btn-active' : ''}
                  onClick={() => setProviderId(p.id)}
                  aria-pressed={providerId === p.id}
                >
                  {p.name}
                </button>
              ))}
            </div>
            <p className="field-help">{getProvider(providerId).notes}</p>
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
                  Bytes saved
                  <span className="field-value">
                    {Math.round(ratioToPctSaved(inputs.compressionRatio))}%
                  </span>
                </label>
                <input
                  id="calc-ratio"
                  type="range"
                  min={PCT_SAVED_MIN}
                  max={PCT_SAVED_MAX}
                  step={1}
                  value={Math.round(ratioToPctSaved(inputs.compressionRatio))}
                  onChange={(e) =>
                    update({
                      compressionRatio: pctSavedToRatio(parseFloat(e.target.value)),
                    })
                  }
                  aria-label="Bytes saved by compression (50% to 99%)"
                />
                <p className="field-help">
                  Conservative default. Verified ReadonlyREST migration ratios so far:
                  74%, 76%, 99%. Run the OSS build's Delta Efficiency Panel on
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
          <ProviderComparison
            sourceGb={sourceGb}
            storedGb={sourceGb / inputs.compressionRatio}
            selectedId={providerId}
            onSelect={setProviderId}
          />
        </section>
      </div>
    </div>
  );
}

// ===========================================================================

interface ProviderComparisonProps {
  sourceGb: number;
  storedGb: number;
  selectedId: string;
  onSelect: (id: string) => void;
}

/** What this footprint costs per month on each provider — today vs after
 *  DeltaGlider. Sorted cheapest-today first; the min-billing floor shows up as
 *  a flat post-DGP bar that can't shrink further. */
function ProviderComparison({ sourceGb, storedGb, selectedId, onSelect }: ProviderComparisonProps) {
  const rows = PROVIDERS.map((p) => ({
    p,
    today: monthlyCostUsd(p.id, sourceGb),
    dgp: monthlyCostUsd(p.id, storedGb),
  })).sort((a, b) => a.today - b.today);

  const max = Math.max(...rows.map((r) => r.today), 1);
  const fmt = (n: number) =>
    n >= 1000 ? `$${(n / 1000).toFixed(1)}k` : `$${n.toFixed(n < 10 ? 2 : 0)}`;

  return (
    <div className="provider-compare">
      <div className="provider-compare-head">
        <span>Monthly storage cost · {formatTb(sourceGb / 1024)} of artifacts</span>
        <span className="provider-compare-legend">
          <i className="dot dot-today" /> today
          <i className="dot dot-dgp" /> with DeltaGlider
        </span>
      </div>
      <ul className="provider-rows">
        {rows.map(({ p, today, dgp }) => (
          <li
            key={p.id}
            className={`provider-row${p.id === selectedId ? ' is-selected' : ''}`}
          >
            <button
              type="button"
              className="provider-row-name"
              onClick={() => onSelect(p.id)}
              aria-pressed={p.id === selectedId}
              title={p.notes}
            >
              {p.name}
              {p.currency === 'EUR' && <span className="provider-eur">€</span>}
              {p.archive && <span className="provider-archive">archive</span>}
            </button>
            <div className="provider-bars">
              <div className="provider-bar provider-bar-today" style={{ width: `${(today / max) * 100}%` }}>
                <span>{fmt(today)}</span>
              </div>
              <div
                className="provider-bar provider-bar-dgp"
                style={{ width: `${Math.max((dgp / max) * 100, 1.5)}%` }}
              >
                <span>{fmt(dgp)}</span>
              </div>
            </div>
          </li>
        ))}
      </ul>
      <p className="provider-compare-note">
        Costs normalised to USD/month for comparison (€-priced providers tagged).
        Min-billing floors mean some bars can't shrink below ~$6–8/mo — DeltaGlider
        helps most where you store far above the floor. Egress & API fees excluded.
      </p>
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
          <a className="btn btn-primary" href="https://github.com/beshu-tech/deltaglider_proxy">
            Try the OSS build
          </a>
          <a className="link-action" href="mailto:sales@beshu.tech?subject=DeltaGlider%20-%20Different%20fit">
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
          At 250 TB+ stored footprint, you need a multi-region SLA, named
          engineering contact, and custom terms — not a one-size-fits-all bracket.
        </p>
        <div className="card-ctas">
          <a
            className="btn btn-primary"
            href="mailto:sales@beshu.tech?subject=DeltaGlider%20-%20Enterprise%20sales%20inquiry"
          >
            Schedule a sales call
          </a>
          <button type="button" className="link-action" onClick={onCopy}>
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
      <h3 id="calc-result-heading" className="calc-hero-heading">
        You'd save approximately <strong className="calc-hero-number">{formatUsd(result.savings, { compact: true })}/year</strong>
      </h3>
      <p className="calc-hero-subtext">
        After subscribing to <strong>{result.bracket.name}</strong> at{' '}
        <strong>{result.bracket.priceLabel}</strong>,
        net annual savings: <strong className="calc-hero-net">{formatUsd(result.netSavings)}</strong>.
      </p>

      {result.warnings.length > 0 && (
        <ul className="warnings">
          {result.warnings.map((w) => (
            <li key={w} className="warning-chip">
              {w === 'lowCompressionRatio' &&
                'Your data compresses below 67% bytes saved — DeltaGlider may not be the right fit. Run the OSS build + Delta Efficiency Panel on your own data to verify.'}
              {w === 'cheapBackendAlready' &&
                "You're already on cheap object storage — savings will be smaller, but data sovereignty and lock-in benefits still apply."}
            </li>
          ))}
        </ul>
      )}

      <div className="card-ctas">
        <a className="btn btn-brand" href="/trial">
          Start a 30-day trial
        </a>
        <a className="link-action" href="https://github.com/beshu-tech/deltaglider_proxy">
          Run the OSS build
        </a>
        <button type="button" className="link-action" onClick={onCopy}>
          {copyState === 'copied' ? 'Copied!' : copyState === 'failed' ? 'Copy failed' : 'Copy this estimate'}
        </button>
      </div>

      <BreakdownTable lines={result.lines} />

      <details className="formula" open={showFormula} onToggle={onToggleFormula}>
        <summary>
          <span>Show your work</span>
        </summary>
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
          <th className="col-today">Today</th>
          <th className="col-dgp">With DeltaGlider</th>
        </tr>
      </thead>
      <tbody>
        {lines.map((line) => {
          const fmt = line.unit === 'tb' ? formatTb : (n: number) => formatUsd(n);
          const isSubtotal = line.label.startsWith('Subtotal') || line.label.startsWith('Total');
          return (
            <tr key={line.label} className={isSubtotal ? 'subtotal' : ''}>
              <th scope="row">{line.label}</th>
              <td className="col-today">{fmt(line.today)}</td>
              <td className="col-dgp">{fmt(line.dgp)}</td>
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
        Bracket lookup: 0–10 TB → Starter $2.5k · 10–50 TB → Growth $7.5k ·
        50–250 TB → Scale $15k · 250 TB+ → Enterprise, talk to sales.
      </p>
    </div>
  );
}
