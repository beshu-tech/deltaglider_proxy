# How to upgrade the proxy

This guide shows you how to move between DeltaGlider Proxy versions safely, including the one-time TOML → YAML config conversion (mandatory before v1.4.1) and the v0.9 encryption-config change.

## Standard upgrade workflow

The proxy is a single stateful binary. Upgrades are "backup, swap, verify."

1. **Back up first.** From the admin UI: **Full Backup → Export**, or via API:

   ```bash
   curl -b /tmp/admin.cookies \
     "https://s3.acme.example/_/api/admin/backup" \
     -o dgp-backup-$(date +%Y%m%d-%H%M%S).zip
   ```

   The zip is atomic and sha256-verified on restore — see [Admin API reference](../reference/admin-api.md). Store it somewhere the upgrade process itself can't break.

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
   curl -s https://s3.acme.example/_/health

   # Version matches the image you deployed
   curl -s -b cookies https://s3.acme.example/_/api/whoami | jq .version

   # A read against an existing object (regression test)
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases

   # Admin session still works
   curl -b cookies https://s3.acme.example/_/api/admin/users | jq '.[] | .name'
   ```

4. **If something broke**, the backup zip from step 1 imports atomically:

   ```bash
   curl -b cookies -X POST \
     -H "Content-Type: application/zip" \
     --data-binary @dgp-backup-...zip \
     https://s3.acme.example/_/api/admin/backup
   ```

## Version compatibility

Patch and minor upgrades inside the `0.8.x` line are drop-in. The config file format, IAM DB schema, and S3 wire format are stable across `0.8.*`.

**Across minors:** schema migrations run automatically on first start. The config DB is on schema v6 in current builds (v6 adds replication runtime-state tables); any binary `0.8.0+` migrates forward on boot. **Forward migrations are one-way** — once the DB is upgraded, an older binary may not read it.

**Across majors (future 0.x → 1.0):** pre-release — expect breaking changes. Always follow the [release notes for that version](https://github.com/beshu-tech/deltaglider_proxy/releases), and always export a Full Backup before trying it.

## TOML → YAML migration (mandatory before v1.4.1)

**TOML support was removed in v1.4.1.** A v1.4.1+ proxy refuses to start when its config is a `.toml` file — whether pointed at via `DGP_CONFIG` / `--config` or found on the default search path — with the error `TOML configs are no longer supported (removed in v1.4.1)`. There is no deprecation warning anymore, and the `config migrate` subcommand is gone from v1.4.1+ binaries.

The one-time conversion path is: **run `config migrate` on v1.4.0 (the last release that ships it), point the server at the YAML file, verify, and only then upgrade to v1.4.1+.**

### One-liner (most installs)

```bash
# On a v1.4.0 binary — config migrate no longer exists in v1.4.1+
deltaglider_proxy config migrate \
  /etc/deltaglider_proxy/config.toml \
  --out /etc/deltaglider_proxy/config.yaml
```

Point the server at the new file (`--config` flag, `DGP_CONFIG` env, or via the standard search path), restart, verify — then upgrade.

### Step-by-step

**1. Run the migrator (on v1.4.0).**

```bash
deltaglider_proxy config migrate /etc/deltaglider_proxy/config.toml \
  --out /etc/deltaglider_proxy/config.yaml
