import type { LucideIcon } from 'lucide-react';
import { Layers, LayoutDashboard, Shield } from 'lucide-react';
import { FeatureCard } from '../components/FeatureCard';
import { DeploymentPathGrid } from '../components/DeploymentPathGrid';
import { Hero } from '../components/Hero';
import { RustBuiltWithMark } from '../components/RustBuiltWithMark';
import { MailtoCTA } from '../components/MailtoCTA';
import { ProofStrip } from '../components/ProofStrip';
import { ScreenshotFrame } from '../components/ScreenshotFrame';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { UseCaseCarousel } from '../components/UseCaseCarousel';
import { LandingHeroSubhead } from '../components/LandingHeroSubhead';
import { SiteIcon } from '../icons/SiteIcon';
import { LUCIDE_STROKE } from '../icons/sizes';
import { USE_CASE_PATHS } from '../config/use-cases';
import { landingMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const OUTCOMES: readonly { icon: LucideIcon; label: string; body: string }[] = [
  {
    icon: Layers,
    label: 'Delta storage that stays invisible',
    body:
      'CI artifacts, builds, and versioned blobs repeat the same bytes. Where ratios win, persist compact xdelta3; GET still streams ordinary objects—no client changes.',
  },
  {
    icon: Shield,
    label: 'Identity and exposure in your boundary',
    body:
      'IAM, OAuth/OIDC mapping, ABAC, scoped public reads, zero trust encryption at rest, and audit visibility—without handing keys to another SaaS control plane.',
  },
  {
    icon: LayoutDashboard,
    label: 'Operations as a single product surface',
    body:
      'Backends, bucket policy, soft quotas, replication, TLS, caches, rate limits, Prometheus metrics, and logs from one embedded admin UI—less glue, fewer runbooks.',
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
    body: 'Copy objects between buckets or backends without bypassing the DeltaGlider engine. Operators can run rules on demand, pause or resume them, inspect history, and replicate deletes.',
    points: ['Source to destination rules', 'Run-now with pause/resume', 'History, failures, and optional delete replication'],
    screenshot: {
      src: 'screenshots/object-replication.jpg',
      alt: 'DeltaGlider Proxy object replication settings page',
      caption: 'Rule-based object replication with run-now controls, history, failures, and delete replication.',
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
        eyebrow="Enterprise S3"
        headline="Unified S3 enterprise control plane"
        subhead={<LandingHeroSubhead />}
        cta={
          <>
            <MailtoCTA
              subject="DeltaGlider Proxy inquiry"
              label="Model our storage"
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
        afterCta={<RustBuiltWithMark />}
        illustration={
          <ScreenshotFrame
            src="screenshots/filebrowser.jpg"
            alt="DeltaGlider Proxy file browser with bucket navigation and embedded admin entry points"
            caption="Bucket browser for S3-compatible object storage: switch buckets, navigate prefixes, inspect objects, upload files, and jump into the admin control plane."
            priority
          />
        }
      />
      <UseCaseCarousel />
      <ProofStrip />
      <Section
        eyebrow="Business impact"
        title="Stop paying to store the same bytes on repeat."
        intro={
          <p className="m-0 text-ink-600 dark:text-ink-300">
            DeltaGlider is an S3-compatible proxy: clients use ordinary SigV4 and your existing SDKs—no application changes. You aim it at MinIO, Amazon S3, or a
            filesystem backend. Where uploads repeat the same logical artifact, it can store compact xdelta3 deltas instead of full copies; IAM, OAuth, quotas,
            replication, metrics, audit, and the admin UI ship in one deployable service.
          </p>
        }
      >
        <div
          className="relative overflow-hidden rounded-3xl border border-ink-200/80 bg-gradient-to-b from-ink-50/95 via-white to-white p-6 shadow-sm shadow-ink-900/5 sm:p-8 dark:from-ink-900/55 dark:via-ink-950/30 dark:to-ink-950/20 dark:border-ink-600/50 dark:shadow-black/15"
        >
          <div
            className="pointer-events-none absolute inset-x-0 top-0 h-px bg-gradient-to-r from-transparent via-brand-400/50 to-transparent dark:via-brand-500/30"
            aria-hidden
          />
          <ul className="m-0 grid list-none grid-cols-1 gap-5 p-0 lg:grid-cols-3">
            {OUTCOMES.map((item) => (
              <li
                key={item.label}
                className="relative flex h-full flex-col rounded-2xl border border-ink-200/90 bg-white/90 p-6 dark:border-ink-600/60 dark:bg-ink-900/55"
              >
                <div
                  className="mb-4 flex h-12 w-12 items-center justify-center rounded-2xl border border-brand-200/80 bg-gradient-to-br from-brand-50 to-white text-brand-700 shadow-sm dark:border-brand-800/60 dark:from-brand-950/80 dark:to-ink-900/80 dark:text-brand-200"
                  aria-hidden
                >
                  <SiteIcon icon={item.icon} className="h-6 w-6" strokeWidth={LUCIDE_STROKE + 0.1} />
                </div>
                <h3 className="m-0 text-lg font-extrabold leading-snug text-ink-900 dark:text-ink-50 sm:text-xl">
                  {item.label}
                </h3>
                <p className="mb-0 mt-3 text-[0.95rem] leading-relaxed text-ink-600 dark:text-ink-300 sm:text-[15px] sm:leading-relaxed">
                  {item.body}
                </p>
              </li>
            ))}
          </ul>
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
        title="Common deployment paths"
        intro="Start with the problem you have today. Each path below opens a focused page on that scenario (the whole card is the link)."
      >
        <div className="relative overflow-hidden rounded-2xl border border-ink-200/70 bg-gradient-to-b from-ink-50/90 via-white to-white p-5 shadow-sm shadow-ink-900/5 sm:p-7 dark:from-ink-900/60 dark:via-ink-950/40 dark:to-ink-950/20 dark:border-ink-600/50 dark:shadow-black/20">
          <div
            className="pointer-events-none absolute inset-x-0 top-0 h-px bg-gradient-to-r from-transparent via-brand-300/50 to-transparent dark:via-brand-500/20"
            aria-hidden
          />
          <DeploymentPathGrid paths={USE_CASE_PATHS} />
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
