# Troubleshooting

*Symptoms you'll see in the wild, mapped to the fix.*

If your symptom isn't here, the audit log at `/_/admin/diagnostics/audit` and the structured logs (`tracing`) are almost always where the real error lives.

## Client gets 403 AccessDenied

**Check the audit log first.** `/_/admin/diagnostics/audit` shows every IAM denial with the user, action, bucket, and path. The most common causes:

1. **Wrong prefix.** The user has `Allow read on my-bucket/public/*` but tried `my-bucket/private/foo.zip`. Prefix ABAC is exact — a trailing `/` matters.
2. **User disabled.** Access → Users → confirm the row is `Enabled`.
3. **Deny rule wins.** Any matching Deny rule in the user's permissions (or any group they're in) wins over an Allow. Grep the user's permissions + the groups'.
4. **`iam_mode: declarative`** and you're trying to mutate IAM via the admin API. Expected behaviour — the API returns `403 { "error": "iam_declarative" }`. Edit YAML and apply the document.

If the audit log is **empty** and you still see 403, the denial is in SigV4 verification, not IAM:

- Check `/_/metrics` for `deltaglider_auth_failures_total{reason="invalid_signature"}`.
- Is the client's system clock within `DGP_CLOCK_SKEW_SECONDS` (default 300s) of the server? Look for `RequestTimeTooSkewed` in the client error.
- Is the access key typo'd? The proxy returns a generic AccessDenied rather than leaking key-existence.

## Admin login fails with the right password

The bootstrap password verification uses bcrypt. Usually one of:

1. **Stale `DGP_BOOTSTRAP_PASSWORD_HASH`.** If you rotated the plaintext but forgot to update the env, the DB key drifts from the hash. The DB refuses to decrypt on restart — you'll see `Failed to open config DB: invalid passphrase` in startup logs.

   Fix: restart with the correct hash, then rotate via the admin UI once logged in (that path re-encrypts the DB atomically).

2. **Rate limiter lockout.** 100 failed attempts / 5-minute window / per-IP, with a 10-minute lockout after. `/_/metrics` → `deltaglider_auth_failures_total`. Wait it out or reduce `DGP_RATE_LIMIT_MAX_ATTEMPTS` tests.

3. **Session IP binding.** If you log in from one IP and the admin cookie ends up used from a different IP (NAT flip, VPN change), the session is rejected. Log in again. Disable `DGP_TRUST_PROXY_HEADERS` if you're not behind a reverse proxy — otherwise clients can spoof IPs.

## Startup fails: `xattr` support missing

**Log line:** `Data directory does not support extended attributes`.

The filesystem backend stores object metadata as `user.dg.metadata` xattrs on the data file's inode. The proxy validates this at startup and refuses to start otherwise.

Filesystems that support xattrs: ext4, XFS, Btrfs, ZFS, APFS.
Filesystems that don't: tmpfs, FAT32, exFAT, NFS-without-acl-over-xattr mount, some overlay2 configurations.

Fix: mount `DGP_DATA_DIR` on a supporting filesystem, or switch to the S3 backend.

## Startup fails: `SQLCipher could not open config DB`

The encryption key doesn't match the DB file. Usually:

1. You restored a Full Backup zip on a fresh instance but didn't feed the corresponding `bootstrap_password_hash` back in. The zip's `secrets.json` carries it — re-import the zip, or inject `DGP_BOOTSTRAP_PASSWORD_HASH` before the restore.
2. You rotated the bootstrap password outside the admin UI (edited env, restarted). The admin UI's `PUT /_/api/admin/password` is the only safe path — it re-encrypts the DB atomically.

Recovery path: `POST /_/api/admin/recover-db` with the correct password. The endpoint is public but rate-limited.

## 503 SlowDown on PUT

The proxy doesn't generate 503 itself — this comes from the upstream S3 backend when it's throttling you. Two tuning knobs:

1. **`DGP_MAX_MULTIPART_UPLOADS`** (default 1000) — limits concurrent multiparts in flight. Lowering this reduces the proxy's burst pressure on the backend.
2. **`DGP_CODEC_CONCURRENCY`** — limits xdelta3 subprocess permits. When this saturates, PUTs queue on delta encoding; the backend isn't the bottleneck.

Check `/_/metrics` → `deltaglider_codec_semaphore_available` (`0` = saturated) and `deltaglider_delta_encode_duration_seconds` for codec pressure. If codec is saturated, bump `DGP_CODEC_CONCURRENCY`.

