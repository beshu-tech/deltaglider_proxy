# FAQ

*The short-version answers to questions that come up often.*

## Deployment

### Can I put it behind an AWS ALB?

Yes. The ALB is a valid reverse proxy. Set `DGP_TRUST_PROXY_HEADERS=true` so the proxy reads `X-Forwarded-For` from the ALB (otherwise rate limits and IAM IP conditions see the ALB's IP for every request). Target group health check: `GET /_/health`, HTTP 200.

### Does it need two ports, one for S3 and one for the UI?

No. Single port (default 9000). The S3 API lives at `/`, the admin UI + admin APIs live under `/_/`. The `_` character is invalid in S3 bucket names, so there's no conflict. This is deliberate — no sidecars, no separate health checks, no CORS headaches.

### Does it work on Fly / Railway / Coolify / bare metal?

Anywhere a Rust binary runs. The embedded UI is baked into the binary at build time (rust-embed), so there's nothing to serve separately.

For stateful deployment (you want your admin config + IAM DB to survive a redeploy), mount a persistent volume at the container's CWD (`/data` in the Docker image). `deltaglider_proxy.yaml`, `deltaglider_config.db`, and the filesystem backend (if used) all live there.

### Does it need S3? Can it use local disk?

Local disk is fully supported as a backend — set `storage.backend.type: filesystem` and `path: /var/lib/deltaglider_proxy/data`. Useful for self-hosted setups and tests. For horizontal scaling or high durability, use S3 as the backend — the proxy itself remains stateless, and the encrypted IAM DB can be synced across instances via `DGP_CONFIG_SYNC_BUCKET`.

### Does it work with Backblaze B2 / Hetzner / Wasabi / R2 / MinIO?

Yes. Anything with an S3-compatible API works. Set `endpoint` to the provider's URL, `region` to whatever they call their region, and `force_path_style: true` for most non-AWS providers (AWS wants `false`). The proxy uses the AWS SDK; if the provider is S3-compatible, it Just Works.

### Does it replace S3 or proxy to S3?

Proxy. The proxy never terminates your data; it routes to a backend (S3, filesystem, whatever). Delta compression is an optimisation applied on top of the backend's own storage — the backend just sees smaller objects.

## Compression

### Can I turn off compression for a specific bucket?

Yes. Set `compression: false` on the per-bucket policy:

```yaml
storage:
  buckets:
    my-images-bucket:
      compression: false
```

Everything in that bucket is stored passthrough. Useful for buckets that hold already-compressed content (JPEGs, video, gzipped archives) where xdelta3 won't help anyway.

### Can I enable compression only for specific prefixes inside a bucket?

Not today. Compression is **per-bucket**. If you want some prefixes to skip delta, either (a) split into two buckets with different `compression` settings, or (b) rely on `max_delta_ratio` — non-compressible files fall back to passthrough automatically when the delta isn't worth keeping. Per-prefix compression policy is on the roadmap.

### What file types actually benefit from delta compression?

Versioned binaries where most of the content doesn't change between versions: zipped releases, database dumps, game-build artefacts, tar archives. DeltaGlider has been seeing 60–95% savings on these in practice.

What doesn't benefit: already-compressed formats (JPEG, MP4, Zstd, gzip), unique-per-upload binaries (photos, user uploads). For these, the proxy auto-falls-back to passthrough via `max_delta_ratio` — no manual intervention needed.

### Does compression slow things down?

**PUT**: yes, slightly. xdelta3 encode takes CPU. Typical overhead is 10–50ms per uploaded object. Tune `DGP_CODEC_CONCURRENCY` if you have lots of concurrent PUTs.

**GET**: only on the first read from a cold reference. The reference is then in the LRU cache (`DGP_CACHE_MB`, default 100 MB — bump to 1024+ in production). Subsequent reads from the same deltaspace are fast.

Every GET is SHA-256-verified before returning to the client — not free, but cheap (hash is computed during the reconstruction anyway).

## Authentication

### Do OAuth and IAM work together?

Yes. OAuth handles admin UI login; IAM handles SigV4 on the S3 API. They're orthogonal. An OAuth-authenticated admin can also have IAM credentials to use the S3 API directly. Mapping rules auto-assign IAM groups to new OAuth identities, so "sign in with Google" can land a user with `read,list` on a specific bucket without operator intervention.

### Can I disable auth entirely for dev?

Yes. Don't set `DGP_ACCESS_KEY_ID`/`DGP_SECRET_ACCESS_KEY` or set `authentication: none` in YAML. The proxy accepts all requests. **Development only** — keep it off the internet. The admin UI still requires the bootstrap password.

### What if I lose the bootstrap password?

Use `POST /_/api/admin/recover-db` (public, rate-limited) with your correct password to reset the DB key. If you've also lost the password: you're stuck — the encrypted IAM DB can't be decrypted without it. Restore from a Full Backup zip (which carries `bootstrap_password_hash` in `secrets.json`) or re-bootstrap the instance from scratch.

**Prevention:** always take a Full Backup after any password change. The zip carries both the hash and the encrypted DB, so a fresh instance can be reconstituted.

## Encryption at rest

### Which backend encryption mode should I pick?

Short decision tree:

- **Filesystem backend** → `aes256-gcm-proxy` (or `none` for non-sensitive data). Native SSE modes are S3-only.
- **S3 with AWS KMS available + you want per-key audit logs** → `sse-kms`. Each decrypt hits CloudTrail.
- **S3 without KMS** → `sse-s3` if AWS-managed AES256 is enough, `aes256-gcm-proxy` if you don't trust the provider.
- **Public-CDN buckets** → `none`. Encryption is pure overhead here.

See [reference/encryption-at-rest.md](reference/encryption-at-rest.md) for the full matrix including the threat model per mode.

### Does enabling encryption re-encrypt my existing objects?

No. The marker-based dispatch means old plaintext objects continue to be served as plaintext; only new writes go through the encrypt path. If you want to encrypt historical objects, copy them through the proxy to themselves (that's a rewrite, which routes through the new encrypt path).

### Can I use different keys for different buckets?

Not directly — encryption is **backend-scoped**, and buckets inherit their backend's encryption. To get per-bucket isolation, create additional backends with different keys and route buckets to them via `storage.buckets[name].backend`.

### What happens if I lose my proxy-AES key?

The encrypted objects on that backend are unrecoverable. There is no recovery path. DeltaGlider does not escrow keys; the admin GUI shows a red banner on every key-touching action saying exactly this.

This is why you should **store the key off-box** (secrets manager, password vault, sealed envelope) before enabling encryption, and why the GUI gates the Apply button behind a "I have stored this key safely" checkbox.

### How do I rotate a proxy-AES key?

Not automatic. Two recipes, documented in [reference/encryption-at-rest.md §Rotation recipes](reference/encryption-at-rest.md#rotation-recipes):

- **Shim-assisted (minimum downtime)** — keep the old key as `legacy_key`, put the new key in `key`. Reads try the new key first and fall back to the shim; new writes use the new key.
- **Data migration (no runtime shim)** — create a new backend with the new key, copy through the proxy, retire the old backend.

### Does compression still work with encryption enabled?

Yes. xdelta3 delta compression runs BEFORE encryption. The delta bytes are then encrypted and written to disk. Compression ratios are preserved exactly — the ciphertext is just the encrypted wrapping of what the delta codec already produced.

### Does encryption apply to object metadata too?

No. Under any mode — including SSE-KMS — object names, sizes, content-type, and `x-amz-meta-*` user metadata are plaintext. This is a combination of AWS constraint (SSE only encrypts the body) and DG constraint (the `dg-encrypted` marker that tells us HOW to decrypt lives in user metadata — it can't itself be encrypted).

If metadata confidentiality matters for your threat model, put the secret in the object body, not in metadata.

### Does encryption add latency?

- **`aes256-gcm-proxy`**: yes, small. ~30-100ms on a 100 MiB upload on modern AES-NI hardware. Reads are streaming-decrypted with similar per-byte cost.
- **`sse-kms`**: AWS handles the crypto; latency impact is dominated by KMS API calls (one per write unless `bucket_key_enabled: true`).
- **`sse-s3`**: negligible. AWS handles AES256 inline.
- **`none`**: none.

### I see `dg-encrypted-native: sse-kms` in my object metadata — is that leaking something?

No. It's a machine marker telling the proxy "this was encrypted by AWS, don't try to decrypt it yourself." It reveals nothing sensitive — the fact that a backend uses SSE-KMS is already visible from the backend config and from any AWS-side listing. Non-secret.

### Does it work if I write objects with the Python DeltaGlider CLI while the backend is encrypted?

No. The Python CLI writes plaintext to the underlying S3 bucket and bypasses the proxy entirely. For an encrypted backend, standardize on the proxy as the write path — point the CLI at the proxy's endpoint, not at raw S3.

## Backup + multi-instance

### What does "Full Backup" include?

Four files, all sha256-verified in the exported zip:

- `manifest.json` — version + timestamps + checksums
- `config.yaml` — canonical YAML, secrets redacted (`null`)
- `iam.json` — users, groups, OAuth providers, mapping rules, external identities
- `secrets.json` — plaintext bootstrap hash, OAuth client_secrets, storage creds (**treat the zip as a keystore**)

Imports via `POST /_/api/admin/backup` with `Content-Type: application/zip`. Atomic — all four parts are unpacked and validated before any state change.

### Is Full Backup the same as `DGP_CONFIG_SYNC_BUCKET`?

No. They're orthogonal.

- **Full Backup** — operator-initiated, point-in-time snapshot. Take one before any upgrade.
- **Config sync** — automatic live replication of the encrypted IAM DB across instances. Good for horizontal scaling. A bad mutation on one instance propagates to all readers — not a backup.

You want both.

### Can I manage IAM entirely in YAML (GitOps)?

Yes, with `access.iam_mode: declarative`. YAML becomes authoritative; admin-API IAM mutations return `403 { "error": "iam_declarative" }`. The expected flow: edit YAML, `POST /_/api/admin/config/apply`, reconcile DB ↔ YAML happens automatically.

Caveat: the reconciler is Phase 3c.3 and currently not implemented. Today, `declarative` mode is a pure lockout — the admin API refuses IAM writes, but there's no automatic sync of YAML → DB. Usable if you seed the DB once via the GUI and then flip to declarative for subsequent GitOps-only changes.

## Limits

### What's the max object size?

`DGP_MAX_OBJECT_SIZE`, default 100 MB. Raise it if your workload has bigger artefacts. This cap applies to the reconstruction memory budget (delta GETs buffer the full reference + delta), so account for headroom on the host.

Passthrough objects are streamed, so their size isn't bounded by this memory budget — but the upload endpoint still honours the same cap. Multipart uploads bypass this (unlimited per part, standard S3 multipart semantics).

### What's the max concurrent users?

No hard limit. In practice, the proxy scales linearly on CPU for PUT (delta encoding) and RAM for GET (reference cache). A single Hetzner AX101 has been tested at > 2k RPS for GET-heavy workloads.

The admin UI session limit is 10 concurrent sessions per user (oldest-evicted). Bump with a source-level change if you need more.

### How long can presigned URLs live?

Max 7 days (604,800 seconds), matching AWS S3's own limit. Shorter expiries are fine. Presigned URLs carry the signing user's IAM permissions.

## Meta

### Why Rust?

Memory safety, single-binary deploy, good async story (Tokio + Axum), mature AWS SDK support. The delta compression is xdelta3 via subprocess — written in C, proven over decades, keeps us from linking C into the binary.

### Is the source available?

Yes. GitHub: [beshu-tech/deltaglider_proxy](https://github.com/beshu-tech/deltaglider_proxy). Also see the developer-facing docs on the GitHub repo (not in this bundle): build-from-source, release process, CI infrastructure.

### Where are the bugs reported?

GitHub issues on the repo. Security issues: see SECURITY.md on the repo.
