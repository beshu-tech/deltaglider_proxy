# Upgrade guide

*How to move between DeltaGlider Proxy versions safely, plus the TOML → YAML migration.*

## Standard upgrade workflow

The proxy is a single stateful binary. Upgrades are "backup, swap, verify."

1. **Back up first.** From the admin UI: **Full Backup → Export**, or via API:

   ```bash
   curl -b /tmp/admin.cookies \
     "https://dgp.example.com/_/api/admin/backup" \
     -o dgp-backup-$(date +%Y%m%d-%H%M%S).zip
   ```

   The zip is atomic and sha256-verified on restore — see [reference/admin-api.md](reference/admin-api.md#full-backup-wave-111). Store it somewhere the upgrade process itself can't break.

2. **Roll the image/binary.** For Docker:

   ```bash
   docker pull beshultd/deltaglider_proxy:0.8.x
   docker stop dgp && docker rm dgp
   docker run -d --name dgp -p 9000:9000 \
     -v dgp-data:/data \
     -e DGP_BOOTSTRAP_PASSWORD_HASH=... \
     beshultd/deltaglider_proxy:0.8.x
   ```

   Coolify, Kubernetes, and systemd have their own "pull + restart" verbs. All that matters: `/data` persists across the swap.

3. **Verify.** Four checks:

   ```bash
   # Health
   curl -s https://dgp.example.com/_/health

   # Version matches the image you deployed
   curl -s -b cookies https://dgp.example.com/_/api/whoami | jq .version

   # A read against an existing object (regression test)
   aws --endpoint-url https://dgp.example.com s3 ls s3://my-bucket

   # Admin session still works
   curl -b cookies https://dgp.example.com/_/api/admin/users | jq '.[] | .name'
   ```

4. **If something broke**, the backup zip from step 1 imports atomically:

   ```bash
   curl -b cookies -X POST \
     -H "Content-Type: application/zip" \
     --data-binary @dgp-backup-...zip \
     https://dgp.example.com/_/api/admin/backup
   ```

## Version compatibility

Patch and minor upgrades inside the `0.8.x` line are drop-in. The config file format, IAM DB schema, and S3 wire format are stable across `0.8.*`.

**Across minors:** schema migrations run automatically on first start. The config DB is on schema v5 as of v0.8.0; any binary `0.8.0+` migrates forward on boot. **Forward migrations are one-way** — once the DB is on v5, an older binary won't read it.

**Across majors (future 0.x → 1.0):** pre-release — expect breaking changes. Always follow the release notes for that version, and always export a Full Backup before trying it.

## TOML → YAML migration

YAML is the canonical format as of v0.8.0. TOML still loads but emits a deprecation warning on every startup (suppress with `DGP_SILENCE_TOML_DEPRECATION=1`). TOML will be removed in a future minor release; migrate at your own pace within the grace window.

### One-liner (most installs)

```bash
deltaglider_proxy config migrate \
  /etc/deltaglider_proxy/config.toml \
  --out /etc/deltaglider_proxy/config.yaml
```

Point the server at the new file (`--config` flag, `DGP_CONFIG` env, or via the standard search path) and restart. Done.

### Step-by-step

**1. Run the migrator.**

```bash
deltaglider_proxy config migrate /etc/deltaglider_proxy/config.toml \
  --out /etc/deltaglider_proxy/config.yaml
```

Emits one file per section if you pass `--split`, useful for GitOps setups:

```bash
deltaglider_proxy config migrate config.toml --split --out-dir ./config.d/
# writes: admission.yaml, access.yaml, storage.yaml, advanced.yaml
```

**2. Inspect the output.** Canonical YAML uses the four-section shape:

```yaml
admission:
  blocks: []

access:
  access_key_id: ...
  secret_access_key: ...
  iam_mode: gui

storage:
  backend:
    type: s3
    endpoint: ...
    region: ...
  buckets: {}

advanced:
  cache_size_mb: 1024
  session_ttl_hours: 4
```

All secrets come through redacted (`null`) — the migrator intentionally doesn't copy them. Inject them via env vars or the admin UI (see step 4).

**3. Validate before applying.** The `config lint` subcommand parses + validates without touching the server:

```bash
deltaglider_proxy config lint /etc/deltaglider_proxy/config.yaml
# Exit: 0 = valid, 3 = I/O, 4 = parse, 6 = validation
```

Wire this into CI so drift is caught in PR.

**4. Point the server at the new file.** File search order (first match wins):

1. `DGP_CONFIG` env var
2. `./deltaglider_proxy.yaml`
3. `./deltaglider_proxy.yml`
4. `./deltaglider_proxy.toml` (deprecated)
5. `/etc/deltaglider_proxy/config.yaml`
6. `/etc/deltaglider_proxy/config.yml`
7. `/etc/deltaglider_proxy/config.toml` (deprecated)

If you keep both `.toml` and `.yaml` in the same directory, `.yaml` wins.

**5. Feed secrets back in.** The migrator redacts:

- `access.access_key_id` / `access.secret_access_key` → inject via `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY` or use the admin UI.
- Storage backend creds → `DGP_BE_AWS_ACCESS_KEY_ID` / `DGP_BE_AWS_SECRET_ACCESS_KEY` or the admin UI.
- `advanced.bootstrap_password_hash` → `DGP_BOOTSTRAP_PASSWORD_HASH` env var (base64-wrapped form avoids `$` escaping issues in Docker).
- OAuth `client_secret` values — restore via the admin UI, or via a Full Backup zip import (see [reference/admin-api.md](reference/admin-api.md#full-backup-wave-111)).

**6. Silence the deprecation warning on stragglers.** If you can't migrate immediately (e.g. third-party Ansible using TOML):

```bash
DGP_SILENCE_TOML_DEPRECATION=1 deltaglider_proxy
```

## The S3-synced IAM database

Entirely separate from the YAML config. `deltaglider_config.db` (SQLCipher-encrypted SQLite) holds users, groups, OAuth providers, mapping rules. The YAML config never carries IAM state.

When upgrading across instances with `DGP_CONFIG_SYNC_BUCKET` set, the *newer* binary uploads after any mutation; *older* binaries (still running during a rolling upgrade) download but won't understand post-migration schema changes. Either:

- Upgrade all instances before making IAM mutations, **or**
- Accept that mid-rollout mutations are lost on older-reader downloads until they too upgrade.

## Common gotchas

- **`$` in Docker env.** Bcrypt hashes contain `$`. Use base64-wrapped form (`DGP_BOOTSTRAP_PASSWORD_HASH=JDJ5JDEyJGV...`) or single-quote the value in compose files.
- **`force_path_style`.** MinIO needs `true`; AWS S3 needs `false`. The migrator preserves whatever the TOML had.
- **Implicit defaults.** Fields absent from YAML take their default. Don't port fields that were already default in TOML — it clutters the canonical shape.
- **Admission chain order.** Admission blocks are order-significant. The migrator preserves order; review `admission:` carefully.
- **`iam_mode: declarative`.** New in v0.8.0. YAML becomes authoritative; admin-API IAM mutations return 403. Only set this *after* seeding the IAM DB via a GUI session or a Full Backup import.

## Verification checklist (after any upgrade or migration)

- [ ] `/_/health` returns HTTP 200.
- [ ] `/_/api/whoami` reports the expected `version`.
- [ ] An existing object downloads byte-identical: `aws s3 cp s3://bucket/known-file ./out && sha256sum out` matches the known checksum.
- [ ] The admin UI logs in with the bootstrap password (or OAuth) on the first try.
- [ ] `/_/admin/diagnostics/audit` shows recent entries — the audit ring is populating.
- [ ] Prometheus scrape returns valid metrics (if monitoring is wired up).

## Related

- [reference/configuration.md](reference/configuration.md) — the complete YAML field reference.
- [reference/admin-api.md](reference/admin-api.md) — Full Backup export/import.
- [20-production-deployment.md](20-production-deployment.md) — the ops side of running the server.
