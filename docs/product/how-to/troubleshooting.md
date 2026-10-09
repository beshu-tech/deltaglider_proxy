# Troubleshooting

This guide maps the symptoms you'll see in the wild to their fixes. If your symptom isn't here, the audit log at `/_/admin/diagnostics/audit` and the structured logs (`tracing`) almost always show the real error. See [How to trace and audit requests](trace-requests.md).

## Client gets 403 AccessDenied

**Check the audit log first.** `/_/admin/diagnostics/audit` shows every IAM denial with the user, action, bucket, and path. The most common causes:

1. **Wrong prefix.** The user has `Allow read on releases/public/*` but tried `releases/private/foo.zip`. Prefix ABAC is exact, so a trailing `/` matters.
2. **User disabled.** Open **Access → Users** (`/_/admin/access/users`), select the user, and check that its **Enabled** switch is on.
3. **Deny rule wins.** Any matching Deny rule in the user's permissions (or any group they're in) wins over an Allow. Search the permissions of the user and of its groups.
4. **`iam_mode: declarative`**, and you try to mutate IAM through the admin API. This is expected behaviour: the API returns `403 { "error": "iam_declarative" }`. Edit the YAML and apply the document.
5. **Stale `${username}` template.** A permission resource written as `${username}` instead of `${iam:username}` is not substituted, so it matches nothing and the user is denied. Fix the template; the save-time config advisories flag this.

If the audit log is **empty** and you still see 403, the denial is in SigV4 verification, not IAM:

- Check `/_/metrics` for `deltaglider_auth_failures_total{reason="signature_rejected"}`.
- Is the client's system clock within `DGP_CLOCK_SKEW_SECONDS` (default 900 s) of the server? Look for `RequestTimeTooSkewed` in the client error.
- Is the access key typo'd? The proxy returns a generic AccessDenied, so the answer does not reveal whether the key exists.

## Intermittent 403s / one client locks out everyone

Symptom: requests succeed most of the time but fail intermittently, and the failures cluster. When one client is busy, *other* clients start to fail too. This is a rate-limit lockout from a shared rate-limit bucket, not an auth problem.

Cause: the proxy is behind a reverse proxy (Coolify, Traefik, nginx, ALB) but `DGP_TRUST_PROXY_HEADERS` is `false`, so every request looks like it comes from the proxy's IP. All clients share one rate-limit bucket; one client that keeps sending a wrong secret exhausts it, and the rest get throttled. The successful requests of the other clients do not clear the bucket.

Tells:
- Throttling returns `503 SlowDown`, but a client retry that re-sends auth can show up as `403`.
- The auth and lockout log lines include `bucket_key=` and `trust_proxy=`. If `bucket_key` is your reverse proxy's IP for unrelated clients, this is the cause.
- The save-time config advisories flag the rate-limit-on + trust-off combination.

Fix: set `DGP_TRUST_PROXY_HEADERS=true` and `DGP_TRUSTED_PROXY_CIDRS` to the reverse proxy's network (only behind a trusted proxy that injects `X-Forwarded-For`/`X-Real-IP`). See [Rate limits](../reference/rate-limits.md#ip-extraction).

## Admin login fails with the right password

The bootstrap password verification uses bcrypt. Usually one of:

1. **Stale `DGP_BOOTSTRAP_PASSWORD_HASH`.** When this variable is set, it sets the admin password at every start, so a password that you changed in the admin UI is replaced by the hash in the environment on the next restart. Put the hash of the current password in the variable, or remove the variable. The password hash does not encrypt the config DB, so a wrong hash never locks the database.

2. **Rate limiter lockout.** The limit is 100 failed attempts per IP in a 5-minute window, followed by a 10-minute lockout, and 10 failed logins per account in one hour lock that account for one hour. During a lockout, the right password is refused too. The sign-in form says "Too many sign-in attempts. Try again in N min.", and the admin API answers `429` with a `Retry-After` header. The counter is `deltaglider_auth_failures_total` on `/_/metrics`. Wait until the lockout ends, or see [Rate limits](../reference/rate-limits.md) for the knobs.

3. **Session IP binding.** If you log in from one IP and the admin cookie ends up used from a different IP (NAT flip, VPN change), the session is rejected. Log in again. Behind a reverse proxy, the binding uses the client address that the proxy names in `X-Forwarded-For`, and only when the proxy's network is in `DGP_TRUSTED_PROXY_CIDRS`.

