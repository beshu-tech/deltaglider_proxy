export const SITE_URL = 'https://deltaglider.com';

export const PAGES = [
  { path: '/', file: 'index.html', label: '/', canary: 'Cut object-storage growth' },
  { path: '/regulated/', file: 'regulated/index.html', label: '/regulated/', canary: 'Regulated workloads' },
  { path: '/artifact-storage/', file: 'artifact-storage/index.html', label: '/artifact-storage/', canary: 'Not S3 object versioning' },
  { path: '/minio-migration/', file: 'minio-migration/index.html', label: '/minio-migration/', canary: 'Self-hosted S3 without losing' },
  { path: '/s3-to-hetzner-wasabi/', file: 's3-to-hetzner-wasabi/index.html', label: '/s3-to-hetzner-wasabi/', canary: 'Move Amazon S3 data to Hetzner or Wasabi' },
  { path: '/multi-cloud-control-plane/', file: 'multi-cloud-control-plane/index.html', label: '/multi-cloud-control-plane/', canary: 'One S3 security layer' },
  { path: '/docs/', file: 'docs/index.html', label: '/docs/', canary: 'DeltaGlider Proxy' },
  { path: '/about/', file: 'about/index.html', label: '/about/', canary: 'DeltaGlider Proxy is built by Beshu Tech' },
  { path: '/privacy/', file: 'privacy/index.html', label: '/privacy/', canary: 'Privacy Policy' },
  { path: '/terms/', file: 'terms/index.html', label: '/terms/', canary: 'Terms of Service' },
];
