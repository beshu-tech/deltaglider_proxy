# How to upgrade the proxy

This guide shows you how to move between DeltaGlider Proxy versions safely, including the one-time TOML-to-YAML config conversion (mandatory before v1.4.1) and the v0.9 encryption-config change. To go from 1.19 to 2.0, follow [How to upgrade to 2.0](upgrade-to-2-0.md) first, because that release has steps that you must do before the new version starts.

## Standard upgrade workflow

The proxy is a single stateful binary. Every upgrade has three steps: back up, swap, and verify.

1. **Back up first.** In the admin UI, open **System → System** (`/_/admin/system`), and click **Download backup** in the backup card at the bottom of the page.

   ![The backup card at the bottom of the System page; the box marks the Download backup button.](/_/screenshots/backup-download.webp)

   Or send a request to the API:

   ```bash
   curl -b /tmp/admin.cookies \
     "https://s3.acme.example/_/api/admin/backup" \
     -o dgp-backup-$(date +%Y%m%d-%H%M%S).zip
   ```

   The zip is atomic, and the proxy verifies its sha256 when you restore it. See the [Admin API reference](../reference/admin-api.md). Store the zip in a place that the upgrade itself cannot damage.

2. **Roll the image/binary.** For Docker:

   ```bash
   docker pull beshultd/deltaglider_proxy:2.0.2
   docker stop dgp && docker rm dgp
   docker run -d --name dgp -p 9000:9000 \
     -v dgp-data:/data \
     -e DGP_BOOTSTRAP_PASSWORD_HASH=... \
     beshultd/deltaglider_proxy:2.0.2
   ```

   Coolify, Kubernetes, and systemd have their own commands to pull and restart. The only requirement is that `/data` persists across the swap.

3. **Verify.** Run four checks:

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

4. **If something broke**, the backup zip from step 1 imports atomically. In the admin UI, click **Restore backup** in the same card, select the zip, and choose what to restore. Or send it to the API:

   ```bash
   curl -b cookies -X POST \
     -H "Content-Type: application/zip" \
     --data-binary @dgp-backup-...zip \
     https://s3.acme.example/_/api/admin/backup
   ```

   Without a `mode` parameter, the API restores everything, including the admin password of the backup. When the config file is read-only, a restore that includes the config answers `409 config_file_read_only`. [How to back up and restore](back-up-and-restore.md) lists the modes.

## Version compatibility

Patch and minor upgrades inside one major version (for example from 1.18 to 1.19) need no extra steps. Read the [changelog](../changelog.md) entries of the versions that you skip, because a minor release can still change a default.

Schema migrations of the config DB run automatically on the first start. The config DB is on schema v29 in 2.0. A migration is one-way: a binary refuses a config DB whose schema is newer than the one it knows, so an older binary cannot open a database that a newer binary migrated.

