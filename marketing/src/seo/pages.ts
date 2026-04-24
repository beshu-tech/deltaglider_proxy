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

export const landingMeta: PageMeta = {
  path: '/',
  title: 'DeltaGlider Proxy — S3-compatible storage up to 10× cheaper',
  description:
    'An S3-compatible proxy that makes cloud storage up to 10× cheaper through transparent delta compression. Drop-in SigV4, ABAC IAM, AES-256-GCM encryption at rest. Open source.',
  ogImage: OG_LANDING,
  jsonLd: [organization(), website(), softwareApplication()],
};

export const regulatedMeta: PageMeta = {
  path: '/regulated/',
  title: 'Regulated workloads — encryption at rest, keys you own',
  description:
    'DeltaGlider Proxy for compliance-first teams: AES-256-GCM encryption at rest with a key that never leaves your environment, ABAC IAM, encrypted multi-instance config sync.',
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
          'The encryption key is supplied at process start via the DGP_ENCRYPTION_KEY environment variable. The key is held in memory only; it is never written to the storage backend, never sent to AWS KMS, and is zeroized on shutdown.',
      },
      {
        question: 'What encryption algorithm is used at rest?',
        answer:
          'AES-256-GCM with a per-object 12-byte random IV and 16-byte authentication tag. Encryption is implemented in src/storage/encrypting.rs.',
      },
      {
        question: 'Can DeltaGlider Proxy replicate data across clouds?',
        answer:
          'Eventually-consistent cross-backend replication is designed and tracked in future/REPLICATION.md. It is not yet shipped; encryption at rest and multi-instance config sync are shipped today.',
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
  path: '/versioning/',
  title: 'Artifact versioning — up to 95% less S3 spend on binaries',
  description:
    'DeltaGlider Proxy for CI/CD teams: transparent xdelta3 compression on versioned artifacts (JAR, tar, zip, ISO, SQL dumps). Drop-in S3 API, live savings analytics, no client changes.',
  ogImage: OG_VERSIONING,
  jsonLd: [
    organization(),
    breadcrumb([
      { name: 'Home', path: '/' },
      { name: 'Artifact versioning', path: '/versioning/' },
    ]),
    faqPage([
      {
        question: 'Which file types benefit from delta compression?',
        answer:
          'Versioned archives and binaries whose successive versions share most of their bytes: .zip, .tar, .tgz, .tar.gz, .tar.bz2, .jar, .war, .ear, .sql, .dump, .bak, .backup, .rar, .7z, .dmg, .iso.',
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
  title: 'MinIO migration — drop-in S3 proxy with ABAC IAM',
  description:
    'For teams leaving MinIO: a single-binary S3-compatible proxy with ABAC IAM, OAuth/OIDC group mapping, encrypted config database. No license drama, no rewrites.',
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
          'For the S3 protocol and IAM shape, yes. DeltaGlider Proxy speaks SigV4, supports per-user access keys, groups, and ABAC policies in AWS IAM grammar. Existing clients do not need changes.',
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
    ]),
  ],
};

export const allPages: readonly PageMeta[] = [
  landingMeta,
  regulatedMeta,
  versioningMeta,
  minioMigrationMeta,
];
