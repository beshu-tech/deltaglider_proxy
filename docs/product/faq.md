# FAQ

*The questions that come up often, each linked to the page that answers it properly.*

## Deployment

- [Can I put it behind an AWS ALB?](how-to/go-to-production.md)
- [Does it need two ports, one for S3 and one for the UI?](explanation/multi-backend-architecture.md)
- [Does it work on Fly / Railway / Coolify / bare metal?](how-to/go-to-production.md)
- [Does it need S3, or can it use local disk?](how-to/route-a-bucket-to-a-backend.md)
- [Does it work with Backblaze B2 / Hetzner / Wasabi / R2 / MinIO?](how-to/route-a-bucket-to-a-backend.md)
- [Does it replace S3 or proxy to it?](explanation/multi-backend-architecture.md)

## Compression

- [Can I turn off compression for a specific bucket?](how-to/set-bucket-compression-and-quotas.md)
- [Can I enable compression only for specific prefixes inside a bucket?](how-to/set-bucket-compression-and-quotas.md)
- [What file types actually benefit from delta compression?](explanation/delta-compression.md)
- [Why doesn't my .tar.gz compress — and can compressed archives delta well at all?](explanation/delta-compression.md)
- [Does compression slow things down?](explanation/delta-compression.md)

## Authentication

- [Do OAuth and IAM work together?](explanation/security-model.md)
- [Can I disable auth entirely for dev?](reference/authentication.md)
- [What if I lose the bootstrap password?](how-to/troubleshooting.md)

## Encryption

- [Which backend encryption mode should I pick?](explanation/encryption-at-rest.md)
- [Does enabling encryption re-encrypt my existing objects?](how-to/encrypt-data-at-rest.md)
- [Can I use cheap or untrusted S3-compatible storage safely?](explanation/encryption-at-rest.md)
- [Can I use different keys for different buckets?](reference/encryption.md)
- [What happens if I lose my proxy-AES key?](explanation/encryption-at-rest.md)
- [How do I rotate a proxy-AES key?](how-to/rotate-encryption-keys.md)
- [Does compression still work with encryption enabled?](explanation/encryption-at-rest.md)
- [Does encryption apply to object metadata too?](explanation/encryption-at-rest.md)
- [Does encryption add latency?](explanation/encryption-at-rest.md)
- [I see `dg-encrypted-native: sse-kms` in my object metadata — is that leaking something?](reference/encryption.md)
- [Can I write objects with the Python DeltaGlider CLI while the backend is encrypted?](explanation/encryption-at-rest.md)

## Backup

- [What does "Full Backup" include?](how-to/back-up-and-restore.md)
- [Is Full Backup the same as config sync (`DGP_CONFIG_SYNC_BUCKET`)?](how-to/back-up-and-restore.md)
- [Can I manage IAM entirely in YAML (GitOps)?](how-to/manage-iam-as-code.md)

## Limits

- [What's the max object size?](reference/configuration.md)
- [What's the max concurrent users?](reference/rate-limits.md)
- [How long can presigned URLs live?](reference/authentication.md)
