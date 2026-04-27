import { Link } from 'react-router-dom';
import { FeatureCard } from '../components/FeatureCard';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
import { ProofStrip } from '../components/ProofStrip';
import { ScreenshotFrame } from '../components/ScreenshotFrame';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { landingMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

interface NicheCard {
  to: string;
  voice: string;
  who: string;
  payoff: string;
}

const NICHES: readonly NicheCard[] = [
  {
    to: '/regulated/',
    voice: 'Security & compliance',
    who: 'Regulated workloads',
    payoff:
      'Use cheap or untrusted storage safely: encrypt before the backend, keep keys on trusted premises, then add compression.',
  },
  {
    to: '/artifact-storage/',
    voice: 'Storage efficiency',
    who: 'Artifact storage',
    payoff:
      'Store backup archives, software catalogs, media asset variants, and AI model variants as deltas.',
  },
  {
    to: '/s3-saas-control-plane/',
    voice: 'Cheaper S3 SaaS',
    who: 'Enterprise controls',
    payoff:
      'Use lower-cost S3-compatible storage without giving up IAM, policy, quotas, replication, audit, and operator UI.',
  },
  {
    to: '/minio-migration/',
    voice: 'Enterprise control plane',
    who: 'MinIO migration',
    payoff:
      'Keep self-hosted S3 plus the controls MinIO refugees miss: IAM, OAuth, policy, quotas, replication, and admin UI.',
  },
];

const OUTCOMES = [
  {
    label: 'Lower storage growth',
    body: 'Store repeated binaries as compact deltas while clients continue to use normal S3.',
  },
  {
    label: 'Control access',
    body: 'Manage users, groups, OAuth, ABAC permissions, public prefixes, and audit visibility from your own environment.',
  },
  {
    label: 'Run it simply',
    body: 'Configure backends, buckets, quotas, replication, TLS, caches, limits, and logs from one admin UI.',
  },
];

const FEATURE_MAP = [
  {
    group: 'Storage',
    summary: 'Reduce backend growth without changing the client contract.',
    items: [
      'S3-compatible API with SigV4',
      'xdelta3 delta storage for repeated binaries',
      'Filesystem, AWS S3, and MinIO-compatible backends',
      'AES-256-GCM proxy-side encryption',
    ],
  },
  {
    group: 'Access',
    summary: 'Control who can read, write, list, and publish.',
    items: [
      'Per-user IAM and groups',
      'ABAC permissions',
      'OAuth/OIDC group mapping',
      'Public read-only prefixes',
    ],
  },
  {
    group: 'Operations',
    summary: 'Run the proxy as production infrastructure.',
    items: [
      'Soft per-bucket quotas and bucket freeze',
      'Object replication with delete replication',
      'Prometheus metrics and embedded dashboard',
      'In-memory audit ring and encrypted config DB sync',
    ],
  },
];

const PRODUCT_SURFACES = [
  {
    title: 'Monitor storage and runtime health.',
    eyebrow: 'Observability',
    body: 'Track requests, latency, cache behavior, object counts, memory, and error rate from the embedded dashboard. Use Prometheus metrics when you need long-term monitoring.',
    points: ['Request rate and latency', 'Cache and memory visibility', 'Prometheus-ready metrics'],
    screenshot: {
      src: 'screenshots/analytics.jpg',
      alt: 'DeltaGlider Proxy operations dashboard with request, cache, and error metrics',
      caption: 'Dashboard view for request health, cache behavior, memory, errors, and object totals.',
    },
  },
  {
    title: 'Replicate through the proxy.',
    eyebrow: 'Replication',
    body: 'Copy objects between buckets or backends without bypassing the DeltaGlider engine. Rules can run on a schedule, run on demand, pause, resume, and replicate deletes.',
    points: ['Source to destination rules', 'Run history and failures', 'Optional delete replication'],
    screenshot: {
      src: 'screenshots/object-replication.jpg',
      alt: 'DeltaGlider Proxy object replication settings page',
      caption: 'Rule-based object replication with scheduler controls, history, failures, and delete replication.',
    },
  },
  {
    title: 'Manage access without leaving the product.',
    eyebrow: 'Identity',
    body: 'Create S3 credentials, assign groups, enforce ABAC permissions, and map OAuth/OIDC claims to DeltaGlider groups from the admin UI.',
    points: ['Per-user S3 credentials', 'Groups and ABAC permissions', 'OAuth/OIDC group mapping'],
    screenshot: {
      src: 'screenshots/iam.jpg',
      alt: 'DeltaGlider Proxy IAM users page',
      caption: 'IAM surface for S3 credentials, users, groups, ABAC permissions, OAuth, and group mapping.',
    },
  },
  {
    title: 'Control bucket behavior explicitly.',
    eyebrow: 'Bucket policy',
    body: 'Set compression policy, routing aliases, public read-only prefixes, soft quotas, and read-only freeze at the bucket level.',
    points: ['Compression policy', 'Public prefixes and aliases', 'Soft quotas and bucket freeze'],
    screenshot: {
      src: 'screenshots/bucket-policies.jpg',
      alt: 'DeltaGlider Proxy per-bucket policies page',
      caption: 'Bucket policy surface for compression, aliases, public prefixes, soft quotas, and frozen buckets.',
    },
  },
];

export function Landing(): JSX.Element {
  return (
    <>
      <SEO meta={landingMeta} />
      <Hero
        eyebrow="S3-compatible storage proxy"
        headline="Smaller object storage. Unchanged S3 workflows."
        subhead="DeltaGlider Proxy stores repeated binaries as compact deltas behind an S3-compatible API. Your applications keep using the same S3 clients; operators get the controls needed for production."
        cta={
          <>
            <MailtoCTA
              subject="DeltaGlider Proxy inquiry"
              label="Get a storage estimate"
            />
            <a
              href={REPO_URL}
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-2 rounded-lg border border-ink-300 bg-white px-5 py-3 font-semibold text-ink-800 hover:border-brand-400 hover:text-brand-700 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100 dark:hover:border-brand-300 dark:hover:text-brand-300"
            >
              View on GitHub
            </a>
          </>
        }
        illustration={
          <ScreenshotFrame
            src="screenshots/filebrowser.jpg"
            alt="DeltaGlider Proxy file browser with bucket navigation and embedded admin entry points"
            caption="Bucket browser for S3-compatible object storage: switch buckets, navigate prefixes, inspect objects, upload files, and jump into the admin control plane."
            priority
          />
        }
      />
      <ProofStrip />
      <Section
        eyebrow="Business impact"
        title="Reduce cost, keep control, avoid rewrites."
        intro="DeltaGlider is designed for teams with growing S3-compatible storage and existing clients they do not want to change."
      >
        <div className="grid gap-5 md:grid-cols-2 lg:grid-cols-4">
          {OUTCOMES.map((item) => (
            <div
              key={item.label}
              className="rounded-2xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/50"
            >
              <div className="text-xl font-extrabold text-ink-900 dark:text-ink-50">
                {item.label}
              </div>
              <p className="mt-3 text-[15px] leading-relaxed text-ink-600 dark:text-ink-300">
                {item.body}
              </p>
            </div>
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Operations"
        title="Everything operators need in one place."
        intro="Monitor usage, manage identity, configure bucket policy, set soft quotas, and run replication from the embedded admin UI."
      >
        <div className="space-y-14">
          {PRODUCT_SURFACES.map((surface, index) => {
            const isReversed = index % 2 === 1;
            return (
              <div
                key={surface.title}
                className="grid gap-8 lg:grid-cols-2 lg:items-center"
              >
                <div className={isReversed ? 'lg:order-2' : undefined}>
                  <ScreenshotFrame
                    src={surface.screenshot.src}
                    alt={surface.screenshot.alt}
                    caption={surface.screenshot.caption}
                  />
                </div>
                <div
                  className={`rounded-3xl border border-ink-200 bg-white/80 p-7 shadow-lg shadow-brand-950/5 backdrop-blur dark:border-ink-700 dark:bg-ink-900/70 ${
                    isReversed ? 'lg:order-1' : ''
                  }`}
                >
                  <div className="text-xs font-extrabold uppercase tracking-widest text-brand-600 dark:text-brand-300">
                    {surface.eyebrow}
                  </div>
                  <h3 className="mt-3 text-3xl font-extrabold tracking-tight text-ink-900 dark:text-ink-50">
                    {surface.title}
                  </h3>
                  <p className="mt-4 text-base leading-relaxed text-ink-600 dark:text-ink-300">
                    {surface.body}
                  </p>
                  <ul className="mt-6 grid gap-3">
                    {surface.points.map((point) => (
                      <li
                        key={point}
                        className="flex gap-3 rounded-xl bg-ink-50 px-4 py-3 text-sm font-bold text-ink-800 dark:bg-ink-800 dark:text-ink-100"
                      >
                        <span className="text-brand-600 dark:text-brand-300">✓</span>
                        <span>{point}</span>
                      </li>
                    ))}
                  </ul>
                </div>
              </div>
            );
          })}
        </div>
      </Section>
      <Section
        eyebrow="Capabilities"
        title="Feature map."
        intro="Storage efficiency is the core. Access and operations controls make it deployable."
      >
        <div className="relative rounded-3xl border border-ink-200 bg-white/85 p-5 shadow-xl shadow-brand-950/5 backdrop-blur dark:border-ink-700 dark:bg-ink-900/70">
          <div className="absolute left-[16.66%] right-[16.66%] top-16 hidden h-px bg-gradient-to-r from-transparent via-brand-300 to-transparent lg:block" />
          <div className="grid gap-5 lg:grid-cols-3">
            {FEATURE_MAP.map((group, index) => (
              <div
                key={group.group}
                className="relative rounded-2xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/70"
              >
                <div className="flex items-center gap-3">
                  <div className="flex h-11 w-11 items-center justify-center rounded-2xl bg-brand-100 text-lg font-extrabold text-brand-800 dark:bg-brand-900/80 dark:text-brand-100">
                    {index + 1}
                  </div>
                  <div>
                    <div className="text-lg font-extrabold text-ink-900 dark:text-ink-50">
                      {group.group}
                    </div>
                    <div className="text-xs font-bold uppercase tracking-widest text-brand-600 dark:text-brand-300">
                      Layer
                    </div>
                  </div>
                </div>
                <p className="mt-4 text-sm leading-relaxed text-ink-600 dark:text-ink-300">
                  {group.summary}
                </p>
                <ul className="mt-5 space-y-3">
                  {group.items.map((item) => (
                    <li
                      key={item}
                      className="flex gap-3 rounded-xl bg-ink-50 px-3 py-2 text-sm font-semibold text-ink-800 dark:bg-ink-900/80 dark:text-ink-100"
                    >
                      <span className="text-brand-600 dark:text-brand-300">✓</span>
                      <span>{item}</span>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>
        </div>
      </Section>
      <Section
        eyebrow="Use cases"
        title="Common deployment paths."
        intro="Start with the problem you have today."
      >
        <div className="grid gap-5 md:grid-cols-3">
          {NICHES.map((niche) => (
            <Link
              key={niche.to}
              to={niche.to}
              className="group rounded-xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/40 hover:border-brand-400 hover:shadow-md transition-all"
            >
              <div className="text-xs font-bold uppercase tracking-widest text-brand-600 dark:text-brand-300">
                {niche.voice}
              </div>
              <div className="mt-4 text-xl font-extrabold text-ink-900 dark:text-ink-50">
                {niche.who}
              </div>
              <p className="mt-2 text-[15px] text-ink-600 dark:text-ink-300 leading-relaxed">
                {niche.payoff}
              </p>
              <div className="mt-5 inline-flex items-center gap-1 text-sm font-semibold text-brand-700 group-hover:text-brand-800 dark:text-brand-300 dark:group-hover:text-brand-200">
                Read the pitch <span aria-hidden>→</span>
              </div>
            </Link>
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Technical proof"
        title="Verify the implementation."
        intro="Key subsystems are small enough to inspect."
      >
        <div className="grid gap-5 md:grid-cols-2">
          <FeatureCard
            title="xdelta3 delta engine"
            body="Uses the xdelta3 CLI for portable, inspectable deltas and compatibility with the original DeltaGlider format."
            sourceLabel="src/deltaglider/codec.rs"
            sourceHref={`${REPO_URL}/blob/main/src/deltaglider/codec.rs`}
          />
          <FeatureCard
            title="S3-compatible front end"
            body="Supports S3-compatible object workflows including SigV4, ranges, conditionals, copy, multipart uploads, and S3-shaped errors."
            sourceLabel="src/s3_adapter_s3s.rs"
            sourceHref={`${REPO_URL}/blob/main/src/s3_adapter_s3s.rs`}
          />
          <FeatureCard
            title="Governance built in"
            body="IAM, OAuth, admission, bucket policy, quotas, audit, and config sync are managed through the admin UI and API."
            sourceLabel="src/api/admin"
            sourceHref={`${REPO_URL}/tree/main/src/api/admin`}
          />
          <FeatureCard
            title="Rule-based replication"
            body="Replication rules, run history, failure records, and continuation state are tracked by the proxy."
            sourceLabel="src/replication"
            sourceHref={`${REPO_URL}/tree/main/src/replication`}
          />
        </div>
      </Section>
      <Section
        eyebrow="Next step"
        title="Measure it on your data."
        intro="Run it beside an existing S3 endpoint and compare original bytes, stored bytes, latency, and cache behavior."
      >
        <div className="flex flex-wrap gap-3">
          <MailtoCTA
            subject="DeltaGlider Proxy inquiry"
            label="Run a sizing pass"
          />
        </div>
      </Section>
    </>
  );
}