A major upgrade (for example from 1.19 to 2.0) has steps that you must do before the new version starts. Follow the upgrade guide of that version ([How to upgrade to 2.0](upgrade-to-2-0.md)) and the [release notes](https://github.com/beshu-tech/deltaglider_proxy/releases), and always export a Full Backup before you start.

## TOML to YAML migration (mandatory before v1.4.1)

**TOML support was removed in v1.4.1.** A v1.4.1+ proxy refuses to start when its config is a `.toml` file (named by `DGP_CONFIG` / `--config` or found on the default search path). The error is `TOML configs are no longer supported (removed in v1.4.1)`. There is no deprecation warning anymore, and the `config migrate` subcommand is gone from v1.4.1+ binaries.

To convert the config once, run `config migrate` on v1.4.0 (the last release that ships it), point the server at the YAML file, and verify it. Only then upgrade to v1.4.1+.

### One-liner (most installs)

```bash
# On a v1.4.0 binary — config migrate no longer exists in v1.4.1+
deltaglider_proxy config migrate \
  /etc/deltaglider_proxy/config.toml \
  --out /etc/deltaglider_proxy/config.yaml
```

Point the server at the new file (with the `--config` flag, the `DGP_CONFIG` env var, or the standard search path). Restart the server and verify it. Then upgrade.

### Step-by-step

**1. Run the migrator (on v1.4.0).**

```bash
deltaglider_proxy config migrate /etc/deltaglider_proxy/config.toml \
  --out /etc/deltaglider_proxy/config.yaml
```

Without `--out`, the migrator writes the YAML to stdout, and you can pipe it to any destination. The migrator does not expand `${env:NAME}` placeholders. It copies them verbatim.

**2. Inspect the output.** Canonical YAML uses the four-section shape:

```yaml
# fragment
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
```

The `...` values stand for your own values. The session lifetime of the admin UI is not in the file: it is the environment variable `DGP_SESSION_TTL_HOURS` (default `4`). The proxy refuses an unknown key, so do not add it to `advanced`.

The migrator keeps the SigV4 credentials (`access.access_key_id` / `secret_access_key` and the storage backend credentials), so you can use the output as it is. It strips only the infrastructure secrets (the bootstrap password hash and any encryption keys). Supply those again with env vars (see step 5).

**3. Validate before applying.** The `config lint` subcommand parses and validates the file without a request to the server:

```bash
deltaglider_proxy config lint /etc/deltaglider_proxy/config.yaml
# Exit: 0 = valid, 3 = I/O, 4 = parse, 6 = validation
```

Add this command to CI, so that CI catches drift in a pull request.

**4. Point the server at the new file.** File search order (first match wins):

1. `DGP_CONFIG` env var
2. `./deltaglider_proxy.yaml`
3. `./deltaglider_proxy.yml`
4. `./deltaglider_proxy.toml` (tripwire: startup fails on v1.4.1+)
5. `/etc/deltaglider_proxy/config.yaml`
6. `/etc/deltaglider_proxy/config.yml`
7. `/etc/deltaglider_proxy/config.toml` (tripwire: startup fails on v1.4.1+)

If you keep both `.toml` and `.yaml` in the same directory, `.yaml` wins. Still, delete the `.toml` after you verify the YAML file. On v1.4.1+, a leftover TOML that the search matches first stops the startup, and the proxy does not ignore it silently.

**5. Feed the stripped secrets back in.** The migrator strips only the infrastructure secrets:

- Move `advanced.bootstrap_password_hash` to the `DGP_BOOTSTRAP_PASSWORD_HASH` env var. The base64-wrapped form avoids `$` escaping issues in Docker.
- Move the per-backend encryption keys to `DGP_ENCRYPTION_KEY` (singleton backend) or `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` (named backends).

OAuth `client_secret` values live in the encrypted config DB, not in the YAML, so the migration does not touch them.

**6. Delete the old TOML and upgrade.** Once the v1.4.0 proxy runs cleanly from the YAML file, remove the `.toml` (it would trip the v1.4.1+ startup check) and roll to the new version.

## The S3-synced IAM database

The IAM database is separate from the YAML config. `deltaglider_config.db` (SQLCipher-encrypted SQLite) holds users, groups, OAuth providers, and mapping rules. The YAML config never carries IAM state (unless you run [declarative IAM](../reference/declarative-iam.md)).

When upgrading across instances with `DGP_CONFIG_SYNC_BUCKET` set, the *newer* binary uploads after any mutation; *older* binaries (still running during a rolling upgrade) refuse a database with a newer schema, so they do not see the change. A newer binary reads the schema version of a synced database before it migrates it. It merges a copy from an older release after it migrates that copy, and the rows of that copy have an unknown change time, so a conflict with them goes to the copy in the bucket. Either:

- Upgrade all instances before you make IAM changes, or
- Accept that the instances on the older release lose the IAM changes of the rollout until they upgrade too.

## Upgrade to the separate config DB key

Earlier releases encrypted `deltaglider_config.db` with the bootstrap password hash. Current releases use a separate key: `DGP_CONFIG_DB_KEY`, or the key file `deltaglider_config.db.key` next to the database. The first start after the upgrade re-encrypts the database from the hash to the new key. That step works on a copy of the database, and the copy replaces the original only after it opens with the new key.

- **One instance:** nothing to do. The proxy generates the key file on the first start and re-encrypts the database with it. Back up the key file together with the database from then on.
- **Several instances with `config_sync_bucket`:** generate one key (`openssl rand -hex 32`) and set it as `DGP_CONFIG_DB_KEY` on every instance before you start the new release. An instance with a sync bucket but without the variable refuses to start. Upgrade all instances together and avoid IAM changes during the rollout, because instances on the old release cannot read uploads under the new key. During the rollout, also set `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC=true` on every instance. Without that variable, the new release refuses a synced database that an old instance wrote under the hash, because the hash is not a secret: it is in configuration files and backups, and anyone who can write to the bucket and knows the hash could plant an IAM database. Remove the variable and restart when every instance runs the new release.
- **Kubernetes operator with `bootstrapPassword.autoGenerate`:** the operator adds a `dbKey` to the `<name>-bootstrap` Secret and injects it into every pod. Without `autoGenerate`, add `DGP_CONFIG_DB_KEY` to your env Secret; the operator refuses to scale beyond one pod without it.
- **Before the upgrade, keep the hash that encrypted the database.** The first start needs it once, to open the database. After the upgrade, the hash is only the admin password, and a change of the password no longer touches the database.

## Common gotchas

- **`$` in Docker env.** Bcrypt hashes contain `$`. Use the base64-wrapped form (`DGP_BOOTSTRAP_PASSWORD_HASH=JDJ5JDEyJGV...`) or single-quote the value in compose files.
- **`force_path_style`.** MinIO needs `true`; AWS S3 needs `false`. The migrator preserves whatever the TOML had.
- **Implicit defaults.** Fields absent from YAML take their default. Do not copy fields that were already at their default in TOML, because they clutter the canonical shape.
- **Request rule order.** The order of the request rules (`admission.blocks`) matters. The migrator preserves order; review `admission:` carefully.
- **`iam_mode: declarative`.** YAML becomes authoritative for IAM users, groups, OAuth providers, and mapping rules. Admin-API IAM mutations return 403; `/config/apply` reconciles the encrypted DB to YAML atomically. Seed from an existing DB with `GET /_/api/admin/config/declarative-iam-export`, or author IAM directly in YAML.

## v0.9: per-backend encryption (breaking)

v0.9 replaced the single global `advanced.encryption_key` field with per-backend encryption blocks. If you upgrade from a pre-0.9 pre-release, the proxy no longer recognizes the old field. It silently drops the key from any YAML that still carries the field.

The changes:

| Before (pre-v0.9) | After (v0.9) |
|---|---|
| `advanced.encryption_key: <hex>` | `storage.backend_encryption: { mode: aes256-gcm-proxy, key: <hex> }` (singleton) or `storage.backends[*].encryption: { ... }` (list) |
| `DGP_ENCRYPTION_KEY` env var | Same name for singleton path; `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` for named backends |
| Global `encryption_enabled` in `GET /config` | Per-backend `encryption` summary on each `BackendInfoResponse` |
| Dedicated `EncryptionPanel` page | Subsection inside each backend card on the `BackendsPanel` |

To convert the pre-v0.9 YAML of a single-backend deployment, start from this YAML:

```yaml
# not-proxy-config: pre-0.9 config, which current releases refuse
# OLD (pre-0.9)
advanced:
  encryption_key: 0123456789abcdef...
```

becomes:

```yaml
# validate
# NEW (v0.9+)
storage:
  backend_encryption:
    mode: aes256-gcm-proxy
    key: "${env:DGP_ENCRYPTION_KEY}"   # move the hex to the env
```

Keep `DGP_ENCRYPTION_KEY` in the environment unchanged.

The upgrade does not change the objects on disk. v0.9 reads objects that a pre-v0.9 release encrypted, because the wire format did not change. Only the config location changed. v0.9 adds the `dg-encryption-key-id` stamp, but the stamp is optional. Older objects without the stamp still decrypt when the key material matches.

If you did not configure encryption before v0.9, you have nothing to do. Your backends default to `encryption: none`.

To move from proxy-AES to SSE-KMS as part of the upgrade, do the upgrade first and keep your existing key. Then move each bucket to a new SSE-KMS backend with the migrate job, because the re-encrypt job does not write to SSE backends. The steps are in [Recipe C: migrate from proxy-AES to SSE-KMS](rotate-encryption-keys.md#recipe-c-migrate-from-proxy-aes-to-sse-kms).

## Verify

After any upgrade or migration:

- [ ] `/_/health` returns HTTP 200, and `/_/ready` returns HTTP 200 (it checks the backends and the config DB).
- [ ] `/_/api/whoami` (with an admin session cookie, because anonymous requests do not get `version`) reports the expected `version`.
- [ ] An existing object downloads byte-identical: `aws s3 cp s3://releases/known-file ./out && sha256sum out` matches the known checksum.
- [ ] The admin UI logs in with the bootstrap password (or OAuth) on the first try.
- [ ] `/_/admin/diagnostics/audit` shows recent entries, so the audit ring fills.
- [ ] Prometheus scrape returns valid metrics (if monitoring is wired up).

## Related

- [How to upgrade to 2.0](upgrade-to-2-0.md): the steps for the 1.19 to 2.0 upgrade
- [How to back up and restore](back-up-and-restore.md): the backup you take in step 1
- [Configuration reference](../reference/configuration.md): the complete YAML field reference
- [CLI reference](../reference/cli.md): `config lint` exit codes
- [Admin API reference](../reference/admin-api.md): Full Backup export/import
