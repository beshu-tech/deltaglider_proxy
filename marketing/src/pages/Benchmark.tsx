import type { ReactNode } from 'react';
import { BenchmarkInteractiveCharts } from '../components/BenchmarkInteractiveCharts';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { BENCHMARK_NARRATIVE } from '../data/benchmarkSampleRun';
import { benchmarkMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

function CodeBlock({ children }: { children: ReactNode }): JSX.Element {
  return (
    <pre className="overflow-x-auto rounded-xl border border-ink-200 bg-ink-950 p-4 text-left text-[13px] leading-relaxed text-brand-100 shadow-inner dark:border-ink-700">
      <code className="font-mono">{children}</code>
    </pre>
  );
}

/** Product UI without analytics chart canvas (analytics.jpg crops often show a dark/broken chart band in marketing heroes). */
const HERO_IMG = `${import.meta.env.BASE_URL}screenshots/filebrowser.jpg`;

export function Benchmark(): JSX.Element {
  const putRatio = BENCHMARK_NARRATIVE.putPassthroughVsCompressionRatio.toFixed(1);
  const storagePct = Math.round(BENCHMARK_NARRATIVE.storageReductionVsLogicalPct);

  return (
    <>
      <SEO meta={benchmarkMeta} />
      <header className="relative w-full overflow-hidden border-b border-ink-200 bg-ink-950 dark:border-ink-800">
        <img
          src={HERO_IMG}
          alt=""
          role="presentation"
          className="block max-h-[min(28rem,52vh)] min-h-[min(18rem,42vh)] w-full object-cover object-[center_35%] sm:max-h-[min(34rem,58vh)] sm:min-h-[20rem]"
          width={1600}
          height={640}
          loading="eager"
          decoding="async"
          fetchPriority="high"
        />
        {/* Dark band on the left (and bottom on narrow screens) so white title stays readable */}
        <div
          className="pointer-events-none absolute inset-0 bg-gradient-to-r from-black/88 via-black/45 to-transparent sm:from-black/82 sm:via-black/28 sm:to-transparent"
          aria-hidden
        />
        <div
          className="pointer-events-none absolute inset-0 bg-gradient-to-t from-black/75 via-transparent to-transparent sm:hidden"
          aria-hidden
        />
        <div className="absolute inset-0 flex flex-col justify-end pb-10 pt-16 sm:justify-center sm:pb-12 sm:pt-12">
          <div className="mx-auto w-full max-w-3xl px-6 text-left">
            <div className="inline-block max-w-full rounded-2xl border border-white/20 bg-black/25 px-5 py-5 shadow-2xl shadow-black/40 backdrop-blur-xl backdrop-saturate-150 sm:px-7 sm:py-6 dark:bg-black/35">
              <p className="text-xs font-bold uppercase tracking-[0.22em] text-brand-300">
                Engineering notes
              </p>
              <h1 className="mt-3 text-balance text-3xl font-extrabold tracking-tight text-white sm:text-4xl lg:text-[2.65rem] lg:leading-[1.12]">
                What the compression tax costs you — and when it still pays
              </h1>
            </div>
          </div>
        </div>
      </header>

      <article className="mx-auto max-w-3xl px-6 pb-20 pt-10 sm:pt-12">
        <div className="border-b border-ink-200 pb-10 dark:border-ink-700">
          <p className="text-lg leading-relaxed text-ink-600 dark:text-ink-300">
            If you are sizing a DeltaGlider deployment, negotiating SLAs, or choosing between cheap plaintext storage and
            delta compression plus optional proxy encryption, you need numbers tied to <strong className="text-ink-800 dark:text-ink-100">your</strong>{' '}
            artifact shape and network — not a slogan. This is the published narrative around our{' '}
            <strong className="text-ink-800 dark:text-ink-100">compression-tax benchmark</strong> harness — reproducible,
            forkable, and meant to survive scrutiny from your own performance reviewers. This page explains what our published harness measures, what the
            pinned sample run implies for capacity planning, and how you can rerun the same workload on your hardware.
            The charts render <strong className="text-ink-800 dark:text-ink-100">in your browser</strong> (Chart.js);
            values come from one archived bundle so you can verify them against the repo.
          </p>
        </div>

        <Section
          id="reader-outcomes"
          eyebrow="Why read this"
          title="What you can take away."
          intro="You are not here for our implementation details — you want decisions: sizing, risk, and economics."
        >
          <ul className="list-disc space-y-4 pl-5 text-base leading-relaxed text-ink-700 dark:text-ink-200">
            <li>
              <strong className="text-ink-900 dark:text-white">Capacity &amp; latency budgets.</strong> You see wall-clock
              MB/s for PUT and GET per mode, so you can compare upload pipelines (CI bursts) vs download-heavy workloads
              (deploys, mirrors) against what your users tolerate.
            </li>
            <li>
              <strong className="text-ink-900 dark:text-white">Storage economics vs CPU.</strong> When deltas shrink
              stored bytes enough, you fund cheaper object storage or longer retention; the charts show whether that
              trade shows up on your profile or disappears under network noise.
            </li>
            <li>
              <strong className="text-ink-900 dark:text-white">Encryption without rewriting clients.</strong> Proxy-side
              AES-GCM keeps SigV4 and S3 semantics; the benchmark isolates encryption cost so you can judge whether your
              bottleneck is crypto, disk, or something else.
            </li>
            <li>
              <strong className="text-ink-900 dark:text-white">Reproducibility.</strong> Everything is scripted — if our
              headline claims do not match what you see on your VM, you still own the methodology and raw CSV/JSON to
              explain the gap (region, disk class, concurrency, artifact mix).
            </li>
          </ul>
        </Section>

        <Section
          id="questions"
          eyebrow="Premise"
          title="What the harness answers — in operator terms."
          intro="These are the questions we encode into CSV + summary.json + HTML so you are not guessing from a single headline metric."
        >
          <ul className="list-disc space-y-3 pl-5 text-base leading-relaxed text-ink-700 dark:text-ink-200">
            <li>
              <strong className="text-ink-900 dark:text-white">Will uploads keep up?</strong> Wall-clock PUT MB/s with
              compression on vs off — so you know whether ingest pipelines need wider concurrency or bigger proxy CPUs.
            </li>
            <li>
              <strong className="text-ink-900 dark:text-white">Will downloads feel OK?</strong> Cold vs warm GET MB/s when
              objects are stored as deltas or ciphertext — so you know whether edge caches or client parallelism matter
              more than proxy tuning.
            </li>
            <li>
              <strong className="text-ink-900 dark:text-white">Will the proxy fit?</strong> RSS, Docker CPU (mean/max over
              each mode window), and backend disk footprint — so you can map containers to instance sizes and spot noisy
              neighbours.
            </li>
            <li>
              <strong className="text-ink-900 dark:text-white">Will storage shrink enough?</strong> Logical bytes vs
              Prometheus Δ saved — so you can translate artifact churn into TB/month before you trust a cheaper tier.
            </li>
          </ul>
        </Section>

        <Section
          id="methodology"
          eyebrow="Methodology"
          title="Same artifacts, four buckets, one harness."
          intro={
            <>
              The runner ships under{' '}
              <code className="rounded bg-ink-100 px-1.5 py-0.5 font-mono text-sm dark:bg-ink-800">docs/benchmark/</code>.
              It downloads real artifacts (here: contiguous Alpine virt ISOs), runs PUT → cold GET → warm GET per mode,
              scrapes Prometheus and health, optionally captures host JSON (<code className="font-mono">docker stats</code>,{' '}
              <code className="font-mono">du</code>), and emits CSVs plus an HTML report — so{' '}
              <strong className="text-ink-900 dark:text-white">your</strong> operators get the same artefact trail they
              would keep for an internal performance review.
            </>
          }
        >
          <div className="space-y-6 text-base leading-relaxed text-ink-700 dark:text-ink-200">
            <p>
              <strong className="text-ink-900 dark:text-white">Four modes</strong> map to four buckets: passthrough,
              compression-only, encryption-only (proxy AES-GCM at rest), compression + encryption — so you never confuse
              codec effects with backend routing.
            </p>
            <p>
              <strong className="text-ink-900 dark:text-white">Isolation.</strong> Single-VM smoke can restart the proxy
              between modes so per-mode RSS reflects a fresh process; split client/proxy VMs are what you use when you
              want publication-grade separation (see README).
            </p>
            <p>
              <strong className="text-ink-900 dark:text-white">Honest metrics.</strong> An inner{' '}
              <code className="font-mono">docker restart</code> between PUT and cold GET resets Prometheus counters —
              verification fails unless you choose <code className="font-mono">--no-proxy-restart</code> or{' '}
              <code className="font-mono">--skip-compression-verify</code>. You decide which trade-off matches how you
              test cold reads.
            </p>
          </div>
        </Section>

        <Section
          id="interactive-charts"
          eyebrow="Results"
          title="Interactive charts (pinned sample run)."
          intro="Charts render client-side so they stay faithful to Chart.js. Numbers match `benchmarkSampleRun.ts`; refresh that file when you adopt a new canonical run for marketing."
        >
          <BenchmarkInteractiveCharts />
        </Section>

        <Section
          id="reading-cpu"
          eyebrow="Interpretation"
          title="How to read Docker CPU without fooling yourself."
          intro="CPU% from a single idle snapshot lied; the report aggregates mean/max inside each mode window when timeseries exists."
        >
          <div className="space-y-4 text-base leading-relaxed text-ink-700 dark:text-ink-200">
            <p>
              For <strong className="text-ink-900 dark:text-white">your</strong> planning, treat{' '}
              <strong className="text-ink-900 dark:text-white">throughput MB/s</strong> as the primary speed signal for
              user-visible work. CPU charts complement that: they show whether you are thermally or scheduler-bound during
              mixed phases, not a perfect map of “xdelta cost” vs “memcpy cost.”
            </p>
            <p>
              Whole-window averages blend PUT spikes with quieter GET phases — ordering effects happen. If your SLA is
              upload-bound, weight PUT; if mirror/download dominates, weight GET rows from the summary tables under the
              charts.
            </p>
          </div>
        </Section>

        <Section
          id="conclusions"
          eyebrow="Conclusions"
          title="What this sample run implies — and what it does not."
          intro="Grounded in the pinned Hetzner single-VM Alpine ISO bundle referenced on this page. Your mileage will differ; these are structured takeaways, not guarantees."
        >
          <div className="space-y-8 text-base leading-relaxed text-ink-700 dark:text-ink-200">
            <div>
              <h3 className="text-lg font-bold text-ink-900 dark:text-white">Throughput</h3>
              <ul className="mt-3 list-disc space-y-2 pl-5">
                <li>
                  On this hardware and artifact mix, passthrough PUT landed near{' '}
                  <strong className="text-ink-900 dark:text-white">{BENCHMARK_NARRATIVE.putPassthroughMbS} MB/s</strong>{' '}
                  wall-clock vs compression near{' '}
                  <strong className="text-ink-900 dark:text-white">{BENCHMARK_NARRATIVE.putCompressionMbS} MB/s</strong> —
                  roughly <strong className="text-ink-900 dark:text-white">{putRatio}×</strong> faster ingest without
                  delta encoding. For you: if CI spends most of its wall time in uploads and CPU headroom is scarce,
                  budget more cores or fewer concurrent writers before blaming the network.
                </li>
                <li>
                  Encryption-mode cold GET showed very high reported MB/s (
                  <strong className="text-ink-900 dark:text-white">{BENCHMARK_NARRATIVE.coldGetEncryptionMbS} MB/s</strong>{' '}
                  vs passthrough ~{BENCHMARK_NARRATIVE.coldGetPassthroughMbS} MB/s on this run). The client still receives
                  plaintext; the gap reflects workload timing and cache state — not a promise every workload decrypts
                  “faster than plaintext.” Use it as a sign your bottleneck may not be AES here.
                </li>
                <li>
                  Compression GET MB/s sat in the same ballpark as passthrough cold/warm on this profile — for you: once
                  objects are written, read paths may be acceptable even when PUT was expensive; validate with{' '}
                  <strong className="text-ink-900 dark:text-white">your</strong> object sizes and cache behaviour.
                </li>
              </ul>
            </div>

            <div>
              <h3 className="text-lg font-bold text-ink-900 dark:text-white">Storage</h3>
              <p>
                Compression modes pushed implied stored size to about{' '}
                <strong className="text-ink-900 dark:text-white">{storagePct}%</strong> below logical uploads (
                ~{BENCHMARK_NARRATIVE.impliedStoredCompressionGb.toFixed(3)} GB implied vs{' '}
                ~{BENCHMARK_NARRATIVE.logicalGb.toFixed(3)} GB logical for the same five ISOs). For you: when object-store
                bills dominate opex, that delta can fund cheaper tiers or longer retention — rerun with{' '}
                <strong className="text-ink-900 dark:text-white">your</strong> binaries (kernels, models, DB dumps) because
                similarity drives savings.
              </p>
            </div>

            <div>
              <h3 className="text-lg font-bold text-ink-900 dark:text-white">Footprint &amp; ops</h3>
              <ul className="mt-3 list-disc space-y-2 pl-5">
                <li>
                  Docker CPU mean/max varied by mode window — use it alongside RSS and disk charts when rightsizing the
                  proxy container versus colocated workloads.
                </li>
                <li>
                  Single-VM smoke intentionally stresses everything on one box — fine for regression detection and quick
                  parity checks; it is not a substitute for isolating client network RTT or backend latency in production.
                  When you publish externally, prefer the two-VM topology from the benchmark README.
                </li>
              </ul>
            </div>

            <div className="rounded-2xl border border-white/10 bg-ink-950 p-6 shadow-xl ring-1 ring-black/20 dark:border-white/10 dark:bg-black/50 dark:ring-white/10">
              <h3 className="text-xs font-extrabold uppercase tracking-[0.2em] text-brand-300">
                Bottom line for buyers &amp; builders
              </h3>
              <ul className="mt-4 list-disc space-y-3 pl-5 text-[15px] leading-relaxed text-slate-100 marker:text-brand-400">
                <li>
                  Need <strong className="font-semibold text-white">minimum ingest latency</strong> and do not pay per GB?
                  Passthrough or tuned concurrency may beat compression for that pipeline — confirm with your files.
                </li>
                <li>
                  Need <strong className="font-semibold text-white">minimum stored bytes</strong> on similar sequential
                  artifacts? Compression modes here show the order-of-magnitude storage win you should model before picking
                  cold storage.
                </li>
                <li>
                  Need <strong className="font-semibold text-white">ciphertext at the provider</strong> without client
                  changes? Encryption modes isolate that tax so you can decide if proxy AES fits your CPU envelope.
                </li>
                <li>
                  None of this replaces <strong className="font-semibold text-white">your</strong> proof: rerun the harness,
                  compare charts to this page, and attach the tarball when you internal sign-off.
                </li>
              </ul>
            </div>
          </div>
        </Section>

        <Section
          id="reproduce"
          eyebrow="Reproduce"
          title="Run the same benchmark yourself."
          intro={
            <>
              Clone <a href={REPO_URL}>{REPO_URL}</a>, install Python deps, export{' '}
              <code className="font-mono">HCLOUD_TOKEN</code>, then drive the lifecycle (single-VM smoke below; swap for a
              split client/proxy run when you want cleaner networking).
            </>
          }
        >
          <div className="space-y-6">
            <div>
              <h3 className="text-lg font-bold text-ink-900 dark:text-white">1 · Toolchain</h3>
              <CodeBlock>
                {`python3 -m venv .venv-dgp-bench
source .venv-dgp-bench/bin/activate
pip install -r docs/benchmark/requirements.txt`}
              </CodeBlock>
            </div>
            <div>
              <h3 className="text-lg font-bold text-ink-900 dark:text-white">2 · Provision Hetzner single VM</h3>
              <CodeBlock>
                {`export HCLOUD_TOKEN=…
python docs/benchmark/bench_production_tax.py up \\
  --run-id dgp-bench-$(date -u +%Y%m%d-%H%M%SZ) \\
  --single-vm --location hel1 --client-type ccx33 \\
  --ssh-key-name YOUR_HCLOUD_SSH_KEY_NAME`}
              </CodeBlock>
            </div>
            <div>
              <h3 className="text-lg font-bold text-ink-900 dark:text-white">3 · Execute smoke + download bundle</h3>
              <CodeBlock>
                {`python docs/benchmark/bench_production_tax.py single-vm-smoke \\
  --run-id YOUR_RUN_ID \\
  --artifact-count 5 \\
  --artifact-source alpine-iso \\
  --artifact-extension .iso \\
  --alpine-branch v3.19 \\
  --alpine-flavor virt \\
  --alpine-arch x86_64 \\
  --concurrency 1 \\
  --no-proxy-restart`}
              </CodeBlock>
              <p className="mt-3 text-sm leading-relaxed text-ink-600 dark:text-ink-400">
                Use <code className="font-mono">--no-proxy-restart</code> so Prometheus verification stays aligned;
                otherwise add <code className="font-mono">--skip-compression-verify</code>. Bundles land in{' '}
                <code className="font-mono">docs/benchmark/results/&lt;run-id&gt;.tgz</code>.
              </p>
            </div>
            <div>
              <h3 className="text-lg font-bold text-ink-900 dark:text-white">4 · Render the HTML report</h3>
              <CodeBlock>
                {`python docs/benchmark/bench_production_tax.py html-report \\
  --bundle docs/benchmark/results/YOUR_RUN_ID.tgz \\
  --out docs/benchmark/results/YOUR_RUN_ID-report.html`}
              </CodeBlock>
            </div>
          </div>
          <p className="mt-10 text-sm leading-relaxed text-ink-600 dark:text-ink-400">
            Deeper methodology and Grafana mapping:{' '}
            <code className="rounded bg-ink-100 px-1 font-mono dark:bg-ink-800">docs/benchmark/README.md</code>,{' '}
            <code className="rounded bg-ink-100 px-1 font-mono dark:bg-ink-800">docs/benchmark/grafana-parity.md</code>.
          </p>
        </Section>

        <footer className="mt-16 border-t border-ink-200 pt-10 dark:border-ink-700">
          <p className="text-sm leading-relaxed text-ink-600 dark:text-ink-400">
            Harness and HTML report evolve with the proxy (CPU rollup semantics, restart flags). Pin{' '}
            <code className="font-mono">benchmarkSampleRun.ts</code> when you refresh marketing numbers — your stakeholders
            should always be able to diff narrative claims against an archived tarball.
          </p>
        </footer>
      </article>
    </>
  );
}
