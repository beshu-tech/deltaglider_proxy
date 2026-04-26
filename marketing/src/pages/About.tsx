import { FeatureCard } from '../components/FeatureCard';
import { MailtoCTA } from '../components/MailtoCTA';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { aboutMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const SECTORS = [
  'Software teams with large artifact stores',
  'Security-conscious platform teams',
  'Research and engineering groups',
  'Financial and regulated operators',
  'Teams replacing MinIO deployments',
  'Organizations standardizing on S3-compatible APIs',
];

export function About(): JSX.Element {
  return (
    <>
      <SEO meta={aboutMeta} />
      <Section
        eyebrow="About"
        title="DeltaGlider Proxy is built by Beshu Tech."
        intro="We build infrastructure software for teams that need standard protocols, strong access control, and practical operations. DeltaGlider Proxy is an open-source S3-compatible storage proxy; commercial support and services are available separately."
      >
        <div className="grid gap-5 md:grid-cols-3">
          <FeatureCard
            title="S3-compatible"
            body="Applications keep using S3 clients and SigV4. The proxy handles compression, policy, and governance behind the API."
          />
          <FeatureCard
            title="Operator-first"
            body="IAM, OAuth, quotas, replication, metrics, audit, and config sync are managed from one deployable control plane."
          />
          <FeatureCard
            title="Open source"
            body="The implementation is inspectable, testable, and available on GitHub."
            sourceLabel="GitHub repository"
            sourceHref={REPO_URL}
          />
        </div>
      </Section>
      <Section
        eyebrow="Trusted use cases"
        title="Built for serious storage workflows."
        intro="DeltaGlider is useful when object storage is already part of your infrastructure and the problem is cost, governance, or migration complexity."
      >
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {SECTORS.map((sector) => (
            <div
              key={sector}
              className="rounded-xl border border-ink-200 bg-white px-4 py-3 text-sm font-semibold text-ink-800 dark:border-ink-700 dark:bg-ink-800/50 dark:text-ink-100"
            >
              <span className="mr-2 text-brand-600 dark:text-brand-300">✓</span>
              {sector}
            </div>
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Contact"
        title="Talk to the engineering team."
        intro="If you are evaluating object storage efficiency, MinIO migration, or regulated S3-compatible storage, send a short note."
      >
        <MailtoCTA subject="DeltaGlider Proxy inquiry" label="Contact Beshu Tech" />
      </Section>
    </>
  );
}
