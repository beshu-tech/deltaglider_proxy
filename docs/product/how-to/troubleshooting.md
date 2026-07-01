# Troubleshooting

This guide maps the symptoms you'll see in the wild to their fixes. If your symptom isn't here, the audit log at `/_/admin/diagnostics/audit` and the structured logs (`tracing`) are almost always where the real error lives — see [How to trace and audit requests](trace-requests.md).

## Client gets 403 AccessDenied

**Check the audit log first.** `/_/admin/diagnostics/audit` shows every IAM denial with the user, action, bucket, and path. The most common causes:

1. **Wrong prefix.** The user has `Allow read on releases/public/*` but tried `releases/private/foo.zip`. Prefix ABAC is exact — a trailing `/` matters.
2. **User disabled.** Access → Users → confirm the row is `Enabled`.
3. **Deny rule wins.** Any matching Deny rule in the user's permissions (or any group they're in) wins over an Allow. Grep the user's permissions + the groups'.
4. **`iam_mode: declarative`** and you're trying to mutate IAM via the admin API. Expected behaviour — the API returns `403 { "error": "iam_declarative" }`. Edit YAML and apply the document.
5. **Stale `${username}` template.** A permission resource written as `${username}` instead of `${iam:username}` is not substituted, so it matches nothing and the user is denied. Fix the template; the save-time config advisories flag this.

If the audit log is **empty** and you still see 403, the denial is in SigV4 verification, not IAM:

- Check `/_/metrics` for `deltaglider_auth_failures_total{reason="invalid_signature"}`.
- Is the client's system clock within `DGP_CLOCK_SKEW_SECONDS` (default 300s) of the server? Look for `RequestTimeTooSkewed` in the client error.
- Is the access key typo'd? The proxy returns a generic AccessDenied rather than leaking key-existence.

## Intermittent 403s / one client locks out everyone

Symptom: requests succeed most of the time but **intermittently** fail, and the failures cluster — when one client is busy, *other* clients start failing too. This is rate-limit lockout from a **shared bucket**, not an auth problem.

Cause: the proxy is behind a reverse proxy (Coolify, Traefik, nginx, ALB) but `DGP_TRUST_PROXY_HEADERS` is `false`, so every request looks like it comes from the proxy's IP. All clients share one rate-limit bucket; one busy client exhausts it and the rest get throttled.

Tells:
- Throttling returns `503 SlowDown`, but a client retry that re-sends auth can surface as `403`; the auth/lockout log lines now include `bucket_key=` and `trust_proxy=` — if `bucket_key` is your proxy's IP for unrelated clients, that's the smoking gun.
- The save-time config advisories flag the rate-limit-on + trust-off combination.