## Startup fails: `xattr` support missing

Log line: `Data directory does not support extended attributes`.

The filesystem backend stores object metadata as `user.dg.metadata` xattrs on the data file's inode. The proxy validates this at startup and refuses to start otherwise.

Filesystems that support xattrs: ext4, XFS, Btrfs, ZFS, APFS.
Filesystems that don't: tmpfs, FAT32, exFAT, NFS-without-acl-over-xattr mount, some overlay2 configurations.

Fix: mount `DGP_DATA_DIR` on a supporting filesystem, or switch to the S3 backend.

## The config DB is locked: `The config DB key ... does not open the config DB`

No key that the proxy has opens `deltaglider_config.db`. The proxy moves the database aside as `deltaglider_config.db.bak`, starts with an empty database, and answers every S3 request with `503` until the right key is back. Usually one of these happened:

1. `DGP_CONFIG_DB_KEY` changed, or an instance got a different value than its peers. Set the value that encrypted the database and restart.
2. The key file `deltaglider_config.db.key` was lost, for example because a volume was recreated without it. Restore the file from a backup and restart. On the next start, the proxy moves the preserved database back into place.
3. You copied a database snapshot to another instance without its key. Start that instance with the same `DGP_CONFIG_DB_KEY`, or copy the key file along with the database.

The admin GUI shows a recovery wizard while the database is locked. It sends the candidate to `POST /_/api/admin/recover-db`, which tests it against the preserved database without changing anything and says whether it is a config DB key or a legacy bootstrap hash. The endpoint is public but rate-limited.

## Startup fails: `config_sync_bucket is set, but DGP_CONFIG_DB_KEY is not`

All instances that share a sync bucket share one encrypted database, so they need one key. Set `DGP_CONFIG_DB_KEY` to the same value on every instance (at least 32 characters, for example from `openssl rand -hex 32`) and restart. See [How to run multiple instances](run-multiple-instances.md).

## 502 Bad Gateway / 504 Gateway Timeout on large uploads

Symptom: multipart uploads of files >50 MB fail with `502` (Traefik) or `504` (Caddy / nginx). The embedded UI shows "Upload object … failed (502): Gateway returned 502 Bad Gateway." The proxy logs show `tower_http::trace::on_response: finished processing request latency=60000 ms status=400` (latency is exactly 60 000 ms).

Cause: the default request read-timeout of a reverse proxy is 60 seconds. A 16 MB multipart part over a typical home upload link (1 to 5 MB/s, shared between concurrent parts) takes longer than that. The reverse proxy closes the upstream connection in the middle of the body. Hyper then raises a body-read error, and axum's `Bytes` extractor returns `400 BAD_REQUEST` with the body "Failed to buffer the request body". The reverse proxy translates the broken upstream response into a 502 or 504 for the client.

