import { FeatureCard } from '../components/FeatureCard';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
import { ScreenshotFrame } from '../components/ScreenshotFrame';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { versioningMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const SUBJECT = 'Artifact storage savings';

const DELTA_TYPES = [
  '.zip',
  '.tar',
  '.tgz',
  '.tar.gz',
  '.tar.bz2',
  '.jar',
  '.war',
  '.ear',
  '.sql',
  '.dump',
  '.bak',
  '.backup',
  '.rar',
  '.7z',
  '.dmg',
  '.iso',
];

const FLOW = [
  {
    step: '1',
    title: 'Point clients at the proxy',
    body: 'Keep aws-cli, boto3, SDKs, rclone, and MinIO Client. The wire protocol remains S3 + SigV4.',
  },
  {
    step: '2',
    title: 'Upload normal artifacts',
    body: 'Delta-eligible archives and binary dumps route through xdelta3 against a per-prefix reference baseline.',
  },
  {
    step: '3',
    title: 'Measure the truth',
    body: 'Dashboard and Prometheus metrics report original bytes, stored bytes, cache behavior, request rate, and errors.',
  },
];

const SCENARIOS = [
  {
    title: 'Backup archives',
    body: 'Daily backup bundles often contain the same files, tables, or blocks with a small change set. Store every point in time without paying full price for repeated bytes.',
  },
  {
    title: 'Software catalogs',
    body: 'Release catalogs keep many builds, installers, packages, and archives. Adjacent versions are often highly similar, especially when packaged by the same pipeline.',
  },
  {
    title: 'Media and texture variants',
    body: 'Texture packs, asset bundles, and generated media variants can share large binary regions. Delta storage helps when variants are stored together over time.',
  },
  {
    title: 'AI model variants',
    body: 'Fine-tuned checkpoints and model variants can be binary-similar while still needing separate full-object reads. This is where the compression benefit can compound quickly.',
  },
];

export function Versioning(): JSX.Element {
  return (
    <>
      <SEO meta={versioningMeta} />
      <Hero
        eyebrow="Use case · artifact storage"
        headline="Reduce storage for binary-similar versions."
        subhead="Backups, software catalogs, media asset variants, and AI model variants often share most of their bytes. DeltaGlider stores the differences while clients still write and read full S3 objects. This is storage deduplication, not S3 object versioning."
        cta={
          <>
            <MailtoCTA
              subject={SUBJECT}
              label="Get a savings estimate"
            />
          </>
        }
        illustration={
          <ScreenshotFrame
            src="screenshots/analytics.jpg"
            alt="DeltaGlider Proxy analytics dashboard showing per-bucket compression savings"
            caption="Analytics dashboard for artifact storage: compare original bytes, stored bytes, savings percentage, cache behavior, and request health."
            priority
          />
        }
      />
      <Section
        eyebrow="How it runs"
        title="Three steps. No client rewrite."
        intro="Use your existing S3 tooling."
      >
        <div className="grid gap-5 md:grid-cols-3">
          {FLOW.map((item) => (
            <div
              key={item.step}
              className="rounded-2xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/50"
            >
              <div className="flex h-10 w-10 items-center justify-center rounded-full bg-brand-100 text-lg font-extrabold text-brand-800 dark:bg-brand-900 dark:text-brand-100">
                {item.step}
              </div>
              <h3 className="mt-5 text-lg font-extrabold text-ink-900 dark:text-ink-50">
                {item.title}
              </h3>
              <p className="mt-2 text-[15px] leading-relaxed text-ink-600 dark:text-ink-300">
                {item.body}
              </p>
            </div>
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Important distinction"
        title="Not S3 object versioning."
        intro="DeltaGlider does not restore old S3 object versions today. It stores repeated artifact releases more efficiently while preserving normal full-object reads."
      >
        <div className="grid gap-5 md:grid-cols-2">
          <FeatureCard
            title="What it does"
            body="Reduces storage for repeated binaries by storing deltas behind the S3 API."
          />
          <FeatureCard
            title="What it does not do"
            body="Expose S3 version IDs or provide object-version restore workflows."
          />
        </div>
      </Section>
      <Section
        eyebrow="Real-world scenarios"
        title="Where the savings usually come from."
        intro="The benefit grows when you keep many versions that are internally similar. The more repeated binary structure you store, the more delta compression can compound."
      >
        <div className="grid gap-5 md:grid-cols-2 lg:grid-cols-4">
          {SCENARIOS.map((scenario) => (
            <FeatureCard
              key={scenario.title}
              title={scenario.title}
              body={scenario.body}
            />
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Storage efficiency"
        title="Delta compression where it pays off."
      >
        <div className="grid gap-5 md:grid-cols-3">
          <FeatureCard
            title="Smart routing"
            body={
              <>
                Repeated archives and binary dumps use xdelta3 against a
                per-prefix baseline. Media and other poor-fit files pass through
                unchanged.
              </>
            }
            sourceLabel="src/deltaglider/file_router.rs"
            sourceHref={`${REPO_URL}/blob/main/src/deltaglider/file_router.rs`}
          />
          <FeatureCard
            title="Drop-in S3 API"
            body={
              <>
                Existing S3 tools keep working. The proxy reconstructs full
                objects on GET, so applications never see delta files.
              </>
            }
          />
          <FeatureCard
            title="Live savings analytics"
            body={
              <>
                Per-bucket compression ratios, bytes saved, cache behavior, and
                request health from the dashboard. Prometheus metrics{' '}
                <code className="text-sm">deltaglider_delta_compression_ratio</code>{' '}
                and{' '}
                <code className="text-sm">delta_bytes_saved_total</code> are
                exposed at <code className="text-sm">/metrics</code>.
              </>
            }
            sourceLabel="src/api/handlers/status.rs"
            sourceHref={`${REPO_URL}/blob/main/src/api/handlers/status.rs`}
          />
        </div>
      </Section>
      <Section
        eyebrow="Specifics"
        title="Default delta candidates, not a fixed promise."
        intro="These extensions are a starting point. You can add or remove file types for your workload. Savings depend on internal structure and binary similarity across versions."
      >
        <div className="rounded-xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/40">
          <div className="flex flex-wrap gap-2">
            {DELTA_TYPES.map((ext) => (
              <code
                key={ext}
                className="rounded-md bg-ink-100 px-2.5 py-1 text-sm font-bold text-ink-800 dark:bg-ink-700 dark:text-ink-100"
              >
                {ext}
              </code>
            ))}
          </div>
          <p className="mt-4 text-sm text-ink-600 dark:text-ink-400">
            Default routing lives in{' '}
            <a
              href={`${REPO_URL}/blob/main/src/deltaglider/file_router.rs`}
              target="_blank"
              rel="noopener noreferrer"
              className="text-brand-700 hover:text-brand-800 dark:text-brand-300"
            >
              src/deltaglider/file_router.rs
            </a>
            . The right answer is workload-specific: a custom archive may delta
            well; a compressed archive with shuffled blocks may not.
          </p>
        </div>
      </Section>
      <Section
        eyebrow="Measure"
        title="Use your own artifact stream."
        intro="Savings depend on churn rate and file format. The dashboard shows original bytes, stored bytes, latency, and cache behavior."
      >
        <MailtoCTA subject={SUBJECT} label="Tell us about your pipeline" />
      </Section>
    </>
  );
}
