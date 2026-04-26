import {
  type BreadcrumbListSchema,
  type FaqPageSchema,
  type OrganizationSchema,
  type SoftwareApplicationSchema,
  type WebSiteSchema,
  SITE_URL,
  breadcrumb,
  faqPage,
  organization,
  softwareApplication,
  website,
} from './schema';

export type JsonLdPayload =
  | OrganizationSchema
  | WebSiteSchema
  | SoftwareApplicationSchema
  | FaqPageSchema
  | BreadcrumbListSchema;

export interface PageMeta {
  path: string;
  title: string;
  description: string;
  ogImage: string;
  jsonLd: JsonLdPayload[];
}

const OG_LANDING = `${SITE_URL}/screenshots/filebrowser.jpg`;
const OG_REGULATED = `${SITE_URL}/screenshots/advanced_security.jpg`;
const OG_VERSIONING = `${SITE_URL}/screenshots/analytics.jpg`;
const OG_MINIO = `${SITE_URL}/screenshots/iam.jpg`;
const OG_S3_SAAS = `${SITE_URL}/screenshots/bucket-policies.jpg`;

export const landingMeta: PageMeta = {
  path: '/',
  title: 'DeltaGlider Proxy — smaller S3-compatible object storage',
  description:
    'An S3-compatible proxy that stores repeated binaries as compact deltas. Includes IAM, OAuth, soft quotas, replication, encryption, metrics, and audit controls.',
  ogImage: OG_LANDING,
  jsonLd: [organization(), website(), softwareApplication()],
};

export const regulatedMeta: PageMeta = {
  path: '/regulated/',
  title: 'Regulated workloads — S3-compatible storage with local control',
  description:
    'DeltaGlider Proxy lets regulated teams use cheaper or untrusted S3-compatible storage by encrypting before the backend while keys stay in the trusted environment.',
  ogImage: OG_REGULATED,
  jsonLd: [
    organization(),
    breadcrumb([
      { name: 'Home', path: '/' },
      { name: 'Regulated workloads', path: '/regulated/' },
    ]),
    faqPage([
      {
        question: 'Where does the encryption key live?',
        answer:
          'The encryption key is supplied at process start via the DGP_ENCRYPTION_KEY environment variable. The key is held in the trusted runtime only; it is never written to the storage backend, never sent to AWS KMS, and is zeroized on shutdown.',
      },
      {
        question: 'What encryption algorithm is used at rest?',
        answer:
          'AES-256-GCM with a per-object 12-byte random IV and 16-byte authentication tag. Encryption is implemented in src/storage/encrypting.rs.',
      },
      {
        question: 'Can DeltaGlider Proxy replicate data across backends?',
        answer:
          'Yes. Object replication rules copy objects between configured buckets or prefixes, keep runtime state and failures in the encrypted config database, and can optionally replicate deletes for objects previously written by the rule.',
      },
      {
        question: 'How do you enforce per-user access?',
        answer:
          'Attribute-Based Access Control (ABAC) policies written in the same policy grammar as AWS IAM (parsed by iam-rs). Conditions cover IP-range restrictions and prefix-scoped access. Users and groups are stored in an encrypted SQLCipher configuration database.',
      },
    ]),
  ],
};

export const versioningMeta: PageMeta = {
  path: '/artifact-storage/',
  title: 'Artifact storage — reduce storage for binary-similar versions',
  description:
    'DeltaGlider Proxy stores binary-similar backups, software catalogs, media assets, and AI model variants as compact deltas behind an S3-compatible API.',
  ogImage: OG_VERSIONING,
  jsonLd: [
    organization(),
    breadcrumb([
      { name: 'Home', path: '/' },
      { name: 'Artifact storage', path: '/artifact-storage/' },
    ]),
    faqPage([
      {
        question: 'Is this S3 object versioning?',
        answer:
          'No. DeltaGlider Proxy does not expose S3 version IDs or restore old object versions today. This page is about reducing storage for repeated artifact releases with transparent delta compression.',
      },
      {
        question: 'Which file types benefit from delta compression?',
        answer:
          'The default delta candidates include archives and binary dumps such as .zip, .tar, .jar, .sql, .dump, .dmg, and .iso. The list is configurable. Actual benefit depends on internal file structure and binary similarity across versions, which is common in backup archives, software catalogs, media asset variants, and AI model variants.',
      },
      {
        question: 'What about images, video, or already-compressed content?',
        answer:
          'Formats that are already compressed (PNG, JPG, MP4, PDF, EXE) are passed through unchanged. No CPU is wasted trying to compress noise.',
      },
      {
        question: 'Do clients need changes to benefit?',
        answer:
          'No. DeltaGlider Proxy speaks the S3 API on the wire, including SigV4. Your existing boto3, aws-sdk-java, or rclone workflows keep working — compression is transparent.',
      },
      {
        question: 'How do I see the actual savings?',
        answer:
          'The built-in analytics dashboard displays per-bucket compression ratios and bytes saved. Prometheus metrics (deltaglider_delta_compression_ratio, delta_bytes_saved_total) are also exposed at /metrics.',
      },
    ]),
  ],
};

