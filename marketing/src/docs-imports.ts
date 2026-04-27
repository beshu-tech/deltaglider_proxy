import README from '../../docs/product/README.md?raw';
import QUICKSTART from '../../docs/product/01-quickstart.md?raw';
import FIRST_BUCKET from '../../docs/product/10-first-bucket.md?raw';
import PROD_DEPLOY from '../../docs/product/20-production-deployment.md?raw';
import PROD_SECURITY from '../../docs/product/20-production-security-checklist.md?raw';
import UPGRADE_GUIDE from '../../docs/product/21-upgrade-guide.md?raw';
import OAUTH_SETUP from '../../docs/product/auth/30-oauth-setup.md?raw';
import SIGV4_IAM from '../../docs/product/auth/31-sigv4-and-iam.md?raw';
import IAM_CONDITIONS from '../../docs/product/auth/32-iam-conditions.md?raw';
import RATE_LIMITING from '../../docs/product/auth/33-rate-limiting.md?raw';
import MONITORING from '../../docs/product/40-monitoring-and-alerts.md?raw';
import TROUBLESHOOTING from '../../docs/product/41-troubleshooting.md?raw';
import FAQ from '../../docs/product/42-faq.md?raw';
import REF_CONFIGURATION from '../../docs/product/reference/configuration.md?raw';
import REF_ADMIN_API from '../../docs/product/reference/admin-api.md?raw';
import REF_AUTHENTICATION from '../../docs/product/reference/authentication.md?raw';
import REF_METRICS from '../../docs/product/reference/metrics.md?raw';
import REF_DELTA from '../../docs/product/reference/how-delta-works.md?raw';
import REF_ENCRYPTION from '../../docs/product/reference/encryption-at-rest.md?raw';
import REF_DECLARATIVE_IAM from '../../docs/product/reference/declarative-iam.md?raw';
import REF_REPLICATION from '../../docs/product/reference/replication.md?raw';

export type DocGroup =
  | 'Start here'
  | 'Deploy to production'
  | 'Authentication & access'
  | 'Day 2 operations'
  | 'Reference';

export const DOC_GROUPS: readonly DocGroup[] = [
  'Start here',
  'Deploy to production',
  'Authentication & access',
  'Day 2 operations',
  'Reference',
] as const;

export const GROUP_TAGLINE: Record<DocGroup, string> = {
  'Start here': 'Install, first bucket, first upload.',
  'Deploy to production': 'Hardening, TLS, backups, upgrades.',
  'Authentication & access': 'OAuth, SigV4, IAM, rate limiting.',
  'Day 2 operations': 'Monitoring, troubleshooting, FAQ.',
  'Reference': 'Config fields, admin API, metrics, internals.',
};

export interface DocEntry {
  id: string;
  title: string;
  filename: string;
  content: string;
  group: DocGroup;
  order: number;
}

interface ProductDoc {
  path: string;
  content: string;
  group: DocGroup;
  order: number;
}

const PRODUCT_DOCS: ProductDoc[] = [
  { path: 'README', content: README, group: 'Start here', order: 0 },
  { path: '01-quickstart', content: QUICKSTART, group: 'Start here', order: 10 },
  { path: '10-first-bucket', content: FIRST_BUCKET, group: 'Start here', order: 20 },
  { path: '20-production-deployment', content: PROD_DEPLOY, group: 'Deploy to production', order: 0 },
  { path: '20-production-security-checklist', content: PROD_SECURITY, group: 'Deploy to production', order: 10 },
  { path: '21-upgrade-guide', content: UPGRADE_GUIDE, group: 'Deploy to production', order: 20 },
  { path: 'auth/30-oauth-setup', content: OAUTH_SETUP, group: 'Authentication & access', order: 0 },
  { path: 'auth/31-sigv4-and-iam', content: SIGV4_IAM, group: 'Authentication & access', order: 10 },
  { path: 'auth/32-iam-conditions', content: IAM_CONDITIONS, group: 'Authentication & access', order: 20 },
  { path: 'auth/33-rate-limiting', content: RATE_LIMITING, group: 'Authentication & access', order: 30 },
  { path: '40-monitoring-and-alerts', content: MONITORING, group: 'Day 2 operations', order: 0 },
  { path: '41-troubleshooting', content: TROUBLESHOOTING, group: 'Day 2 operations', order: 10 },
  { path: '42-faq', content: FAQ, group: 'Day 2 operations', order: 20 },
  { path: 'reference/configuration', content: REF_CONFIGURATION, group: 'Reference', order: 0 },
  { path: 'reference/admin-api', content: REF_ADMIN_API, group: 'Reference', order: 10 },
  { path: 'reference/authentication', content: REF_AUTHENTICATION, group: 'Reference', order: 20 },
  { path: 'reference/metrics', content: REF_METRICS, group: 'Reference', order: 30 },
  { path: 'reference/how-delta-works', content: REF_DELTA, group: 'Reference', order: 40 },
  { path: 'reference/encryption-at-rest', content: REF_ENCRYPTION, group: 'Reference', order: 50 },
  { path: 'reference/declarative-iam', content: REF_DECLARATIVE_IAM, group: 'Reference', order: 60 },
  { path: 'reference/replication', content: REF_REPLICATION, group: 'Reference', order: 70 },
];

function extractTitle(content: string): string {
  for (const line of content.split('\n')) {
    const match = line.match(/^#\s+(.+)/);
    if (match?.[1]) return match[1].trim();
  }
  return 'Untitled';
}

function pathToId(path: string): string {
  return path.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
}

export const DOCS: DocEntry[] = PRODUCT_DOCS.map((doc) => ({
  id: pathToId(doc.path),
  title: extractTitle(doc.content),
  filename: `${doc.path}.md`,
  content: doc.content,
  group: doc.group,
  order: doc.order,
}));

export function findDocByFilename(filename: string): DocEntry | undefined {
  let target = filename.trim().split('#')[0] ?? '';
  target = target.split('?')[0] ?? '';
  while (target.startsWith('../')) target = target.slice(3);
  while (target.startsWith('./')) target = target.slice(2);

  const exact = DOCS.find((doc) => doc.filename === target);
  if (exact) return exact;

  const bare = target.replace(/^.*\//, '');
  return DOCS.find((doc) => doc.filename.replace(/^.*\//, '') === bare);
}