```

Without `--out`, the YAML is written to stdout — pipe it wherever you like. `${env:NAME}` placeholders are not expanded; they migrate verbatim.

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

SigV4 credentials (`access.access_key_id` / `secret_access_key` and storage backend creds) are **kept**, so the output is drop-in usable. Only infra secrets — the bootstrap password hash and any encryption keys — are stripped; feed those back in via env vars (see step 5).

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
4. `./deltaglider_proxy.toml` (tripwire — startup fails on v1.4.1+)
5. `/etc/deltaglider_proxy/config.yaml`
6. `/etc/deltaglider_proxy/config.yml`
7. `/etc/deltaglider_proxy/config.toml` (tripwire — startup fails on v1.4.1+)

If you keep both `.toml` and `.yaml` in the same directory, `.yaml` wins — but delete the `.toml` once verified: on v1.4.1+ a leftover TOML matched first by the search stops startup rather than being silently ignored.

**5. Feed the stripped secrets back in.** The migrator strips infra secrets only:

- `advanced.bootstrap_password_hash` → `DGP_BOOTSTRAP_PASSWORD_HASH` env var (base64-wrapped form avoids `$` escaping issues in Docker).
- Per-backend encryption keys → `DGP_ENCRYPTION_KEY` (singleton backend) or `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` (named backends).

OAuth `client_secret` values live in the encrypted config DB, not the YAML — they are untouched by the migration.

**6. Delete the old TOML and upgrade.** Once the v1.4.0 proxy runs cleanly from the YAML file, remove the `.toml` (it would trip the v1.4.1+ startup check) and roll to the new version.

## The S3-synced IAM database

Entirely separate from the YAML config. `deltaglider_config.db` (SQLCipher-encrypted SQLite) holds users, groups, OAuth providers, mapping rules. The YAML config never carries IAM state (unless you run [declarative IAM](../reference/declarative-iam.md)).

When upgrading across instances with `DGP_CONFIG_SYNC_BUCKET` set, the *newer* binary uploads after any mutation; *older* binaries (still running during a rolling upgrade) download but won't understand post-migration schema changes. Either:

- Upgrade all instances before making IAM mutations, **or**
- Accept that mid-rollout mutations are lost on older-reader downloads until they too upgrade.

## Upgrade to the separate config DB key

Earlier releases encrypted `deltaglider_config.db` with the bootstrap password hash. Current releases use a separate key: `DGP_CONFIG_DB_KEY`, or the key file `deltaglider_config.db.key` next to the database. The first start after the upgrade re-encrypts the database from the hash to the new key. That step works on a copy of the database, and the copy replaces the original only after it opens with the new key.

- **One instance:** nothing to do. The proxy generates the key file on the first start and re-encrypts the database with it. Back up the key file together with the database from then on.
- **Several instances with `config_sync_bucket`:** generate one key (`openssl rand -hex 32`) and set it as `DGP_CONFIG_DB_KEY` on every instance before you start the new release. An instance with a sync bucket but without the variable refuses to start. Upgrade all instances together and avoid IAM changes during the rollout, because instances on the old release cannot read uploads under the new key. The new release still accepts a synced database that an old instance wrote under the hash.
- **Kubernetes operator with `bootstrapPassword.autoGenerate`:** the operator adds a `dbKey` to the `<name>-bootstrap` Secret and injects it into every pod. Without `autoGenerate`, add `DGP_CONFIG_DB_KEY` to your env Secret; the operator refuses to scale beyond one pod without it.
- **Before the upgrade, keep the hash that encrypted the database.** The first start needs it once, to open the database. After the upgrade, the hash is only the admin password, and a change of the password no longer touches the database.

## Common gotchas

- **`$` in Docker env.** Bcrypt hashes contain `$`. Use the base64-wrapped form (`DGP_BOOTSTRAP_PASSWORD_HASH=JDJ5JDEyJGV...`) or single-quote the value in compose files.
- **`force_path_style`.** MinIO needs `true`; AWS S3 needs `false`. The migrator preserves whatever the TOML had.
- **Implicit defaults.** Fields absent from YAML take their default. Don't port fields that were already default in TOML — it clutters the canonical shape.
- **Request rule order.** The order of the request rules (`admission.blocks`) matters. The migrator preserves order; review `admission:` carefully.
- **`iam_mode: declarative`.** YAML becomes authoritative for IAM users, groups, OAuth providers, and mapping rules. Admin-API IAM mutations return 403; `/config/apply` reconciles the encrypted DB to YAML atomically. Seed from an existing DB with `GET /_/api/admin/config/declarative-iam-export`, or author IAM directly in YAML.

## v0.9: per-backend encryption (breaking)

v0.9 replaced the single global `advanced.encryption_key` field with per-backend encryption blocks. If you're upgrading from a pre-0.9 pre-release, the old field is no longer recognized — any YAML still carrying it silently drops the key.

**What changed:**

| Before (pre-v0.9) | After (v0.9) |
|---|---|
| `advanced.encryption_key: <hex>` | `storage.backend_encryption: { mode: aes256-gcm-proxy, key: <hex> }` (singleton) or `storage.backends[*].encryption: { ... }` (list) |
| `DGP_ENCRYPTION_KEY` env var | Same name for singleton path; `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` for named backends |
| Global `encryption_enabled` in `GET /config` | Per-backend `encryption` summary on each `BackendInfoResponse` |
| Dedicated `EncryptionPanel` page | Subsection inside each backend card on the `BackendsPanel` |

**Mechanical conversion** — single-backend deployment, pre-v0.9 YAML:

```yaml
# OLD (pre-0.9)
advanced:
  encryption_key: 0123456789abcdef...
```

becomes:

```yaml
# NEW (v0.9+)
storage:
  backend_encryption:
    mode: aes256-gcm-proxy
    key: "${env:DGP_ENCRYPTION_KEY}"   # move the hex to the env
```

With `DGP_ENCRYPTION_KEY` in the environment unchanged.

**On-disk objects are unaffected.** v0.9 reads pre-v0.9 encrypted objects without ceremony — the wire format didn't change, only the config location. The `dg-encryption-key-id` stamp is new as of v0.9 but optional — historical objects without the stamp still decrypt as long as the key material matches.

**If you had no encryption configured pre-0.9:** nothing to do. Your backends default to `encryption: none`.

**If you want to move from proxy-AES to SSE-KMS as part of the upgrade:** do the upgrade first (keep your existing key), then configure the decrypt-only shim to migrate. See [Encryption reference](../reference/encryption.md).

## Verify

After any upgrade or migration:

- [ ] `/_/health` returns HTTP 200.
- [ ] `/_/api/whoami` (with an admin session cookie — anonymous callers do not get `version`) reports the expected `version`.
- [ ] An existing object downloads byte-identical: `aws s3 cp s3://releases/known-file ./out && sha256sum out` matches the known checksum.
- [ ] The admin UI logs in with the bootstrap password (or OAuth) on the first try.
- [ ] `/_/admin/diagnostics/audit` shows recent entries — the audit ring is populating.
- [ ] Prometheus scrape returns valid metrics (if monitoring is wired up).

## Related

- [How to back up and restore](back-up-and-restore.md) — the backup you take in step 1
- [Configuration reference](../reference/configuration.md) — the complete YAML field reference
- [CLI reference](../reference/cli.md) — `config lint` exit codes
- [Admin API reference](../reference/admin-api.md) — Full Backup export/import