Fix: set `DGP_TRUST_PROXY_HEADERS=true` (only behind a trusted proxy that injects `X-Forwarded-For`/`X-Real-IP`). See [Rate limits](../reference/rate-limits.md#ip-extraction).

## Admin login fails with the right password

The bootstrap password verification uses bcrypt. Usually one of:

1. **Stale `DGP_BOOTSTRAP_PASSWORD_HASH`.** If you rotated the plaintext but forgot to update the env, the DB key drifts from the hash. The DB refuses to decrypt on restart — you'll see `Failed to open config DB: invalid passphrase` in startup logs.

   Fix: restart with the correct hash, then rotate via the admin UI once logged in (that path re-encrypts the DB atomically).

2. **Rate limiter lockout.** 100 failed attempts / 5-minute window / per-IP, with a 10-minute lockout after. `/_/metrics` → `deltaglider_auth_failures_total`. Wait it out, or see [Rate limits](../reference/rate-limits.md) for the knobs.

3. **Session IP binding.** If you log in from one IP and the admin cookie ends up used from a different IP (NAT flip, VPN change), the session is rejected. Log in again. Disable `DGP_TRUST_PROXY_HEADERS` if you're not behind a reverse proxy — otherwise clients can spoof IPs.

## Startup fails: `xattr` support missing

**Log line:** `Data directory does not support extended attributes`.

The filesystem backend stores object metadata as `user.dg.metadata` xattrs on the data file's inode. The proxy validates this at startup and refuses to start otherwise.

Filesystems that support xattrs: ext4, XFS, Btrfs, ZFS, APFS.
Filesystems that don't: tmpfs, FAT32, exFAT, NFS-without-acl-over-xattr mount, some overlay2 configurations.

Fix: mount `DGP_DATA_DIR` on a supporting filesystem, or switch to the S3 backend.

## Startup fails: `SQLCipher could not open config DB`

The encryption key doesn't match the DB file. Usually:

1. You restored a Full Backup zip on a fresh instance but didn't feed the corresponding `bootstrap_password_hash` back in. The zip's `secrets.json` carries it — re-import the zip, or inject `DGP_BOOTSTRAP_PASSWORD_HASH` before the restore. See [How to back up and restore](back-up-and-restore.md).
2. You rotated the bootstrap password outside the admin UI (edited env, restarted). The admin UI's `PUT /_/api/admin/password` is the only safe path — it re-encrypts the DB atomically.

Recovery path: `POST /_/api/admin/recover-db` with the correct password. The endpoint is public but rate-limited.

## 502 Bad Gateway / 504 Gateway Timeout on large uploads

**Symptom:** Multipart uploads of files >50 MB fail with `502` (Traefik) or `504` (Caddy / nginx). The embedded UI shows "Upload object … failed (502): Gateway returned 502 Bad Gateway." The proxy logs show `tower_http::trace::on_response: finished processing request latency=60000 ms status=400` (latency is exactly 60 000 ms).

**Cause:** Reverse-proxy default request read-timeout is **60 seconds**. A 16 MB multipart part over a typical home upload link (1–5 MB/s, shared between concurrent parts) takes longer than that. The reverse proxy closes the upstream connection mid-body; hyper raises a body-read error; axum's `Bytes` extractor returns `400 BAD_REQUEST` with body "Failed to buffer the request body"; the reverse proxy translates the broken upstream response into 502 / 504 to the client.

**Fix:** extend the reverse-proxy read-timeout. Per-proxy settings table and examples in [How to serve TLS](serve-tls.md#raise-the-reverse-proxy-read-timeout--mandatory-for-large-uploads).

For Coolify users specifically:

```bash
# On the Coolify host:
sudo $EDITOR /data/coolify/proxy/docker-compose.yml
# Add to the traefik service `command:` block:
#   - '--entrypoints.https.transport.respondingTimeouts.readTimeout=30m'
#   - '--entrypoints.https.transport.respondingTimeouts.writeTimeout=30m'
sudo docker compose -f /data/coolify/proxy/docker-compose.yml up -d
```

**Quick mitigation without operator access:** the embedded uploader since v0.9.18 caps concurrent files at 1 so each part gets fair share of the upload pipe and completes within the 60 s default window for typical workloads. For very slow links or larger parts, the reverse-proxy timeout still has to be raised.

## 503 SlowDown on PUT

Unless a maintenance job is write-gating the bucket (see the next entry), the proxy doesn't generate 503 itself — this comes from the upstream S3 backend when it's throttling you. Two tuning knobs:

1. **`DGP_MAX_MULTIPART_UPLOADS`** (default 1000) — limits concurrent multiparts in flight. Lowering this reduces the proxy's burst pressure on the backend.
2. **`DGP_CODEC_CONCURRENCY`** — limits xdelta3 subprocess permits. When this saturates, PUTs queue on delta encoding; the backend isn't the bottleneck.

Check `/_/metrics` → `deltaglider_codec_semaphore_available` (`0` = saturated) and `deltaglider_delta_encode_duration_seconds` for codec pressure. If codec is saturated, bump `DGP_CODEC_CONCURRENCY`.

## Writes to one bucket return 503 SlowDown

A maintenance job (re-encryption or migration) is running on that bucket. Writes are intentionally gated while the job rewrites objects — SDKs retry automatically and succeed once the job finishes. Reads are unaffected. Check **Settings → Jobs** (or `GET /_/api/admin/jobs`) for the job's progress; cancel it if it shouldn't be running. If a job is stuck, it survives restarts by design — cancel it via `POST /_/api/admin/jobs/maintenance:<id>/cancel` rather than restarting the proxy. See [Jobs reference](../reference/jobs.md).

## Cache miss storm on GET

**Symptom:** sudden latency spike on GETs; `deltaglider_cache_miss_rate_ratio` jumps above 0.5.

Most common cause: a restart with a cold cache against a hot-read workload. Expected for ~5 minutes; the LRU repopulates.

Less common: `DGP_CACHE_MB` is undersized. The startup log warns `[cache] In-memory reference cache is only 100 MB — recommend ≥1024 MB for production`. Bump it.

Very rare: a write burst is pushing fresh references in and evicting the hot set. Check `deltaglider_delta_decisions_total{decision="reference"}` rate. If you're creating many new deltaspaces quickly, consider segregating write-heavy and read-heavy workloads onto separate buckets (different LRU scope) or different instances.

## Public prefix returns 403

```yaml
storage:
  buckets:
    downloads:
      public_prefixes:
        - public/        # note the trailing /
```

Checks in order:

1. **Trailing slash on the prefix.** `public/` matches `public/foo.zip` but **not** `publicish/bar.zip`. Always end prefixes with `/`.
2. **Bucket policy actually applied.** `/_/api/admin/config/section/storage` should show the `public_prefixes` array. If it's empty, the YAML didn't land — re-check `config apply` response.
3. **Reverse proxy stripping the path.** If Traefik / Caddy is rewriting the URL (e.g. `/downloads/public/*` → `/public/*`), the proxy sees a different bucket than the client intends. Point the reverse proxy at the proxy 1:1.

Confirm which layer denies with a synthetic trace — see [How to trace and audit requests](trace-requests.md).

## Object goes to the wrong backend

Per-bucket backend routing lives in `storage.buckets[name].backend`. Quickest debugging:

```bash
# Confirm the per-bucket routing
curl -b cookies "https://s3.acme.example/_/api/admin/config/section/storage?format=yaml"
```

If the routing looks right but the object still went somewhere unexpected:

- Did the PUT hit the proxy or the backend directly? Traefik/ALB misrouting can skip the proxy entirely.
- Was the request signed? An unauthenticated request (no auth configured) hits whatever the default backend is.
- `alias:` in effect? The UI shows the virtual bucket name; the real name on the backend is the alias.

### Startup warning: "bucket 'X' routes to unknown backend 'Y' — route will be ignored"

`storage.buckets[X].backend` references a name that's not in the `backends[]` list (e.g. a backend was renamed or removed). Subsequent requests to bucket X land on the default backend instead. Update the routing or restore the backend. Note that objects already written to the old backend stay there — "route will be ignored" only affects future requests.

## S3 config sync ETag mismatch

**Log line:** `[config-sync] ETag mismatch on DB download — retrying`.

Expected when two instances mutate within the 5-minute poll window — the race resolves on the next cycle. Only a problem if it happens continuously.

Continuous mismatch usually means two instances are **both writing** via `DGP_CONFIG_SYNC_BUCKET`. Sync is not multi-master — one instance is the writer; others read. If you have multiple active writers, the "loudest" one wins and the others lose mutations.

Fix: run only one instance as the IAM administration surface and point the others at the same sync bucket read-only (see [How to run multiple instances](run-multiple-instances.md)). Or switch to `iam_mode: declarative` and manage IAM via YAML + GitOps (takes both writes out of the picture).

If an S3-compatible endpoint rejects `If-Match` conditional update PUTs, a single-writer deployment can set `DGP_CONFIG_SYNC_UPDATE_CAS=false`. With the opt-out, an existing remote DB that cannot be decrypted with the local bootstrap key is replaced by the next local upload instead of being treated as a guarded first create. Keep the default `true` whenever more than one instance can write.

## Audit ring is empty after a restart

The audit ring is **in-memory only**. It resets to empty on every restart — that's by design. For persistent audit, the authoritative source is stdout `tracing::info!` — scrape it into your log pipeline.

For operational (non-audit) logs — rate-limit, S3-error, replication lines — use **Observability → System logs** for a live, filterable tail without SSH; see [View live logs](view-live-logs.md). For retention, ship the `DGP_LOG_FORMAT=json` stdout stream to your aggregator.

Increase `DGP_AUDIT_RING_SIZE` (default 500) if you want a larger in-memory window for the admin UI view.

## Delta compression not kicking in

**Check the decision.** `/_/metrics` → `deltaglider_delta_decisions_total` broken out by `decision` label (`delta` / `passthrough` / `reference`).

If everything is `passthrough`, usually:

1. **Bucket has `compression: false`.** Check `/_/api/admin/config/section/storage`.
2. **File extension isn't in the delta allow-list.** Images, video, already-compressed archives skip delta entirely — by design. See [Delta compression](../explanation/delta-compression.md).
3. **`max_delta_ratio`** too strict. Default 0.75. Lowering it (0.5, 0.3) rejects more deltas; raising it (0.9) accepts more. The default is a reasonable balance.
4. **First upload in a deltaspace** is always the `reference` — no delta yet. Only the second and subsequent uploads in the same prefix generate deltas.

## Encryption at rest — symptoms and fixes

Background and mode mechanics: [Encryption at rest](../explanation/encryption-at-rest.md) and the [encryption reference](../reference/encryption.md).

### Reads return 500 with "object is encrypted but no key is configured"

The object's metadata carries `dg-encrypted` (it was encrypted) but the backend currently has no key — mode was flipped to `none`, or proxy-AES mode is missing the `key`. Restore the key — via `DGP_*_ENCRYPTION_KEY` env var, YAML, or the admin GUI's Backends panel. If the key is genuinely lost, the object is unrecoverable.

### Reads return 500 with "object was encrypted with key id 'X', but this backend is configured with key id 'Y'"

Rotation without a shim, OR a bucket routed to the wrong backend, OR two backends sharing storage with different keys. Most commonly: restore the old key as `legacy_key: <old-hex>` + `legacy_key_id: <X>` on the backend's encryption block to let historical reads go through (shim-assisted rotation — see the [encryption reference](../reference/encryption.md)). If the mismatch is a routing error, fix `storage.buckets[*].backend`. If two backends share physical storage with different keys, that's a config bug — pick one.

### Reads return 500 with "xattrs may have been stripped during backup/restore"

The object body starts with `DGE1` (proxy-AES chunked wire format) but has no `dg-encrypted` metadata marker — classic sign of a backup/restore round-trip that preserved file contents but dropped extended attributes. On filesystem backends, per-object metadata lives in the `user.dg.metadata` xattr; older `rsync` without `-X` and some S3 sync tools strip it. Re-run the backup with xattr support, or rebuild the metadata from a known-good source. See [How to back up and restore](back-up-and-restore.md).

### Startup fails: "backends X and Y share key_id but declare DIFFERENT keys"

Two backends pinned the same explicit `key_id` but the `key` values differ — the server refuses to start at engine construction. This is almost always a copy-paste error. Make the ids distinct, OR make the keys identical (the documented "portability" escape hatch for two aliases of the same physical bucket).

### Startup warning: "backend 'X' has encryption mode aes256-gcm-proxy but no key is configured"

YAML declares proxy-AES mode but there's no `key` in YAML and no `DGP_*_ENCRYPTION_KEY` in the environment — writes to this backend land as plaintext despite the declared mode. Set the env var, or put the hex key into YAML (treated as an infra secret — stripped by canonical exports so it doesn't leak via `/config/export`).

### Startup warning: "backend 'X' encryption key was loaded from config file (not DGP_*_ENCRYPTION_KEY)"

The key is in YAML rather than in an env var. Not an error, but the canonical export strips infra secrets — if you persist the YAML back from the admin API and treat it as the source of truth, the key will be gone on the round-trip. Move the key to an env var for operational hygiene.

### Writes to an SSE-KMS backend fail with "KMS key is disabled" or 403

The AWS KMS key is disabled, deleted, or the proxy's IAM role lacks `kms:GenerateDataKey` on it. Check the KMS key status in the AWS console, and confirm the proxy's role/credentials have:

- `s3:PutObject` + `s3:GetObject` on the bucket.
- `kms:Encrypt`, `kms:Decrypt`, `kms:GenerateDataKey`, `kms:DescribeKey` on the KMS key (or via a KMS grant).

### Disabled encryption on a backend — historical objects now fail to read

Expected if you removed the key entirely. The decrypt path errors explicitly (it won't serve ciphertext as plaintext). If you need the objects back, restore the key. If not, delete them. Note that `mode: none` with `legacy_key: <hex>` + `legacy_key_id: <id>` is a valid shape — this lets you disable new-write encryption while keeping historical reads working.

### Reads succeed but return garbage

GET returns 200 with an object that looks like random bytes, no error. This shouldn't happen in v0.9+ — every backend is always wrapped, and a missing key produces an explicit error. If you see it, it's a bug; file a report with the config + the first 16 bytes of the object body.

## Where to look next

- **Trace it.** Dry-run the failing request through the admission chain and read the audit log: [How to trace and audit requests](trace-requests.md).
- Set `RUST_LOG=deltaglider_proxy=trace` for maximum verbosity (`RUST_LOG` beats `DGP_LOG_LEVEL` beats `--verbose`). To change the level without a restart, use the admin UI: Settings → System → Logging.
- Hit the audit log API: `GET /_/api/admin/audit?limit=500` for a JSON dump of recent mutations + denials.
- `curl /_/metrics | grep deltaglider_` — 20+ Prometheus metrics, most tell a story. Mapping: [How to monitor with Prometheus and Grafana](monitor-with-prometheus.md).
- [Admin API reference](../reference/admin-api.md) — every debug-friendly admin endpoint.