## Cache miss storm on GET

**Symptom:** sudden latency spike on GETs; `deltaglider_cache_miss_rate_ratio` jumps above 0.5.

Most common cause: a restart with a cold cache against a hot-read workload. Expected for ~5 minutes; the LRU repopulates.

Less common: `DGP_CACHE_MB` is undersized. The startup log warns `[cache] In-memory reference cache is only 100 MB — recommend ≥1024 MB for production`. Bump it.

Very rare: a write burst is pushing fresh references in and evicting the hot set. Check `deltaglider_delta_decisions_total{decision="reference"}` rate. If you're creating many new deltaspaces quickly, consider segregating write-heavy and read-heavy workloads onto separate buckets (different LRU scope) or different instances.

## Public prefix returns 403

```yaml
storage:
  buckets:
    releases:
      public_prefixes:
        - builds/public/       # note the trailing /
```

Checks in order:

1. **Trailing slash on the prefix.** `builds/public/` matches `builds/public/foo.zip` but **not** `builds/publicish/bar.zip`. Always end prefixes with `/`.
2. **Bucket policy actually applied.** `/_/api/admin/config/section/storage` should show the `public_prefixes` array. If it's empty, the YAML didn't land — re-check `config apply` response.
3. **Reverse proxy stripping the path.** If Traefik / Caddy is rewriting the URL (e.g. `/releases/builds/public/*` → `/builds/public/*`), the proxy sees a different bucket than the client intends. Point the reverse proxy at the proxy 1:1.

## Object goes to the wrong backend

Per-bucket backend routing lives in `storage.buckets[name].backend`. Quickest debugging:

```bash
# Confirm the per-bucket routing
curl -b cookies https://dgp.example.com/_/api/admin/config/section/storage?format=yaml
```

If the routing looks right but the object still went somewhere unexpected:

- Did the PUT hit the proxy or the backend directly? Traefik/ALB misrouting can skip the proxy entirely.
- Was the request signed? An unauthenticated request (no auth configured) hits whatever the default backend is.
- `alias:` in effect? The UI shows the virtual bucket name; the real name on the backend is the alias.

## S3 config sync ETag mismatch

**Log line:** `[config-sync] ETag mismatch on DB download — retrying`.

Expected when two instances mutate within the 5-minute poll window — the race resolves on the next cycle. Only a problem if it happens continuously.

Continuous mismatch usually means two instances are **both writing** via `DGP_CONFIG_SYNC_BUCKET`. Sync is not multi-master — one instance is the writer; others read. If you have multiple active writers, the "loudest" one wins and the others lose mutations.

Fix: run only one instance as the IAM administration surface and point the others at the same sync bucket read-only. Or switch to `iam_mode: declarative` and manage IAM via YAML + GitOps (takes both writes out of the picture).

## Audit ring is empty after a restart

The audit ring is **in-memory only**. It resets to empty on every restart — that's by design. For persistent audit, the authoritative source is stdout `tracing::info!` — scrape it into your log pipeline.

Increase `DGP_AUDIT_RING_SIZE` (default 500) if you want a larger in-memory window for the admin UI view.

## Delta compression not kicking in

**Check the decision.** `/_/metrics` → `deltaglider_delta_decisions_total` broken out by `decision` label (`delta` / `passthrough` / `reference`).

If everything is `passthrough`, usually:

1. **Bucket has `compression: false`.** Check `/_/api/admin/config/section/storage`.
2. **File extension isn't in the delta allow-list.** Images, video, already-compressed archives skip delta entirely — by design. See [reference/how-delta-works.md](reference/how-delta-works.md).
3. **`max_delta_ratio`** too strict. Default 0.75. Lowering it (0.5, 0.3) rejects more deltas; raising it (0.9) accepts more. The default is a reasonable balance.
4. **First upload in a deltaspace** is always the `reference` — no delta yet. Only the second and subsequent uploads in the same prefix generate deltas.

## Need more detail?

- Set `RUST_LOG=deltaglider_proxy=trace` for maximum verbosity.
- Hit the audit log API: `GET /_/api/admin/audit?limit=500` for a JSON dump of recent mutations + denials.
- `curl /_/metrics | grep deltaglider_` — 20+ Prometheus metrics, most tell a story.
- [reference/admin-api.md](reference/admin-api.md) — every debug-friendly admin endpoint.
