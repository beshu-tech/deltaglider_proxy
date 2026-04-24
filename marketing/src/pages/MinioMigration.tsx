import { FeatureCard } from '../components/FeatureCard';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
import { RoadmapRibbon } from '../components/RoadmapRibbon';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { minioMigrationMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const SUBJECT = 'MinIO migration';

export function MinioMigration(): JSX.Element {
  return (
    <>
      <SEO meta={minioMigrationMeta} />
      <Hero
        eyebrow="Use case · MinIO migration"
        headline="If you liked MinIO's ABAC and hated its licensing drama — this is your off-ramp."
        subhead="MinIO's license and feature changes left teams with production workloads and no clean path forward. Most alternatives drop the IAM sophistication. We didn't."
        cta={
          <>
            <MailtoCTA
              subject={SUBJECT}
              label="Plan your migration"
            />
          </>
        }
        illustration={
          <img
            src="screenshots/iam.jpg"
            alt="DeltaGlider Proxy IAM user management"
            loading="eager"
            className="block w-full h-auto"
          />
        }
      />
      <Section
        eyebrow="The problem"
        title="You built workflows around MinIO's IAM. Now you can't move."
        intro="Most S3-compatible servers have a bucket-policy mindset. You need per-user keys, groups, OIDC sign-in, conditional policies — the things you actually use in production."
      >
        <></>
      </Section>
      <Section
        eyebrow="What ships today"
        title="The MinIO IAM features you actually relied on."
      >
        <div className="grid gap-5 md:grid-cols-2">
          <FeatureCard
            title="Per-user S3 credentials, groups, ABAC"
            body={
              <>
                Per-user access key + secret pairs. Groups with inherited
                permissions. ABAC policies in AWS IAM grammar (parsed by{' '}
                <code className="text-sm">iam-rs</code>) including IP-range and
                prefix conditions. Same mental model as MinIO's policy engine,
                same mental model as AWS.
              </>
            }
            sourceLabel="src/iam/permissions.rs"
            sourceHref={`${REPO_URL}/blob/main/src/iam/permissions.rs`}
          />
          <FeatureCard
            title="Encrypted SQLCipher config DB"
            body={
              <>
                Users, groups, OAuth providers, and policies live in an encrypted
                SQLCipher database. You bring the passphrase. The DB syncs across
                instances via S3 (encrypted blob, ETag-based) so you can run
                multiple proxies behind a load balancer.
              </>
            }
            sourceLabel="src/config_db_sync.rs"
            sourceHref={`${REPO_URL}/blob/main/src/config_db_sync.rs`}
          />
          <FeatureCard
            title="OAuth / OIDC with group mapping"
            body={
              <>
                Plug in Google, GitHub, your corporate OIDC provider. Map
                provider claims onto DeltaGlider Proxy groups. Admin GUI walks
                you through the mapping rules — no YAML editing required.
              </>
            }
          />
          <FeatureCard
            title="Single binary, S3 on the wire"
            body={
              <>
                Drop in alongside your existing infrastructure. No client
                changes, no SDK swaps, no rewrites. The proxy speaks SigV4 and
                serves a path-style or virtual-host-style S3 endpoint.
              </>
            }
          />
        </div>
      </Section>
      <Section
        eyebrow="On the way"
        title="What's next on the migration roadmap."
      >
        <RoadmapRibbon
          title="Per-bucket quotas"
          body="Set a hard byte limit per bucket; the proxy refuses PUTs once the budget is hit. The underlying usage scanner is already in production for analytics; the quota check itself is small. Designed; not yet shipped."
          href={`${REPO_URL}/blob/main/future/QUOTA.md`}
          hrefLabel="future/QUOTA.md"
        />
      </Section>
      <Section
        eyebrow="Next step"
        title="Send us your IAM export and we'll tell you what's portable."
        intro="If you've got a MinIO IAM export and want a sanity check on what'll port cleanly versus what needs a rewrite, send it over."
      >
        <MailtoCTA subject={SUBJECT} label="Email us" />
      </Section>
    </>
  );
}
