import { FeatureCard } from '../components/FeatureCard';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
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

export function Versioning(): JSX.Element {
  return (
    <>
      <SEO meta={versioningMeta} />
      <Hero
        eyebrow="Use case · artifact versioning"
        headline="Up to 95% less S3 spend on artifacts you're already storing."
        subhead="A typical CI pipeline pushes 20 near-identical builds a day. Each is ~99% the same bytes as the last. S3 charges you for all of it. DeltaGlider Proxy doesn't."
        cta={
          <>
            <MailtoCTA
              subject={SUBJECT}
              label="Get a savings estimate"
            />
          </>
        }
        illustration={
          <img
            src="screenshots/analytics.jpg"
            alt="DeltaGlider Proxy analytics dashboard showing per-bucket compression savings"
            loading="eager"
            className="block w-full h-auto"
          />
        }
      />
      <Section
        eyebrow="The problem"
        title="Your artifact bucket is mostly redundancy."
        intro="If your team ships a 200 MB JAR every commit, you're storing 200 MB × 30 commits a month, when 99% of the bytes haven't changed. The diff between two builds is a few hundred KB — but standard S3 has no way to know that."
      >
        <></>
      </Section>
      <Section
        eyebrow="What ships today"
        title="Transparent xdelta3 on the file types it actually helps."
      >
        <div className="grid gap-5 md:grid-cols-3">
          <FeatureCard
            title="Smart routing"
            body={
              <>
                Versioned archives and binaries get xdelta3'd against a per-prefix
                reference baseline. Already-compressed media (PNG, JPG, MP4, PDF)
                passes through untouched — no wasted CPU.
              </>
            }
            sourceLabel="src/deltaglider/file_router.rs"
            sourceHref={`${REPO_URL}/blob/main/src/deltaglider/file_router.rs`}
          />
          <FeatureCard
            title="Drop-in S3 API"
            body={
              <>
                SigV4 on the wire. Your existing boto3, aws-sdk-java, rclone,
                aws-cli, MinIO Client — all keep working. Compression is invisible
                to clients; the proxy reconstructs the full object on GET via
                xdelta3 and streams it back.
              </>
            }
          />
          <FeatureCard
            title="Live savings analytics"
            body={
              <>
                Per-bucket compression ratios, bytes saved, and cost projections
                from the built-in dashboard. Prometheus metrics{' '}
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
        title="Which file types DeltaGlider compresses."
        intro="Delta-eligible types — i.e. the ones where successive versions usually share most of their bytes — are routed through xdelta3 against a per-prefix reference. Everything else is stored as-is."
      >
        <div className="rounded-xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/40">
          <div className="flex flex-wrap gap-2">
            {DELTA_TYPES.map((ext) => (
              <code
                key={ext}
                className="rounded-md bg-ink-100 px-2.5 py-1 text-sm font-mono text-ink-800 dark:bg-ink-700 dark:text-ink-100"
              >
                {ext}
              </code>
            ))}
          </div>
          <p className="mt-4 text-sm text-ink-600 dark:text-ink-400">
            Source of truth:{' '}
            <a
              href={`${REPO_URL}/blob/main/src/deltaglider/file_router.rs`}
              target="_blank"
              rel="noopener noreferrer"
              className="text-brand-700 hover:text-brand-800 dark:text-brand-300"
            >
              src/deltaglider/file_router.rs
            </a>
            . The list is conservative — extending it for your specific format is
            usually a one-line patch.
          </p>
        </div>
      </Section>
      <Section
        eyebrow="An honest note on numbers"
        title="The screenshot above is what it looks like on real artifacts."
        intro="We're not going to fabricate a benchmark. The compression ratio you see depends on your build's churn rate, format, and reference cadence. The dashboard runs on your own data on day one — point your CI at the proxy for a week and you'll know exactly what you're saving."
      >
        <MailtoCTA subject={SUBJECT} label="Tell us about your pipeline" />
      </Section>
    </>
  );
}