Fix: extend the reverse-proxy read-timeout. A table of settings per reverse proxy, with examples, is in [How to serve TLS](serve-tls.md#raise-the-reverse-proxy-read-timeout--mandatory-for-large-uploads).

For Coolify users specifically:

```bash
# On the Coolify host:
sudo $EDITOR /data/coolify/proxy/docker-compose.yml
# Add to the traefik service `command:` block:
#   - '--entrypoints.https.transport.respondingTimeouts.readTimeout=30m'
#   - '--entrypoints.https.transport.respondingTimeouts.writeTimeout=30m'
sudo docker compose -f /data/coolify/proxy/docker-compose.yml up -d
```

Mitigation without operator access: the embedded uploader uploads one file at a time, so that each part gets a fair share of the upload link. For typical workloads, each part then completes within the default 60 s window. For very slow links or larger parts, you must still raise the reverse-proxy timeout.

## 503 SlowDown on PUT

A `503 SlowDown` comes either from the upstream S3 backend, when it throttles the proxy, or from the proxy itself. SDKs retry it. The message of the error says which limit refused the request. The proxy answers `503 SlowDown` in these cases:

1. **All delta codec slots are busy** (`all delta codec slots busy — try again later`). A PUT that needs a delta encode does not wait for a slot: it fails at once, because a waiting PUT would hold its whole body in memory. (A delta GET waits up to 60 seconds for a slot.) Check `/_/metrics` → `deltaglider_codec_semaphore_available` (`0` = saturated) and `deltaglider_delta_encode_duration_seconds`. If the codec is saturated, raise **Codec concurrency** in the **Caches** card of **System → System** (`advanced.codec_concurrency`, or `DGP_CODEC_CONCURRENCY`). The new limit applies at once, without a restart.
2. **The spool budget is used up** (`spool budget exhausted; retry shortly`). A request that holds no spool space waits for it, up to `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS` (default 120). A request that already holds spool space and needs more fails at once. Raise `DGP_SPOOL_MAX_BYTES` if this happens often.
3. **Too many multipart uploads** (`Too many concurrent multipart uploads`), limited by `DGP_MAX_MULTIPART_UPLOADS` (default 1000), or too many multipart bytes in flight (`Multipart in-flight bytes cap reached`), limited by `DGP_MAX_TOTAL_MULTIPART_BYTES`.
4. **A maintenance job gates the bucket** (see the next entry).
5. **Another instance changed the baseline of a delta prefix during the write.** This happens only with a `config_sync_bucket`.

A `503 ServiceUnavailable` that names a backend is a different case: see [How to diagnose a backend that isn't serving](diagnose-backend-connectivity.md).

## A request returns 503 ServiceUnavailable that names no backend

The message of this error is "The storage backend failed while it served this request. A retry can succeed." The S3 backend answered the proxy, but it failed while it served the request: it answered with a `500`, `502` or `504` after the SDK's own retries, or it stopped sending the body of an object. The proxy answers `503` with a `Retry-After` header, because a retry of the same request can succeed. SDKs and `curl --retry` retry it.

When the backend stops sending the body of an object, the proxy first tries to recover by itself. It requests the rest of the object again, starting at the first byte that it did not receive. The proxy asks for the same version of the object, so it never joins two versions. The client gets the error only when these new requests fail too.

The proxy log has one line for each such error: `transient storage fault answered 503, cause: …`. The cause names what failed. A cause with `minimum throughput was specified at 1 B/s, but throughput of 0 B/s was observed` means that the backend sent no bytes for the stall grace period. This period is `DGP_S3_STALL_GRACE_SECS` (default 20 seconds). A backend that often pauses for longer than this needs a larger value. A cause with `status=502` or `status=504` comes from the backend or from a load balancer in front of it.

## Writes to one bucket return 503 SlowDown

A maintenance job (re-encryption, migration, or metadata backfill) is running on that bucket. The proxy intentionally refuses writes while the job rewrites objects. SDKs retry automatically and succeed when the job finishes. Reads are unaffected. Check **Storage → Jobs** (`/_/admin/jobs`, or `GET /_/api/admin/jobs`) for the job's progress; cancel it if it shouldn't be running. A job survives restarts by design, so if a job is stuck, cancel it through `POST /_/api/admin/jobs/maintenance:<id>/cancel` rather than restarting the proxy. See [Jobs reference](../reference/jobs.md).

## Cache miss storm on GET

Symptom: a sudden latency spike on GETs, and `deltaglider_cache_miss_rate_ratio` jumps above 0.5.

Most common cause: a restart with a cold cache against a hot-read workload. This is expected for about 5 minutes, while the LRU fills again.

Less common: `DGP_CACHE_MB` is undersized. The startup log warns `[cache] In-memory reference cache is only 100 MB — recommend ≥1024 MB for production`. Raise it.

Very rare: a write burst pushes new references into the cache and evicts the hot set. Check `deltaglider_delta_decisions_total{decision="reference"}` rate. If you're creating many new deltaspaces quickly, consider segregating write-heavy and read-heavy workloads onto separate buckets (different LRU scope) or different instances.

## Public prefix returns 403

```yaml
# validate
storage:
  buckets:
    downloads:
      public_prefixes:
        - public/        # note the trailing /
```

Checks in order:

1. **Trailing slash on the prefix.** `public/` matches `public/foo.zip` but **not** `publicish/bar.zip`. Always end prefixes with `/`.
2. **Bucket policy applied.** `/_/api/admin/config/section/storage` should show the `public_prefixes` array. If it is empty, the YAML was not applied. Check the `config apply` response again.
3. **Reverse proxy stripping the path.** If Traefik / Caddy is rewriting the URL (e.g. `/downloads/public/*` → `/public/*`), the proxy sees a different bucket than the client intends. Point the reverse proxy at the proxy 1:1.

Confirm which layer denies the request with a synthetic trace. See [How to trace and audit requests](trace-requests.md).

## Object goes to the wrong backend

Per-bucket backend routing lives in `storage.buckets[name].backend`. The quickest check:

```bash
# Confirm the per-bucket routing
curl -b cookies "https://s3.acme.example/_/api/admin/config/section/storage?format=yaml"
```

If the routing looks right but the object still went somewhere unexpected:

- Did the PUT hit the proxy or the backend directly? Traefik/ALB misrouting can skip the proxy entirely.
- Was the request signed? An unauthenticated request (no auth configured) hits whatever the default backend is.
- `alias:` in effect? The UI shows the virtual bucket name; the real name on the backend is the alias.

### Startup error: "bucket 'X' routes to undefined backend 'Y'"

The full message is `FATAL config error: bucket 'X' routes to undefined backend 'Y' — define the backend under storage.backends or remove the route (available: [...])`. The route `storage.buckets[X].backend` names a backend that is not in the `storage.backends` list, for example because a backend was renamed or removed. The proxy refuses to start with such a config, `config lint` refuses it with exit code 6, and an admin apply refuses it too. Define the backend again, or change or remove the route. The objects on the old backend are not affected.

## S3 config sync ETag mismatch

Log line: `Config DB sync (...): CAS conflict — merging peer state before retry`.

Each upload of the config DB is conditional on the copy that the instance saw last. When another instance uploaded in between, the sync bucket refuses the upload, and the instance downloads the new copy, merges it with its own changes, and uploads again. This is expected when two instances change IAM at about the same time, and no change is lost. It is a problem only if it happens continuously, or if the log also says `upload retries exhausted`.

To see whether the sync of an instance works, send a request to `GET /_/api/admin/config/sync`, or watch the `deltaglider_config_sync_healthy` gauge on `/_/metrics`. An `iam_sync_conflict` audit entry means that two instances changed the same column of the same row, and the merge kept the more recent change. See [How to run multiple instances](run-multiple-instances.md).

## Audit ring is empty after a restart

The audit ring is in memory only. By design, it resets to empty on every restart. For a persistent audit, the authoritative source is the stdout `tracing::info!` output. Collect it into your log pipeline.

For operational (non-audit) logs, such as the rate-limit, S3-error, and replication lines, use **Observability → System logs** for a live, filterable tail without SSH; see [View live logs](view-live-logs.md). For retention, ship the `DGP_LOG_FORMAT=json` stdout stream to your aggregator.

Increase `DGP_AUDIT_RING_SIZE` (default 500) if you want a larger in-memory window for the admin UI view.

## Delta compression not kicking in

**Check the decision.** `/_/metrics` → `deltaglider_delta_decisions_total` broken out by `decision` label (`delta` / `passthrough` / `reference`).

If everything is `passthrough`, usually:

1. **Bucket has `compression: false`.** Check `/_/api/admin/config/section/storage`.
2. **File extension isn't in the delta allow-list.** By design, images, video, and already-compressed archives skip delta entirely. See [Delta compression](../explanation/delta-compression.md).
3. **`max_delta_ratio`** too strict. Default 0.75. Lowering it (0.5, 0.3) rejects more deltas; raising it (0.9) accepts more. The default balances the two.
4. **The first upload in a deltaspace** is always the `reference`, so there is no delta yet. Only the second and subsequent uploads in the same prefix generate deltas.

## Encryption at rest: symptoms and fixes

Background and mode mechanics: [Encryption at rest](../explanation/encryption-at-rest.md) and the [encryption reference](../reference/encryption.md).

### Reads return 500 with "object is encrypted but no key is configured"

The object's metadata carries `dg-encrypted` (it was encrypted) but the backend has no key now. Either the mode was changed to `none`, or proxy-AES mode is missing the `key`. Restore the key through the `DGP_*_ENCRYPTION_KEY` env var, the YAML, or the backend card on **Storage → Backends** in the admin GUI. If the key is lost, the object is unrecoverable.

### Reads return 500 with "object was encrypted with key id 'X', but this backend is configured with key id 'Y'"

The cause is a rotation without a shim, a bucket routed to the wrong backend, or two backends that share storage with different keys. In the most common case, restore the old key as `legacy_key: <old-hex>` and `legacy_key_id: <X>` on the backend's encryption block, so that historical reads go through. This is shim-assisted rotation (see the [encryption reference](../reference/encryption.md)). If the mismatch is a routing error, fix `storage.buckets[*].backend`. If two backends share physical storage with different keys, that is a config bug. Pick one key.

### Reads return 500 with "xattrs may have been stripped during backup/restore"

The object body starts with `DGE1` (proxy-AES chunked wire format) but has no `dg-encrypted` metadata marker. This is the typical sign of a backup/restore round-trip that preserved file contents but dropped extended attributes. On filesystem backends, per-object metadata lives in the `user.dg.metadata` xattr; older `rsync` without `-X` and some S3 sync tools strip it. Re-run the backup with xattr support, or rebuild the metadata from a known-good source. See [How to back up and restore](back-up-and-restore.md).

### Startup fails: "backends X and Y share key_id but declare DIFFERENT keys"

Two backends pinned the same explicit `key_id`, but the `key` values differ. The server refuses to start when it builds the engine. This is almost always a copy-paste error. Make the ids distinct, or make the keys identical (the documented "portability" escape hatch for two aliases of the same physical bucket).

### Startup warning: "backend 'X' has encryption mode aes256-gcm-proxy but no key is configured"

YAML declares proxy-AES mode but there's no `key` in YAML and no `DGP_*_ENCRYPTION_KEY` in the environment. Writes to this backend are therefore stored as plaintext, despite the declared mode. Set the env var, or put the hex key into the YAML. The proxy treats a key in the YAML as an infra secret and strips it from canonical exports, so it does not leak through `/config/export`.

### Startup warning: "backend 'X' encryption key was loaded from config file (not DGP_*_ENCRYPTION_KEY)"

The key is in YAML rather than in an env var. This is not an error, but the canonical export strips infra secrets. If you persist the YAML back from the admin API and treat it as the source of truth, the round-trip loses the key. Move the key to an env var for operational hygiene.

### Writes to an SSE-KMS backend fail with "KMS key is disabled" or 403

The AWS KMS key is disabled, deleted, or the proxy's IAM role lacks `kms:GenerateDataKey` on it. Check the KMS key status in the AWS console, and confirm the proxy's role/credentials have:

- `s3:PutObject` and `s3:GetObject` on the bucket.
- `kms:Encrypt`, `kms:Decrypt`, `kms:GenerateDataKey`, `kms:DescribeKey` on the KMS key (or via a KMS grant).

### Disabled encryption on a backend: historical objects fail to read

Expected if you removed the key entirely. The decrypt path errors explicitly (it won't serve ciphertext as plaintext). If you need the objects back, restore the key. If not, delete them. The shape `mode: none` with `legacy_key: <hex>` and `legacy_key_id: <id>` is valid. It lets you disable new-write encryption while keeping historical reads working.

### Reads succeed but return garbage

GET returns 200 with an object that looks like random bytes, and no error. This should not happen, because every backend is always wrapped and a missing key produces an explicit error. If you see it, it is a bug. File a report with the config and the first 16 bytes of the object body.

## Where to look next

- **Trace it.** Dry-run the failing request through the admission chain and read the audit log: [How to trace and audit requests](trace-requests.md).
- Set `RUST_LOG=deltaglider_proxy=trace` for maximum verbosity (`RUST_LOG` beats `DGP_LOG_LEVEL`, which beats `advanced.log_level`). Without any of them, the level is `deltaglider_proxy=info,tower_http=info`. To change the level without a restart, select a level in the **Log level** card of **System → System** and apply it; see [View live logs](view-live-logs.md#2-raise-the-log-level-to-debug). The card is read-only while `RUST_LOG` or `DGP_LOG_LEVEL` is set.
- Hit the audit log API: `GET /_/api/admin/audit?limit=500` for a JSON dump of recent mutations and denials.
- `curl /_/metrics | grep deltaglider_` lists more than 20 Prometheus metrics. Mapping: [How to monitor with Prometheus and Grafana](monitor-with-prometheus.md).
- [Admin API reference](../reference/admin-api.md): every admin endpoint that helps with debugging.
