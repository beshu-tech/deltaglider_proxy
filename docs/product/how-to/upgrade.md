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
   docker pull beshultd/deltaglider_proxy:2.0.19
   docker stop dgp && docker rm dgp
   docker run -d --name dgp -p 9000:9000 \
     -v dgp-data:/data \
     -e DGP_BOOTSTRAP_PASSWORD_HASH=... \
     beshultd/deltaglider_proxy:2.0.19
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

## Several instances and encryption (the encryption witness)

With `config_sync_bucket` set, this release keeps a witness of each bucket's encryption in the sync bucket, and an instance that disagrees with it refuses writes to that bucket ([How to run multiple instances](run-multiple-instances.md#8-keep-the-encryption-of-each-bucket-the-same-on-every-instance)). This changes the behaviour of clusters that use backend-level encryption only, too:

- Upgrade every instance before you add an `encryption` block to a bucket. An older release ignores the witness.
- A backend key or mode that you change by editing the file or an environment variable and restarting the instances is not published by the restart. After such a rollout, every instance with the new key refuses all writes to that backend until you run `deltaglider_proxy witness publish backend:<name>` once on one of them. A change made with a config apply is published at once and needs nothing else.
- An instance that starts while the sync bucket does not answer refuses writes to every encrypting backend and to every bucket with its own `encryption` block until its first check succeeds.

## Encryption at rest, format v2 (the release after 2.0.14)

This release replaces the format of proxy-side encryption (`aes256-gcm-proxy`). The earlier formats, `aes-256-gcm-v1` and `aes-256-gcm-chunked-v1` (body magic `DGE1`), have no reader and no migration path in this release, because no deployment used them. If a backend or a bucket of yours encrypted objects with an earlier release, those objects fail to read after the upgrade. Decrypt them before the upgrade, for example with a migrate job to a plaintext backend, and encrypt them again after it. Deployments without `aes256-gcm-proxy` have nothing to do for this change. The changes that you can see:

- **Fail closed.** In a bucket that encrypts, the proxy refuses every object that it cannot authenticate, with `500 InternalError`. That includes a plaintext object, unless the `encryption` block has `plaintext_reads: serve` ([why](../explanation/encryption-at-rest.md#why-enabling-isnt-retroactive)).
- **Starting to encrypt.** An apply that makes an existing bucket start to encrypt with `plaintext_reads: refuse` is refused, unless the block sets `plaintext_reads: serve` or the request confirms it (`X-DGP-Confirm-Plaintext-Refusal: <subject>`, `config apply --confirm-plaintext-refusal <subject>`). The admin UI sets `serve` by itself and offers **Refuse unencrypted objects** after the re-encrypt job. A script that turns on encryption through the admin API must add one of the two.
- **Key ids.** A backend key without `key_id` gets the id `fp-` followed by 16 hexadecimal characters of the key's fingerprint. The backend name no longer takes part in the id. One key can have only one id on a backend; the proxy refuses to start with a config that gives one key two ids.
- **Stored metadata.** An encrypted object stores no `dg-md5`, no `dg-encryption-key-id`, and no SHA-256 of its content in clear. It stores `dg-encrypted: aes-256-gcm-chunked-v1` and a `dg-sealed` value instead ([Metadata markers](../reference/encryption.md#metadata-markers)). A script that reads `dg-encryption-key-id` on the raw backend must read the key fingerprint in `dg-sealed` instead.
- **Copies outside the proxy.** An encrypted object that the backend copies to another bucket name or key no longer reads ([Copies outside the proxy](../reference/encryption.md#copies-outside-the-proxy)).
- **Downgrade.** A release before this one refuses to read the objects that this release encrypts. The objects are not damaged, and they read again after you upgrade once more.

With several instances and encryption, stop every instance and start all of them on the new release together. Do not roll the instances one by one, because an instance on the old release cannot read the objects that a new instance writes. Without a sync bucket, nothing stops an old instance from writing in the old format.

With `config_sync_bucket` set, the key check value of the encryption witness is now the key fingerprint, and a generated key id now starts with `fp-`. The witness entries that the old release published therefore differ from what the new release computes, and every new instance refuses writes to the encrypted buckets. When every instance runs the new release, publish the encryption once, on any instance, for each backend and each bucket with its own key:

```bash
deltaglider_proxy witness publish backend:hetzner-fsn1 db-archive --server https://s3.acme.example
```

The **Encryption witness** card on **System → System** lists the subjects that differ, and **Publish all** does the same. After the publish, an instance on the old release disagrees with the witness and refuses writes. Deployments without `aes256-gcm-proxy` can upgrade one instance at a time, as before.

## Storage facts only for readers of the whole folder (the release after 2.0.14)

This change applies to every deployment, with or without encryption. A delta is encoded against the reference of its folder, and that reference can hold data that another user uploaded. The storage facts of an object therefore tell how similar the object is to the other objects in its folder. For this reason, the proxy now sends these facts only to a principal that may read every object in the folder of the key.

The facts are these response headers and metadata keys:

- On the responses to `PutObject`, `CompleteMultipartUpload`, `CopyObject` and a browser form upload: the headers `x-amz-storage-type`, `x-deltaglider-stored-size`, `x-deltaglider-ref-sha256` and `x-deltaglider-baseline`.
- On the responses to `HeadObject` and `GetObject`, and in each entry of a `metadata=true` LIST: the delta keys `x-amz-meta-dg-ref-sha256`, `x-amz-meta-dg-delta-size`, `x-amz-meta-dg-ref-path`, `x-amz-meta-dg-delta-cmd` and `x-amz-meta-dg-note`. Objects that an older toolchain wrote can also carry `x-amz-meta-dg-ref-key` and `x-amz-meta-dg-source-name`, and the proxy treats them in the same way.

A principal may read the whole folder when one of its Allow rules without conditions grants read on a pattern that ends in `*` and covers the folder (for example `releases/*`), and no Deny rule on reads can match a key in the folder. A principal that may only write, or that may read only some keys of the folder, now gets none of these facts. A reader without credentials, such as a reader of a public prefix or the holder of a presigned URL, also gets none of them.

Two kinds of clients see no change. With `authentication: none`, every request still gets the facts. A principal with read on the whole bucket, or on the whole folder of the key, also still gets them. If a script or a CI job reads one of these headers, make sure that its credentials have read on the whole folder. The full rule is in [Storage facts on write responses](../reference/s3-api-compatibility.md#storage-facts-on-write-responses).

## Common gotchas

- **`$` in Docker env.** Bcrypt hashes contain `$`. Use the base64-wrapped form (`DGP_BOOTSTRAP_PASSWORD_HASH=JDJ5JDEyJGV...`) or single-quote the value in compose files.
- **`force_path_style`.** MinIO needs `true`; AWS S3 needs `false`. The migrator preserves whatever the TOML had.
- **Unknown bucket fields.** A bucket policy refuses a field that the proxy does not know (see [Bucket policies](../reference/configuration.md#bucket-policies)). Before you upgrade, run `deltaglider_proxy config lint` with the new binary on your config file. A misspelled bucket field, which an older release ignored, now stops the start with an error that names the field.
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

The upgrade to v0.9 did not change the objects on disk. Only the config location changed. Current releases no longer read the objects that v0.9 to 2.0.14 encrypted (see [Encryption at rest, format v2](#encryption-at-rest-format-v2-the-release-after-2014)).

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
