import { FeatureCard } from '../components/FeatureCard';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
import { ScreenshotFrame } from '../components/ScreenshotFrame';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { s3SaasMeta } from '../seo/pages';

const SUBJECT = 'S3 SaaS control plane';

const TRADEOFFS = [
  {
    title: 'AWS-grade controls',
    body: 'AWS S3 sets the benchmark for enterprise object-storage control: IAM, policies, quotas, audit, replication, and operational visibility.',
  },
  {
    title: 'Lower-cost storage',
    body: 'Many newer S3-compatible SaaS providers are much cheaper and good at basic object storage, but stop short of the enterprise control plane.',
  },
  {
    title: 'DeltaGlider layer',
    body: 'Put DeltaGlider in front to keep S3 workflows, add the missing controls, and route data to the backend that makes economic sense.',
  },
];

const SCENARIOS = [
  {
    title: 'Storage bill pressure',
    body: 'You want to move cold artifacts, dumps, or backups to a lower-cost S3-compatible provider without losing operational control.',
  },
  {
    title: 'Governance blocks migration',
    body: 'Security teams accept cheaper storage only if access control, encryption, public prefixes, audit, quotas, and replication remain explicit.',
  },
  {
    title: 'Provider optionality',
    body: 'Keep applications talking S3 while changing the backend economics behind the proxy.',
  },
];

export function S3Saas(): JSX.Element {
  return (
    <>
      <SEO meta={s3SaasMeta} />
      <Hero
        eyebrow="Use case · cheaper S3-compatible storage"
        headline="Use cheaper S3 storage without giving up enterprise controls."
        subhead="AWS S3 is where teams expect a mature enterprise control plane. Many lower-cost S3-compatible services deliver basic storage, but not the IAM, policy, encryption, quota, replication, audit, and operator workflows production teams need. DeltaGlider adds that layer."
        cta={
          <MailtoCTA
            subject={SUBJECT}
            label="Map your storage savings"
          />
        }
        illustration={
          <ScreenshotFrame
            src="screenshots/bucket-policies.jpg"
            alt="DeltaGlider Proxy bucket policy controls"
            caption="Bucket policy controls for cheaper S3-compatible storage: compression, aliases, public prefixes, soft quotas, and bucket freeze."
            priority
          />
        }
      />
      <Section
        eyebrow="The tradeoff"
        title="Cheap storage is easy. Enterprise control is the gap."
        intro="The product story is not just cheaper bytes. It is cheaper bytes with encryption, policy, and operations your platform team can trust."
      >
        <div className="grid gap-5 md:grid-cols-3">
          {TRADEOFFS.map((item) => (
            <FeatureCard
              key={item.title}
              title={item.title}
              body={item.body}
            />
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Real-world scenarios"
        title="Where DeltaGlider fits."
        intro="Use it when low-cost S3-compatible storage is attractive, but the missing control plane blocks adoption."
      >
        <div className="grid gap-5 md:grid-cols-3">
          {SCENARIOS.map((item) => (
            <FeatureCard
              key={item.title}
              title={item.title}
              body={item.body}
            />
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Control plane"
        title="Add the controls in front of the backend."
        intro="DeltaGlider centralizes encryption, policy, and operations while your objects land in the storage backend you choose."
      >
        <div className="grid gap-8 lg:grid-cols-2 lg:items-center">
          <ScreenshotFrame
            src="screenshots/iam.jpg"
            alt="DeltaGlider Proxy IAM controls"
            caption="IAM, groups, S3 credentials, OAuth/OIDC, and mapping rules stay in the proxy control plane."
          />
          <div className="rounded-3xl border border-ink-200 bg-white/80 p-7 shadow-lg shadow-brand-950/5 backdrop-blur dark:border-ink-700 dark:bg-ink-900/70">
            <div className="text-xs font-extrabold uppercase tracking-widest text-brand-600 dark:text-brand-300">
              Production controls
            </div>
            <h3 className="mt-3 text-3xl font-extrabold tracking-tight text-ink-900 dark:text-ink-50">
              Keep governance portable.
            </h3>
            <p className="mt-4 text-base leading-relaxed text-ink-600 dark:text-ink-300">
              Manage identity, encryption, public access, quotas, replication,
              audit, and config sync in one layer instead of rebuilding those
              controls for each storage provider.
            </p>
            <ul className="mt-6 grid gap-3">
              {[
                'IAM, OAuth, groups, and ABAC',
                'Proxy-side encryption with local key custody',
                'Bucket policy, public prefixes, and soft quotas',
                'Replication, metrics, audit, and encrypted config sync',
              ].map((point) => (
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
      </Section>
      <Section
        eyebrow="Next step"
        title="Compare the control plane, not only the storage price."
        intro="Bring the cheaper S3 provider you are considering and the controls you need to preserve. We will map the deployment shape."
      >
        <MailtoCTA subject={SUBJECT} label="Review a provider fit" />
      </Section>
    </>
  );
}
