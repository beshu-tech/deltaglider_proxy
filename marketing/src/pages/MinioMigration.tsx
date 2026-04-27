import { FeatureCard } from '../components/FeatureCard';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
import { ScreenshotFrame } from '../components/ScreenshotFrame';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { minioMigrationMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const SUBJECT = 'MinIO migration';

const SCENARIOS = [
  {
    title: 'Storage exists. Control is missing.',
    body: 'You can find younger OSS object stores that move bytes. The hard part is getting IAM, policy, quotas, replication, and operator workflows in one place.',
  },
  {
    title: 'IAM semantics are sticky',
    body: 'Applications already rely on per-user keys, groups, prefixes, and conditional access. Rebuilding that around a storage-only server slows the migration.',
  },
  {
    title: 'Operations need guardrails',
    body: 'Cutovers need soft quotas, bucket freeze, sync state, replication history, and a UI operators can understand under pressure.',
  },
];

export function MinioMigration(): JSX.Element {
  return (
    <>
      <SEO meta={minioMigrationMeta} />
      <Hero
        eyebrow="Use case · MinIO migration"
        headline="Self-hosted S3 without losing the control plane."
        subhead="MinIO refugees can find storage engines. What is usually missing is the enterprise control plane: IAM, OAuth, ABAC, bucket policy, quotas, replication, sync, and an operator UI."
        cta={
          <>
            <MailtoCTA
              subject={SUBJECT}
              label="Plan your migration"
            />
          </>
        }
        illustration={
          <ScreenshotFrame
            src="screenshots/iam.jpg"
            alt="DeltaGlider Proxy IAM user management"
            caption="IAM control plane for MinIO migration: users, groups, S3 access keys, inherited permissions, and OAuth/OIDC mappings stay attached to the proxy."
            priority
          />
        }
      />
      <Section
        eyebrow="The gap"
        title="Storage alone is not enough."
        intro="The replacement must preserve the operational contract around S3, not only store objects."
      >
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {[
            'Per-user S3 access keys',
            'Groups with inherited permissions',
            'ABAC policies in AWS IAM grammar',
            'OAuth/OIDC login and claim mapping',
            'Public read-only prefixes',
            'Soft per-bucket quotas and bucket freeze',
            'Bucket routing aliases',
            'Object replication rules',
            'Encrypted multi-instance config DB sync',
          ].map((item) => (
            <div
              key={item}
              className="rounded-xl border border-ink-200 bg-white px-4 py-3 text-sm font-semibold text-ink-800 dark:border-ink-700 dark:bg-ink-800/50 dark:text-ink-100"
            >
              <span className="mr-2 text-brand-600 dark:text-brand-300">✓</span>
              {item}
            </div>
          ))}
        </div>
        <div className="mt-6 rounded-2xl border border-brand-300/50 bg-brand-50 p-5 dark:border-brand-500/30 dark:bg-brand-950/40">
          <div className="text-xs font-extrabold uppercase tracking-[0.18em] text-brand-700 dark:text-brand-300">
            Garage + DeltaGlider
          </div>
          <p className="mt-2 max-w-3xl text-sm leading-6 text-ink-700 dark:text-ink-200">
            Garage is a strong open-source storage layer. Its own S3
            compatibility reference is also a useful checklist for what belongs
            in a separate control plane: identity, policy, quotas, audit,
            replication operations, and operator workflows.
          </p>
          <a
            href="https://garagehq.deuxfleurs.fr/documentation/reference-manual/s3-compatibility"
            target="_blank"
            rel="noopener noreferrer"
            className="mt-3 inline-flex items-center gap-1 text-sm font-bold text-brand-700 hover:text-brand-800 dark:text-brand-300 dark:hover:text-brand-200"
          >
            Garage S3 compatibility reference
            <span aria-hidden>↗</span>
          </a>
        </div>
      </Section>
      <Section
        eyebrow="Real-world scenarios"
        title="Why MinIO migrations get stuck."
        intro="The data path is only one part of the product. The control plane is what keeps production usable."
      >
        <div className="grid gap-5 md:grid-cols-3">
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
        eyebrow="IAM"
        title="Enterprise access control for applications and teams."
      >
        <div className="grid gap-5 md:grid-cols-2">
          <FeatureCard
            title="Per-user credentials"
            body={
              <>
                Give applications their own S3 keys. Use groups, inherited
                permissions, prefix conditions, and source-IP conditions.
              </>
            }
            sourceLabel="src/iam/permissions.rs"
            sourceHref={`${REPO_URL}/blob/main/src/iam/permissions.rs`}
          />
          <FeatureCard
            title="Encrypted config DB"
            body={
              <>
                Users, groups, OAuth providers, and policies live in SQLCipher.
                Sync the encrypted DB across multiple proxy instances.
              </>
            }
            sourceLabel="src/config_db_sync.rs"
            sourceHref={`${REPO_URL}/blob/main/src/config_db_sync.rs`}
          />
          <FeatureCard
            title="OAuth and OIDC"
            body={
              <>
                Connect a provider and map claims to DeltaGlider groups from
                the admin UI.
              </>
            }
          />
          <FeatureCard
            title="No client rewrite"
            body={
              <>
                Keep existing SDKs and tools. The proxy speaks S3 with SigV4,
                so storage migration does not become an application migration.
              </>
            }
          />
        </div>
      </Section>
      <Section
        eyebrow="Bucket policy"
        title="Bucket controls stay in the control plane."
        intro="Configure compression, aliases, public prefixes, soft quotas, and read-only freeze per bucket."
      >
        <ScreenshotFrame
          src="screenshots/bucket-policies.jpg"
          alt="DeltaGlider Proxy per-bucket policy editor"
          caption="Bucket policy covers compression, aliases, public prefixes, soft quotas, and frozen buckets."
        />
      </Section>
      <Section
        eyebrow="Quotas"
        title="Soft write limits per bucket."
        intro="Use soft quotas to control growth. Set quota to zero to freeze a bucket during migration."
      >
        <div className="grid gap-5 md:grid-cols-3">
          <FeatureCard
            title="Soft cap"
            body="Set `quota_bytes` on a bucket. Writes above scanned usage are rejected."
            sourceLabel="docs/product/10-first-bucket.md"
            sourceHref={`${REPO_URL}/blob/main/docs/product/10-first-bucket.md`}
          />
          <FeatureCard
            title="Freeze mode"
            body="Set `quota_bytes: 0` to block writes while reads continue."
          />
          <FeatureCard
            title="Operator-visible"
            body="Quota is managed next to bucket policy, aliases, and public prefixes."
          />
        </div>
      </Section>
      <Section
        eyebrow="Next step"
        title="Map the control plane before moving bytes."
        intro="Share the IAM, bucket policy, quota, replication, and operator workflows you use today. We will map what ports cleanly and what needs adjustment."
      >
        <MailtoCTA subject={SUBJECT} label="Email us" />
      </Section>
    </>
  );
}
