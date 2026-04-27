import { useMemo, useState } from 'react';
import { FeatureCard } from '../components/FeatureCard';
import { MailtoCTA } from '../components/MailtoCTA';
import { S3MigrationHeroSubhead } from '../components/S3MigrationHeroSubhead';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { s3MigrationMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const SUBJECT = 'Amazon S3 to Hetzner / Wasabi migration';

const AWS_S3_STANDARD_PER_TB = 23;

const PROVIDERS = [
  {
    id: 'hetzner',
    name: 'Hetzner Object Storage',
    shortName: 'Hetzner',
    pricePerTb: 7.99,
    color: 'from-cyan-300 to-brand-500',
    notes: 'Uses new USD object-storage base pricing as a simple per-TB proxy. Additional storage and exact month length can vary.',
    sourceLabel: 'Hetzner docs',
    sourceHref:
      'https://docs.hetzner.com/general/infrastructure-and-availability/price-adjustment/',
  },
  {
    id: 'wasabi',
    name: 'Wasabi Hot Cloud Storage',
    shortName: 'Wasabi',
    pricePerTb: 6.99,
    color: 'from-amber-300 to-orange-500',
    notes: 'Wasabi pricing has a 1 TB minimum and policy limits around egress ratio / minimum storage duration.',
    sourceLabel: 'Wasabi pricing',
    sourceHref: 'https://wasabi.com/cloud-storage-pricing',
  },
] as const;

const FEATURE_ROWS = [
  {
    category: 'Identity',
    s3: 'IAM users, access keys, groups/roles, policy documents',
    dgp: 'IAM users, groups, S3 access keys, ABAC resources, prefix/IP conditions',
    fit: 'Excellent',
    note: 'App-facing access',
  },
  {
    category: 'Federation',
    s3: 'IAM Identity Center / external federation patterns',
    dgp: 'OAuth/OIDC providers plus group-mapping rules in the admin UI',
    fit: 'Good',
    note: 'Control plane',
  },
  {
    category: 'Bucket policy',
    s3: 'Bucket policies, public access controls, prefix scoping',
    dgp: 'Per-bucket policy, public read-only prefixes, aliases, compression policy',
    fit: 'Excellent',
    note: 'Migration-ready',
  },
  {
    category: 'Quotas / guardrails',
    s3: 'Service quotas and account-level governance',
    dgp: 'Soft per-bucket quota, write rejection, quota=0 bucket freeze',
    fit: 'Good',
    note: 'Ops guardrail',
  },
  {
    category: 'Replication',
    s3: 'Amazon S3 replication rules between buckets/regions/accounts',
    dgp: 'Object replication rules with run-now, pause/resume, history, failures, delete replication',
    fit: 'Good',
    note: 'Proxy-managed',
  },
  {
    category: 'Audit',
    s3: 'CloudTrail / access logs / storage lens ecosystem',
    dgp: 'Structured audit entries, in-memory audit viewer, stdout/log-pipeline friendly events',
    fit: 'Partial',
    note: 'Local audit',
  },
  {
    category: 'Metrics',
    s3: 'CloudWatch, Storage Lens, inventory jobs',
    dgp: 'Prometheus metrics, embedded dashboard, savings analytics, cache/runtime health',
    fit: 'Good',
    note: 'Operator view',
  },
  {
    category: 'Encryption',
    s3: 'AWS SSE-S3, SSE-KMS, bucket-level encryption policy',
    dgp: 'Proxy-side AES-256-GCM, SSE-S3/SSE-KMS backend modes, key custody options',
    fit: 'Excellent',
    note: 'Key custody',
  },
  {
    category: 'Lifecycle / Object Lock',
    s3: 'Lifecycle transitions, legal hold, Object Lock, deep archive classes',
    dgp: 'Not a full lifecycle or Object Lock replacement today',
    fit: 'Keep native',
    note: 'Use backend',
  },
] as const;

const MIGRATION_STEPS = [
  {
    n: '01',
    title: 'Keep S3 clients',
    body: 'Apps, CI, and SDKs still use S3. Point the endpoint to DeltaGlider instead of Amazon S3.',
  },
  {
    n: '02',
    title: 'Move bytes cheaper',
    body: 'DeltaGlider writes to Hetzner or Wasabi behind the scenes, using the backend economics you choose.',
  },
  {
    n: '03',
    title: 'Replace the control plane',
    body: 'IAM, OAuth, policy, quotas, replication, metrics, audit, encryption at rest, and day-2 ops live in DeltaGlider.',
  },
  {
    n: '04',
    title: 'Compress repeats',
    body: 'Repeated artifacts, archives, builds, dumps, and model variants can shrink further through delta storage.',
  },
] as const;

function usd(value: number): string {
  return new Intl.NumberFormat('en-US', {
    style: 'currency',
    currency: 'USD',
    maximumFractionDigits: value >= 100 ? 0 : 2,
  }).format(value);
}

function percent(value: number): string {
  return `${Math.round(value)}%`;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function fitClass(fit: string): string {
  if (fit === 'Excellent') {
    return 'border-brand-300 bg-brand-100 text-brand-900 dark:border-brand-500/40 dark:bg-brand-900/60 dark:text-brand-100';
  }
  if (fit === 'Good') {
    return 'border-cyan-300 bg-cyan-100 text-cyan-950 dark:border-cyan-500/40 dark:bg-cyan-950/60 dark:text-cyan-100';
  }
  if (fit === 'Partial') {
    return 'border-amber-300 bg-amber-100 text-amber-950 dark:border-amber-500/40 dark:bg-amber-950/60 dark:text-amber-100';
  }
  return 'border-ink-300 bg-ink-100 text-ink-800 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100';
}

export function S3Migration(): JSX.Element {
  const [tb, setTb] = useState(100);
  const [compression, setCompression] = useState(80);
  const compressionRatio = (100 - compression) / 100;

  const model = useMemo(() => {
    const awsMonthly = tb * AWS_S3_STANDARD_PER_TB;
    return PROVIDERS.map((provider) => {
      const backendOnly = tb * provider.pricePerTb;
      const dgpMonthly = tb * compressionRatio * provider.pricePerTb;
      const backendSavings = 1 - backendOnly / awsMonthly;
      const totalSavings = 1 - dgpMonthly / awsMonthly;
      return {
        ...provider,
        backendOnly,
        dgpMonthly,
        backendSavings,
        totalSavings,
        ratioToAws: dgpMonthly / awsMonthly,
        annualSavings: (awsMonthly - dgpMonthly) * 12,
      };
    });
  }, [compressionRatio, tb]);

  const best = model.reduce((winner, candidate) =>
    candidate.dgpMonthly < winner.dgpMonthly ? candidate : winner,
  );
  const compressedTb = tb * compressionRatio;
  const awsMonthly = tb * AWS_S3_STANDARD_PER_TB;

  return (
    <>
      <SEO meta={s3MigrationMeta} />

      <section className="relative isolate overflow-hidden bg-ink-950 text-white">
        <div className="absolute inset-0 -z-10 bg-[radial-gradient(circle_at_12%_20%,rgba(45,212,191,0.28),transparent_28rem),radial-gradient(circle_at_82%_12%,rgba(251,191,36,0.22),transparent_24rem),linear-gradient(135deg,#020617,#0f172a_45%,#042f2e)]" />
        <div className="absolute inset-0 -z-10 opacity-30 [background-image:linear-gradient(90deg,rgba(255,255,255,.08)_1px,transparent_1px),linear-gradient(rgba(255,255,255,.08)_1px,transparent_1px)] [background-size:42px_42px]" />

        <div className="mx-auto grid max-w-6xl gap-10 px-6 py-16 lg:grid-cols-[1fr_0.9fr] lg:items-center lg:py-24">
          <div>
            <div className="inline-flex rounded-full border border-brand-300/40 bg-brand-300/10 px-4 py-2 text-xs font-extrabold uppercase tracking-[0.22em] text-brand-200">
              Amazon S3 migration economics
            </div>
            <h1 className="mt-6 max-w-4xl text-5xl font-black tracking-tight sm:text-6xl lg:text-7xl">
              Move AWS S3 data to Hetzner.
              <span className="block bg-gradient-to-r from-brand-200 via-cyan-200 to-amber-200 bg-clip-text text-transparent">
                Keep the enterprise control plane.
              </span>
            </h1>
            <div className="mt-6 max-w-2xl text-lg sm:text-xl">
              <S3MigrationHeroSubhead />
            </div>
            <div className="mt-8 flex flex-wrap gap-3">
              <MailtoCTA subject={SUBJECT} label="Model our AWS bill" />
              <a
                href="#calculator"
                className="inline-flex items-center gap-2 rounded-lg border border-white/20 bg-white/10 px-5 py-3 font-semibold text-white backdrop-blur transition hover:border-brand-200 hover:text-brand-100"
              >
                Open calculator <span aria-hidden>↓</span>
              </a>
            </div>
          </div>

          <div className="rounded-[2rem] border border-white/15 bg-white/10 p-4 shadow-2xl shadow-black/30 backdrop-blur-xl">
            <div className="rounded-[1.5rem] bg-ink-950/80 p-5 ring-1 ring-white/10">
              <div className="text-xs font-extrabold uppercase tracking-[0.22em] text-ink-400">
                Example at {tb} TB / {compression}% compression
              </div>
              <div className="mt-4 grid gap-3">
                <div className="rounded-2xl border border-rose-300/30 bg-rose-400/10 p-4">
                  <div className="text-sm font-bold text-rose-100">Amazon S3 Standard</div>
                  <div className="mt-1 text-4xl font-black">{usd(awsMonthly)}</div>
                  <div className="text-sm text-ink-300">per month before requests / egress</div>
                </div>
                <div className="rounded-2xl border border-brand-300/40 bg-brand-300/10 p-4">
                  <div className="text-sm font-bold text-brand-100">
                    {best.shortName} + DeltaGlider
                  </div>
                  <div className="mt-1 text-5xl font-black text-brand-200">
                    {usd(best.dgpMonthly)}
                  </div>
                  <div className="text-sm text-ink-300">
                    {percent(best.totalSavings * 100)} cheaper than AWS, about{' '}
                    {usd(best.annualSavings)} saved yearly
                  </div>
                </div>
              </div>
              <div className="mt-5 rounded-2xl border border-white/10 bg-white/5 p-4">
                <div className="flex items-end justify-between gap-4">
                  <div>
                    <div className="text-xs font-bold uppercase tracking-widest text-ink-400">
                      Effective stored bytes
                    </div>
                    <div className="mt-1 text-3xl font-black">{compressedTb.toFixed(1)} TB</div>
                  </div>
                  <div className="text-right text-sm text-ink-300">
                    {tb} TB source
                    <br />
                    {compression}% smaller
                  </div>
                </div>
                <div className="mt-4 h-3 overflow-hidden rounded-full bg-white/10">
                  <div
                    className="h-full rounded-full bg-gradient-to-r from-brand-300 to-cyan-200"
                    style={{ width: `${Math.max(2, compressionRatio * 100)}%` }}
                  />
                </div>
              </div>
            </div>
          </div>
        </div>
      </section>

      <Section
        eyebrow="Migration shape"
        title="Do not replace Amazon S3 with a cheaper bucket. Replace the missing control plane."
        intro="Hetzner and Wasabi can make object storage cheaper. DeltaGlider is the policy and operations layer that makes the migration usable for production teams."
      >
        <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
          {MIGRATION_STEPS.map((step) => (
            <div
              key={step.n}
              className="relative overflow-hidden rounded-3xl border border-ink-200 bg-white p-6 shadow-lg shadow-brand-950/5 dark:border-ink-700 dark:bg-ink-900/70"
            >
              <div className="absolute -right-5 -top-6 text-8xl font-black text-brand-100 dark:text-brand-900/50">
                {step.n}
              </div>
              <div className="relative">
                <div className="text-xs font-extrabold uppercase tracking-[0.2em] text-brand-600 dark:text-brand-300">
                  Step {step.n}
                </div>
                <h2 className="mt-4 text-2xl font-black text-ink-900 dark:text-ink-50">
                  {step.title}
                </h2>
                <p className="mt-3 text-sm leading-6 text-ink-600 dark:text-ink-300">
                  {step.body}
                </p>
              </div>
            </div>
          ))}
        </div>
      </Section>

      <Section
        eyebrow="Cost calculator"
        title="Two levers: cheaper storage and compression."
        intro="Move the sliders. The page models storage capacity only, because request, retrieval, minimum duration, support, egress, and region rules differ by provider."
      >
        <div
          id="calculator"
          className="grid gap-6 rounded-[2rem] border border-ink-200 bg-white p-5 shadow-2xl shadow-brand-950/10 dark:border-ink-700 dark:bg-ink-950 lg:grid-cols-[0.72fr_1.28fr]"
        >
          <div className="rounded-[1.5rem] bg-ink-950 p-6 text-white">
            <div className="text-xs font-extrabold uppercase tracking-[0.22em] text-brand-200">
              Inputs
            </div>
            <label className="mt-6 block">
              <div className="flex items-baseline justify-between gap-4">
                <span className="font-bold">Source data in Amazon S3</span>
                <span className="text-2xl font-black text-brand-200">{tb} TB</span>
              </div>
              <input
                type="range"
                min="10"
                max="1000"
                step="10"
                value={tb}
                onChange={(event) => setTb(Number(event.target.value))}
                className="mt-4 w-full accent-teal-300"
              />
            </label>
            <label className="mt-8 block">
              <div className="flex items-baseline justify-between gap-4">
                <span className="font-bold">Delta compression savings</span>
                <span className="text-2xl font-black text-amber-200">{compression}%</span>
              </div>
              <input
                type="range"
                min="50"
                max="99"
                value={compression}
                onChange={(event) =>
                  setCompression(clamp(Number(event.target.value), 50, 99))
                }
                className="mt-4 w-full accent-amber-300"
              />
              <p className="mt-3 text-sm leading-6 text-ink-300">
                50% means stored bytes are half of source. 99% means the
                backend stores 1% of the repeated artifact stream.
              </p>
            </label>
            <div className="mt-8 rounded-2xl border border-white/10 bg-white/5 p-4">
              <div className="text-xs font-bold uppercase tracking-widest text-ink-400">
                Assumed AWS baseline
              </div>
              <div className="mt-1 text-3xl font-black">{usd(AWS_S3_STANDARD_PER_TB)}/TB-mo</div>
              <p className="mt-2 text-sm leading-6 text-ink-300">
                Amazon S3 Standard, first 50 TB in us-east-1 style pricing. This
                keeps the model legible and conservative for hot storage.
              </p>
            </div>
          </div>

          <div className="grid gap-4">
            {model.map((provider) => (
              <div
                key={provider.id}
                className="rounded-[1.5rem] border border-ink-200 bg-ink-50 p-5 dark:border-ink-700 dark:bg-ink-900"
              >
                <div className="grid gap-5 lg:grid-cols-[0.9fr_1.1fr] lg:items-center">
                  <div>
                    <div className="text-xs font-extrabold uppercase tracking-[0.2em] text-brand-600 dark:text-brand-300">
                      {provider.name}
                    </div>
                    <div className="mt-2 text-4xl font-black text-ink-900 dark:text-white">
                      {usd(provider.dgpMonthly)}
                    </div>
                    <div className="mt-1 text-sm font-semibold text-ink-600 dark:text-ink-300">
                      per month after compression
                    </div>
                    <div className="mt-4 inline-flex rounded-full bg-brand-100 px-3 py-1 text-sm font-black text-brand-800 dark:bg-brand-900 dark:text-brand-100">
                      {percent(provider.totalSavings * 100)} cheaper than AWS
                    </div>
                  </div>
                  <div>
                    <div className="grid gap-2 text-sm font-bold">
                      <div className="flex items-center justify-between">
                        <span>Amazon S3 Standard</span>
                        <span>{usd(awsMonthly)}</span>
                      </div>
                      <div className="h-4 overflow-hidden rounded-full bg-rose-200 dark:bg-rose-950">
                        <div className="h-full w-full bg-rose-500" />
                      </div>
                      <div className="mt-2 flex items-center justify-between">
                        <span>{provider.shortName}, no compression</span>
                        <span>{usd(provider.backendOnly)}</span>
                      </div>
                      <div className="h-4 overflow-hidden rounded-full bg-cyan-100 dark:bg-cyan-950">
                        <div
                          className={`h-full bg-gradient-to-r ${provider.color}`}
                          style={{
                            width: `${Math.max(2, provider.backendOnly / awsMonthly * 100)}%`,
                          }}
                        />
                      </div>
                      <div className="mt-2 flex items-center justify-between">
                        <span>{provider.shortName} + DGP compression</span>
                        <span>{usd(provider.dgpMonthly)}</span>
                      </div>
                      <div className="h-4 overflow-hidden rounded-full bg-brand-100 dark:bg-brand-950">
                        <div
                          className={`h-full bg-gradient-to-r ${provider.color}`}
                          style={{
                            width: `${Math.max(2, provider.ratioToAws * 100)}%`,
                          }}
                        />
                      </div>
                    </div>
                    <p className="mt-4 text-xs leading-5 text-ink-500 dark:text-ink-400">
                      {provider.notes}{' '}
                      <a
                        href={provider.sourceHref}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="font-bold text-brand-700 hover:text-brand-800 dark:text-brand-300"
                      >
                        {provider.sourceLabel}
                      </a>
                    </p>
                  </div>
                </div>
              </div>
            ))}
          </div>
        </div>
      </Section>

      <Section
        eyebrow="Feature replacement"
        title="Amazon S3 enterprise feature map."
        intro="DGP does not try to clone every AWS storage class. It replaces the operational controls you lose when moving app-facing S3 API workloads to a cheaper S3-compatible backend."
      >
        <div className="overflow-hidden rounded-[2rem] border border-ink-200 bg-white shadow-2xl shadow-brand-950/10 dark:border-ink-700 dark:bg-ink-950">
          <div className="grid grid-cols-12 bg-ink-950 px-5 py-4 text-xs font-extrabold uppercase tracking-[0.18em] text-ink-300">
            <div className="col-span-3">AWS feature</div>
            <div className="col-span-4">In AWS</div>
            <div className="col-span-4">In DeltaGlider</div>
            <div className="col-span-1 text-right">Fit</div>
          </div>
          {FEATURE_ROWS.map((row, index) => (
            <div
              key={row.category}
              className={`grid grid-cols-12 gap-4 px-5 py-5 text-sm ${
                index % 2 === 0
                  ? 'bg-white dark:bg-ink-950'
                  : 'bg-ink-50 dark:bg-ink-900/70'
              }`}
            >
              <div className="col-span-12 font-black text-ink-900 dark:text-white md:col-span-3">
                {row.category}
              </div>
              <div className="col-span-12 leading-6 text-ink-600 dark:text-ink-300 md:col-span-4">
                {row.s3}
              </div>
              <div className="col-span-12 leading-6 text-ink-700 dark:text-ink-200 md:col-span-4">
                {row.dgp}
              </div>
              <div className="col-span-12 md:col-span-1 md:text-right">
                <div className="inline-flex flex-col items-start gap-1 md:items-end">
                  <span
                    className={`inline-flex rounded-full border px-2.5 py-1 text-[11px] font-black ${fitClass(row.fit)}`}
                  >
                    {row.fit}
                  </span>
                  <span className="text-[11px] font-semibold leading-4 text-ink-500 dark:text-ink-400">
                    {row.note}
                  </span>
                </div>
              </div>
            </div>
          ))}
        </div>
      </Section>

      <Section
        eyebrow="When it works"
        title="Best fit: hot S3 API workflows with repeated binary structure."
        intro="The largest reductions come when both levers fire: the backend is cheaper per TB and the data stream has repeated archive/build/dump/model bytes."
      >
        <div className="grid gap-5 md:grid-cols-3">
          <FeatureCard
            title="Artifact and build retention"
            body="CI outputs, release zips, package catalogs, installers, and archive bundles usually keep many adjacent versions."
          />
          <FeatureCard
            title="Backup and dump streams"
            body="Daily archives and database dumps often repeat most internal structure while still needing full-object reads."
          />
          <FeatureCard
            title="Model and asset variants"
            body="Fine-tuned checkpoints, game assets, and generated media bundles can be binary-similar across variants."
          />
        </div>
        <div className="mt-8 rounded-3xl border border-amber-300/50 bg-amber-50 p-6 text-sm leading-7 text-amber-950 dark:border-amber-500/30 dark:bg-amber-950/40 dark:text-amber-100">
          <strong>Not modeled here:</strong> egress, request pricing, retrieval
          fees, storage-class minimum durations, object-lock requirements,
          support plans, tax/VAT, and provider-specific limits. Use this page as
          the fast economic screen, then run a sizing pass on your real object
          stream.
        </div>
      </Section>

      <Section
        eyebrow="Next step"
        title="Bring one AWS bill and one object sample."
        intro="We can model backend price, compression ratio, request profile, egress, and which Amazon S3 controls must move into DeltaGlider before migration."
      >
        <div className="flex flex-wrap gap-3">
          <MailtoCTA subject={SUBJECT} label="Review our AWS migration" />
          <a
            href={`${REPO_URL}/tree/main/docs/benchmark`}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-2 rounded-lg border border-ink-300 bg-white px-5 py-3 font-semibold text-ink-800 hover:border-brand-400 hover:text-brand-700 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100 dark:hover:border-brand-300 dark:hover:text-brand-300"
          >
            Benchmark tooling <span aria-hidden>↗</span>
          </a>
        </div>
      </Section>
    </>
  );
}