export const minioMigrationMeta: PageMeta = {
  path: '/minio-migration/',
  title: 'MinIO migration — self-hosted S3 with an enterprise control plane',
  description:
    'For MinIO refugees: self-hosted S3-compatible storage with the enterprise control plane younger OSS object stores often lack: IAM, OAuth, quotas, policy, replication, and operator UI.',
  ogImage: OG_MINIO,
  jsonLd: [
    organization(),
    breadcrumb([
      { name: 'Home', path: '/' },
      { name: 'MinIO migration', path: '/minio-migration/' },
    ]),
    faqPage([
      {
        question: 'Is DeltaGlider Proxy a drop-in replacement for MinIO?',
        answer:
          'For common S3 workflows and the IAM/control-plane shape, yes. DeltaGlider Proxy speaks SigV4 and supports per-user access keys, groups, ABAC policies, OAuth/OIDC mapping, bucket policy, quotas, and replication controls.',
      },
      {
        question: 'What ABAC features are supported?',
        answer:
          'Per-user S3 credentials, groups, permission actions (read, write, delete, list, admin), resource globs on bucket + prefix, and conditions including IP ranges and prefix restrictions.',
      },
      {
        question: 'How is IAM state stored?',
        answer:
          'In an encrypted SQLCipher database using a passphrase that you supply. The database can be synced across multiple proxy instances via S3 (encrypted blob, ETag-based polling).',
      },
      {
        question: 'Does it support OIDC / OAuth logins?',
        answer:
          'Yes. OAuth and OIDC providers are configurable via the admin GUI, with group-mapping rules that translate provider claims into DeltaGlider Proxy groups.',
      },
      {
        question: 'Are per-bucket quotas implemented?',
        answer:
          'Yes. Bucket policies support quota_bytes as a soft write limit backed by the usage scanner. Setting quota_bytes to 0 freezes a bucket for read-only migration windows.',
      },
    ]),
  ],
};

export const s3SaasMeta: PageMeta = {
  path: '/s3-saas-control-plane/',
  title: 'Cheaper S3 SaaS — enterprise control plane for lower-cost storage',
  description:
    'Use lower-cost S3-compatible storage without giving up enterprise controls. DeltaGlider adds local-key encryption, IAM, OAuth, bucket policy, quotas, replication, audit, and operator UI.',
  ogImage: OG_S3_SAAS,
  jsonLd: [
    organization(),
    breadcrumb([
      { name: 'Home', path: '/' },
      { name: 'Cheaper S3 SaaS', path: '/s3-saas-control-plane/' },
    ]),
    faqPage([
      {
        question: 'Why not just use a cheaper S3-compatible provider directly?',
        answer:
          'Many lower-cost S3-compatible providers are good at basic object storage but do not provide the enterprise control plane teams expect from AWS S3. DeltaGlider adds local-key encryption, IAM, OAuth/OIDC mapping, bucket policy, soft quotas, replication, metrics, audit, and operator UI in front of the backend.',
      },
      {
        question: 'Does DeltaGlider replace the storage backend?',
        answer:
          'No. DeltaGlider is a proxy and control-plane layer. Applications speak S3 to DeltaGlider, and DeltaGlider stores data in the backend you choose.',
      },
    ]),
  ],
};

export const aboutMeta: PageMeta = {
  path: '/about/',
  title: 'About DeltaGlider Proxy — built by Beshu Tech',
  description:
    'DeltaGlider Proxy is an open-source S3-compatible storage proxy from Beshu Tech, with commercial support and services available separately.',
  ogImage: OG_LANDING,
  jsonLd: [
    organization(),
    breadcrumb([
      { name: 'Home', path: '/' },
      { name: 'About', path: '/about/' },
    ]),
  ],
};

export const privacyMeta: PageMeta = {
  path: '/privacy/',
  title: 'Privacy Policy — DeltaGlider Proxy',
  description:
    'Privacy policy for the DeltaGlider Proxy marketing site and business contact channels.',
  ogImage: OG_LANDING,
  jsonLd: [
    organization(),
    breadcrumb([
      { name: 'Home', path: '/' },
      { name: 'Privacy Policy', path: '/privacy/' },
    ]),
  ],
};

export const termsMeta: PageMeta = {
  path: '/terms/',
  title: 'Terms of Service — DeltaGlider Proxy',
  description:
    'Terms for use of the DeltaGlider Proxy marketing site. Software use is governed by the repository license unless a separate agreement applies.',
  ogImage: OG_LANDING,
  jsonLd: [
    organization(),
    breadcrumb([
      { name: 'Home', path: '/' },
      { name: 'Terms of Service', path: '/terms/' },
    ]),
  ],
};

export const allPages: readonly PageMeta[] = [
  landingMeta,
  regulatedMeta,
  versioningMeta,
  minioMigrationMeta,
  s3SaasMeta,
  aboutMeta,
  privacyMeta,
  termsMeta,
];
