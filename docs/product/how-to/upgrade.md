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
   docker pull beshultd/deltaglider_proxy:2.2.0
   docker stop -t 60 dgp && docker rm dgp
   docker run -d --name dgp -p 9000:9000 \
     -v dgp-data:/data \
     -e DGP_BOOTSTRAP_PASSWORD_HASH=... \
     beshultd/deltaglider_proxy:2.2.0
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

## Encryption at rest, format v2 (2.0.15)

This release replaces the format of proxy-side encryption (`aes256-gcm-proxy`). The earlier formats, `aes-256-gcm-v1` and `aes-256-gcm-chunked-v1` (body magic `DGE1`), have no reader and no migration path in this release, because no deployment used them. If a backend or a bucket of yours encrypted objects with an earlier release, those objects fail to read after the upgrade. Decrypt them before the upgrade, for example with a migrate job to a plaintext backend, and encrypt them again after it. Deployments without `aes256-gcm-proxy` have nothing to do for this change. The changes that you can see:

- **Fail closed.** In a bucket that encrypts, the proxy refuses every object that it cannot authenticate, with `500 InternalError`. That includes a plaintext object, except the plaintext history that an open conversion window serves ([why](../explanation/encryption-at-rest.md#why-enabling-isnt-retroactive)).
- **Starting to encrypt.** An apply that makes a bucket with objects start to encrypt opens a conversion window and starts the re-encrypt job. Release 2.1.0 introduced this. See [The conversion window replaces a read setting](#the-conversion-window-replaces-a-read-setting-210).
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

## Storage facts only for readers of the whole folder (2.0.15)

This change applies to every deployment, with or without encryption. A delta is encoded against the reference of its folder, and that reference can hold data that another user uploaded. The storage facts of an object therefore tell how similar the object is to the other objects in its folder. For this reason, the proxy now sends these facts only to a principal that may read every object in the folder of the key.

The facts are these response headers and metadata keys:

- On the responses to `PutObject`, `CompleteMultipartUpload`, `CopyObject` and a browser form upload: the headers `x-amz-storage-type`, `x-deltaglider-stored-size`, `x-deltaglider-ref-sha256` and `x-deltaglider-baseline`.
- On the responses to `HeadObject` and `GetObject`, and in each entry of a `metadata=true` LIST: the delta keys `x-amz-meta-dg-ref-sha256`, `x-amz-meta-dg-delta-size`, `x-amz-meta-dg-ref-path`, `x-amz-meta-dg-delta-cmd` and `x-amz-meta-dg-note`. Objects that an older toolchain wrote can also carry `x-amz-meta-dg-ref-key` and `x-amz-meta-dg-source-name`, and the proxy treats them in the same way.

A principal may read the whole folder when one of its Allow rules without conditions grants read on a pattern that ends in `*` and covers the folder (for example `releases/*`), and no Deny rule on reads can match a key in the folder. A principal that may only write, or that may read only some keys of the folder, now gets none of these facts. A reader without credentials, such as a reader of a public prefix or the holder of a presigned URL, also gets none of them.

Two kinds of clients see no change. With `authentication: none`, every request still gets the facts. A principal with read on the whole bucket, or on the whole folder of the key, also still gets them. If a script or a CI job reads one of these headers, make sure that its credentials have read on the whole folder. The full rule is in [Storage facts on write responses](../reference/s3-api-compatibility.md#storage-facts-on-write-responses).

## The conversion window replaces a read setting (2.1.0)

In earlier releases, a bucket that started to encrypt needed `plaintext_reads: serve` in its `encryption` block, so that the proxy served its plaintext history while the re-encrypt job converted it. Under `serve`, the proxy also served an object that someone put on the backend during the conversion. This release replaces the setting with a conversion window. A config write that makes a bucket with objects start to encrypt writes a sealed marker into the bucket and starts the re-encrypt job. While the window is open, clients read only the objects that the backend stored before the write, and the job closes the window when it converted every object ([Plaintext objects in a bucket that encrypts](../reference/encryption.md#plaintext-objects-in-a-bucket-that-encrypts)).

The changes that you can see:

- **`plaintext_reads` is removed.** The proxy no longer knows the field, with `serve` or with `refuse`. A configuration that still has it fails to load, and the error names the field: the proxy does not start with such a file, `config lint` and `config apply` fail, and a section or document apply answers `400`. The fix is to delete the line from each `encryption` block, as the steps below describe.
- **The confirmation is gone.** The apply that started encryption on a bucket with objects no longer answers `422`. The header `X-DGP-Confirm-Plaintext-Refusal` is ignored, and `config apply --confirm-plaintext-refusal` is an unknown option, so a script that passes it fails. Remove both from your scripts.
- **A new `409`.** The same apply is refused with `409` when another job, for example a migration, holds one of the buckets. Wait for that job, or cancel it, and apply again.
- **The admin API.** The bucket list and the backend summary no longer have `serves_plaintext`. The bucket list reports the window in `effective_encryption.conversion`, and a config write reports the conversions that it started in `conversions` ([admin API](../reference/admin-api.md#starting-a-conversion)).
- **A metadata backfill waits.** While a bucket holds a conversion marker, `POST /_/api/admin/jobs/backfill-metadata` for that bucket answers `409`.

Do these steps before you start the new release, for each backend and each bucket whose `encryption` block has `plaintext_reads`:

1. On the old release, run the re-encrypt job of each bucket that the block covers, and wait until it ends without failures: `POST /_/api/admin/jobs/reencrypt` with `{"buckets": ["<bucket>"]}`. The job converts the plaintext history while `serve` still serves it, so no read fails during the upgrade.
2. Delete the `plaintext_reads` line from the configuration file. Find every line with `grep -n plaintext_reads deltaglider_proxy.yaml`.
3. Start the new release with the edited file. Upgrade every instance that writes to the bucket: an instance on an older release ignores the window, and it serves and writes plaintext as before.

When a bucket still holds plaintext objects after the upgrade (you skipped step 1, or the job failed for some objects), the proxy refuses to read them, because the bucket encrypts and no window is open. A bucket that had `serve` already holds objects that the proxy encrypted, so it holds a key claim, and a restart opens no window for a bucket with a key claim ([A conversion that a restart starts](../reference/encryption.md#a-conversion-that-a-restart-starts)). Open the window and start the job yourself with `POST /_/api/admin/jobs/reencrypt` and `{"buckets": ["<bucket>"], "open_window": true}`. Clients then read the history that the backend stored before that request while the job converts it. Follow the job until the window closes, as in [How to encrypt data at rest](encrypt-data-at-rest.md#6-encrypt-the-objects-that-the-backend-already-holds).

A restart does start the conversion of a bucket that never encrypted before. When you add `aes256-gcm-proxy` to the config file of a deployment that the Kubernetes operator, a Helm chart or a GitOps pipeline restarts, each bucket with plaintext objects, no key claim and no marker gets its window and its re-encrypt job at the start. The log names the run and the job.

With several instances, apply the change on every instance, and then let the job finish. An instance whose configuration does not encrypt the bucket yet writes plaintext into it, and the instances that encrypt refuse those objects after the cutoff. While the job runs, stop client writes to the bucket on every instance, or send them all to the instance that runs the job: the write gate of the job holds only on its own instance ([A conversion on several instances](../reference/encryption.md#a-conversion-on-several-instances)).

## Encryption needs atomic writes (2.1.0)

Proxy encryption (`aes256-gcm-proxy`) now works only on a backend whose conditional writes the proxy proves with a probe. The proxy rewrites encrypted objects in place, each rewrite conditional on the version that it read, and a backend that ignores the condition could let a rewrite overwrite a newer client write ([Encryption needs atomic writes](../reference/encryption.md#encryption-needs-atomic-writes)). Native SSE modes are not affected.

The changes that you can see:

- **Read-only buckets.** A bucket that encrypts with the proxy on a backend that does not support atomic writes becomes read-only after the upgrade. An example is an encrypted bucket on a Backblaze B2 backend: B2 drops the connection on a conditional `PUT`, and the probe reads that as "does not support". Reads keep working. Every write gets `403 AccessDenied` with a fixed sentence that says that the bucket is read-only and points to the migrate. A replication rule into the bucket shows `deferred` with the reason. The start log has a warning that names the read-only buckets.
- **Writes wait for the check (`503`).** The proxy must verify a backend before it accepts writes to the encrypted buckets on it. At start, it probes every S3 backend that holds such buckets at once, in parallel with the boot health check, and the start waits for these probes up to 3 seconds. A probe ends within 15 seconds. Until the probe of a backend ends, the writes to its encrypted buckets get `503 SlowDown`, which S3 clients retry, and reads are served. So after a restart, the writes to an encrypted bucket on an S3 backend can answer `503` for up to about 15 seconds. A filesystem backend needs no probe, so its buckets never wait. When the probe cannot reach the backend, the writes keep answering `503` and the proxy probes again after 5 seconds, doubling to every 60 seconds. `GET /_/ready` shows the state as `atomic_writes` (`checking`, `unverified`, `supported`, `unsupported`), and the state never makes the proxy not ready.
- **New `409` refusals.** A config change, a bucket create, a re-encrypt job (also Resume), a migrate and a restart that would turn proxy encryption on for such a backend are refused with `409` and an `atomic_writes` object.
- **The probe runs on one instance too.** Before, the capability probe ran only with `config_sync_bucket` set. The proxy now probes, in the background, every backend that holds a bucket that encrypts with the proxy.
- **A form POST checks the signature first.** A browser form POST into a bucket marked `replication_target_only` now gets its `403` after the signature check, so an unsigned form gets the authentication error instead.

Check your backends before or right after the upgrade:

1. Open **Storage → Backends** (`/_/admin/storage/backends`), or send `GET /_/api/admin/backends`. A backend whose `capability.verdict` is `non-cas` lists its read-only buckets in `read_only_buckets`, and its card shows that they are read-only. A backend whose verdict is `unknown` says that the proxy could not verify it; the writes to its encrypted buckets answer `503` until a probe succeeds.
2. For an `unknown` verdict, fix the connection to the backend. The proxy probes it again by itself, at once when its health check sees the backend recover. To probe at once, click **Check again** on its card (`POST /_/api/admin/backends/<name>/capability-probe`).
3. For a `non-cas` verdict, move each read-only bucket to a backend with atomic writes with a migrate job: [Make a read-only bucket writable again](backend-capability-validation.md#make-a-read-only-bucket-writable-again). The move out of a read-only bucket is allowed.

## A rule on `bucket/*` no longer allows CreateBucket or DeleteBucket (2.1.0)

A bucket-level request, such as a LIST request or `CreateBucket`, has no object key, so a rule with the resource `releases/*` does not match it on its own. In earlier releases, the proxy then checked the request again with the resource `releases/`, which `releases/*` matches, for every action. So a user whose only rule was `{"actions": ["*"], "resources": ["releases/*"]}` could also create and delete the bucket `releases`. This release makes that second check for every action except `admin`.

The changes that you can see:

- **`CreateBucket` and `DeleteBucket` get `403`** for a user whose rules name only `releases/*`. The same applies to the other bucket-level `PUT` and `DELETE` requests, which need `admin`.
- **The other bucket-level requests still work.** A LIST request, a batch delete (`POST ?delete`) and a browser form upload still need only a rule on `releases/*` with `list`, `delete` or `write`.

When a user or a group must create or delete a bucket, add a rule whose resources hold the bucket name itself, for example `{"effect": "Allow", "actions": ["admin"], "resources": ["releases"]}`, or use `*` ([Resources](../reference/iam-permissions.md#resources)).

## A replication destination that contains its source (2.1.0)

A replication rule must not write into a prefix that contains its own source. An example is the rule `releases/builds/nightly/` → `releases/builds/`. The destination `builds/` holds the folder `builds/nightly/`, so the rule finds its own source objects in the destination. These objects have no twin at the source side, so a rule with delete replication deleted them, and its next run deleted the copies. Two bucket names that an `alias` points at one real bucket on one backend count as one storage, so a rule from `releases` to such a second name can have the same problem.

The changes that you can see:

- **Such a rule does not run.** The proxy still starts with a configuration that holds one, and it logs a warning that names the rule. Every run of the rule fails at its start with a failure entry, and the event consumer sends nothing for it.
- **`config lint` reports such a rule as an error** (exit code `6`), and a config change that makes a new one is refused. A bucket change, such as a new `alias`, that makes one is refused too.

Before you upgrade, run `deltaglider_proxy config lint deltaglider_proxy.yaml` with the new binary. For each rule that it reports, give the rule a destination prefix outside its source, or another bucket ([Replication reference](../reference/replication.md#static-validation-configcheck)).

## A migrate of an aliased bucket (2.1.0)

A migrate job blocks writes to the bucket that it moves, and it copies only that bucket. When another bucket name points at the same storage through an `alias`, the writes through that name were not blocked and not copied, and the cleanup deleted them. The proxy now refuses to start such a job: `POST /_/api/admin/buckets/:bucket/migrate` answers `409 Conflict`, and the error names the other bucket names. Remove the alias before you move the bucket ([How to move a bucket to another backend](move-a-bucket-between-backends.md)).

## The Kubernetes operator (2.1.0)

Operator 0.3.24 and later need a new ClusterRole and a new custom resource definition:

- The ClusterRole allows `patch` on Secrets. The operator adds the config DB key (`dbKey`) to an existing `<name>-bootstrap` Secret that does not have it. With the old ClusterRole, that request got `403`, so every reconcile failed and the operator changed nothing in the cluster.
- The ClusterRole allows the operator to manage NetworkPolicies, for the new `spec.router.networkPolicy` field.
- The custom resource definition refuses a change of `spec.storage` after you create the resource, because a StatefulSet refuses a change to its volume claim templates.

Apply the new `operator/deploy/crd.yaml` first, and then the new `operator/deploy/operator.yaml`. The second file holds the ClusterRole and the operator image, so you get them together ([How to scale out with the Kubernetes operator](scale-out-with-the-kubernetes-operator.md#1-install-the-operator)).

## Every upload must declare its length (2.2.0)

The proxy now reserves memory or spool space for an upload body before it reads the body, so it must know the length of the body first. A `PutObject` request that declares no length gets `411 MissingContentLength`, as on S3. A request declares the length in `Content-Length`, or, for an aws-chunked body, in `x-amz-decoded-content-length`. A body that is sent with plain `Transfer-Encoding: chunked` declares neither. The AWS SDKs and the usual S3 tools always send the length, so only a hand-written client can be affected.

A browser form upload (`POST /{bucket}` with `multipart/form-data`) changes in two ways:

- **The form must declare its length** in `Content-Length`, or it gets `411 MissingContentLength`.
- **Every field that the upload needs must come before the file.** The proxy reads the form as a stream and ignores the fields after the file, as S3 does. A form that sends `key`, `policy`, `x-amz-signature` or `x-amz-meta-*` after the file now fails, or stores the object without that metadata. The AWS SDKs and the embedded browser put the file last.

A field other than the file can be at most 64 KiB, else the form gets `400 InvalidArgument`. The same refusal applies on each side of the file separately when its fields, with their names, hold more than 1 MiB, or when it has more than 1,000 fields. See [How the proxy reads an upload body](../reference/s3-api-compatibility.md#how-the-proxy-reads-an-upload-body).

## Size the spool for large uploads (2.2.0)

Before this release, the proxy read the whole body of a `PutObject` request into memory. It now reads a body larger than `DGP_SPOOL_THRESHOLD_BYTES` (16 MiB by default) into a file in the spool directory (`DGP_SPOOL_DIR`), and it reads a smaller body into memory while the upload memory budget (`DGP_INGEST_MEMORY_MAX_BYTES`, 1 GiB by default) has room for it. Every multipart part now goes into a relay file in the spool too, also for an upload that can become a delta. So concurrent large uploads now use disk instead of memory. Check these points before you upgrade:

1. **Put `DGP_SPOOL_DIR` on a real disk.** Each large upload holds about its own size in the spool until it is stored, and twice that in a bucket that the proxy encrypts, because the encrypted copy is written to the spool too. Do not use a tmpfs or a Kubernetes `emptyDir` without a size limit, because those use memory or can fill the disk of the node.
2. **Set `DGP_SPOOL_MAX_BYTES` (default 16 GiB in 2.2.0; `auto` since 2.2.1, see [the spool and upload limits follow the disk](#the-spool-and-upload-limits-follow-the-disk-221)) for the large uploads that arrive at the same time.** An upload whose declared size is larger than this budget gets `400 EntityTooLarge` before its body is read, and the message names `DGP_SPOOL_MAX_BYTES`. In a bucket that the proxy encrypts, the body and its encrypted copy must fit together, so the largest upload there is about half of the budget. A copy of a large delta object holds the spool file of its source while it writes the destination, so it needs about twice its size.
3. **Check your largest multipart object.** One multipart upload can hold at most `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` of spool space, which is half of `DGP_SPOOL_MAX_BYTES` by default (8 GiB with the default budget). A larger multipart object gets `400 EntityTooLarge`, with a message that names the variable. Raise `DGP_SPOOL_MAX_BYTES`, or set `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES`, if you upload larger objects. A single part larger than 5 GiB (the S3 limit) or larger than `max_object_size` now gets `400 EntityTooLarge` before the proxy reads it.
4. **Check the total of open multipart uploads.** The default of `DGP_MAX_TOTAL_MULTIPART_BYTES`, the spool space that all open uploads may hold together, is now half of `DGP_SPOOL_MAX_BYTES` instead of `max_object_size × DGP_MAX_MULTIPART_UPLOADS / 4`. With the default spool of 16 GiB, the total drops to 8 GiB (it was 2,500 GiB with a `max_object_size` of 10 GiB). So two concurrent uploads of 5 GiB each can get `503 SlowDown` on their later parts, and an upload that a client abandons holds its share until the idle sweep removes it, which can take up to about 65 minutes. Size the spool for the uploads that are open at the same time, or set `DGP_MAX_TOTAL_MULTIPART_BYTES`.
5. **Give the spool a directory of its own.** The proxy creates `DGP_SPOOL_DIR` with mode `0700`. At startup, it sets an existing directory that only its group can write to (one made under umask `002`) to `0700`. A directory that another user owns, or that all users can write to (such as `/tmp` itself), now stops the proxy at startup with an error that names it.

A large upload that finds the spool budget full waits for space, up to `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS`, before the proxy reads its body. That wait counts toward the request timeout of 300 seconds. The [capacity planning reference](../reference/capacity-planning.md#memory) shows how to size the memory and the spool disk.

## Kubernetes: the spool, the update strategy and the stop (2.2.0)

- **The spool budget follows the size of `/tmp`.** The spool lives in `/tmp`, which is an `emptyDir`, and the kubelet evicts a pod whose `emptyDir` grows past its size limit. So the Helm chart now sets `DGP_SPOOL_MAX_BYTES` to three quarters of `tmp.sizeLimit` (1.5 GiB with the default `2Gi`), and `tmp.sizeLimit` must now be given in `Mi` or `Gi`. Raise `tmp.sizeLimit` for larger or more concurrent uploads ([Size `/tmp` for the spool](deploy-on-kubernetes.md#size-tmp-for-the-spool)). The operator gives `/tmp` a size limit of 8 GiB (it had none) and sets `DGP_SPOOL_MAX_BYTES` to 6 GiB.
- **One process per state directory.** The proxy now takes a lock on the folder of `deltaglider_config.db`. A second process on the same folder waits up to `DGP_STATE_LOCK_WAIT_SECS` (default 60 seconds) and then exits with a message that names the folder. A rolling update starts the new pod while the old pod still runs, so a deployment with one shared volume needs `updateStrategy.type: Recreate`. The Helm chart now sets it. Or give each replica its own volume.
- **Set `terminationGracePeriodSeconds` to at least 60.** A stop now takes up to 40 seconds: up to 10 seconds each for the running requests, a running maintenance job, the event outbox, and the work on the blocking threads (`DGP_SHUTDOWN_TIMEOUT_SECS`, default 10). The Helm chart sets 60. With Docker, stop the container with `docker stop -t 60`, or set `stop_grace_period: 60s` in a compose file.

## More reasons for a boot to fail (2.2.0)

- **A locked state directory.** A second process on the same state directory exits after the wait of `DGP_STATE_LOCK_WAIT_SECS`, as the previous section describes. Outside Kubernetes, this happens when a systemd unit or a container starts the new process before the old one stopped, or when two proxies point at the same directory.
- **A bootstrap password hash that the proxy cannot keep.** A proxy without a terminal that generates its bootstrap password, and cannot write the hash to `.deltaglider_bootstrap_hash`, now exits, because the admin password would be lost at the next start. Make the working directory of the proxy writable, or set `DGP_BOOTSTRAP_PASSWORD_HASH`.
- **A `0` in a count that cannot be zero.** A `0` in `advanced.blocking_threads`, `advanced.codec_concurrency` or `advanced.filtered_list_max_engine_pages` (or in `DGP_BLOCKING_THREADS`, `DGP_CODEC_CONCURRENCY` or `DGP_FILTERED_LIST_MAX_ENGINE_PAGES`) now stops the proxy at startup with an error that names the setting. Before, `blocking_threads: 0` made the proxy panic at startup, `codec_concurrency: 0` answered every delta PUT with `503`, and `filtered_list_max_engine_pages: 0` made every filtered LIST fail. Remove the `0`. Other `DGP_*` timeouts and counts that a `0` would break, for example `DGP_REQUEST_TIMEOUT_SECS`, `DGP_MAX_CONCURRENT_REQUESTS`, `DGP_CLOCK_SKEW_SECONDS` and `DGP_SESSION_TTL_HOURS`, are now read as `1`.

## Readiness and the coordination bucket (2.2.0)

`GET /_/ready` now answers `503` in two more cases, because the S3 API of such a node cannot serve requests. The first is a config DB that no key opens: the body then says `config_db: "key_mismatch"`. The second is a spool directory in which the proxy cannot make a file: the new `spool` field then says `unusable` instead of `ready`. A Kubernetes Service sends no traffic to a node that answers `503`, so reach the recovery wizard of such a node through `kubectl port-forward pod/<name> 9000:9000` ([Admin API reference](../reference/admin-api.md)).

A proxy with a `config_sync_bucket` used to stop when its boot probe could not reach the coordination bucket. It now starts, but it does not write to that bucket until a background probe proves that the bucket supports atomic conditional writes: the lease, the reference lock, the encryption witness and the upload of the synced config DB wait. In that time, a delta PUT answers `503 ServiceUnavailable`, because it needs the reference lock, and a write to a bucket with an encryption witness answers `503 SlowDown`. When a probe proves the bucket, these writers open without a restart. After two probes in a row that prove that the bucket has no atomic conditional writes, the proxy drains its requests and stops with exit code 1 ([Backend capability validation](backend-capability-validation.md)).

## Event delivery sends more per tick (2.2.0)

`advanced.event_delivery.batch_size` used to be the most events that one tick claimed. Now a tick claims one batch of `batch_size` rows after the other, for as long as the last batch was full and more than half of its rows were delivered, and for up to one `tick_interval`. Raw webhooks get up to 8 posts at the same time. So if you set a small `batch_size` with a long `tick_interval` to protect a slow receiver, that receiver now gets continuous traffic while many events wait. Set a lower `batch_size`, and put a rate limit in front of the receiver if it cannot take this load ([Event log reference](../reference/event-outbox.md)).

## Outbound requests and the SSRF guard (2.2.0)

- **Webhook, Slack and OIDC requests no longer use an environment proxy.** These requests ignore `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`, and they connect directly. A deployment that reaches a webhook, Slack or an identity provider only through an egress proxy must allow a direct route to those hosts, or filter the traffic on the network instead.
- **An S3 endpoint name that resolves to a loopback address needs `allow_local: true`.** This applies, for example, to an `https` endpoint that reaches a TLS sidecar or a tunnel through `/etc/hosts` or `hostAliases`. Without `allow_local: true`, every request to that backend fails with "SSRF guard: … resolved only to refused address(es)". When another backend answers, the proxy still starts; when that backend is the only one, the boot backend probe (`DGP_BOOT_BACKEND_PROBE`, default `enforce`) stops the proxy at startup. Set `allow_local: true` on that backend. Link-local addresses and the cloud metadata endpoints are refused, also with `allow_local: true`.

## The jobs database keeps `-wal` and `-shm` files (2.2.0)

`deltaglider_jobs.db` now runs in write-ahead-log mode, so next to it the proxy keeps `deltaglider_jobs.db-wal` and `deltaglider_jobs.db-shm` while it runs. The newest commits can be only in the `-wal` file, so a copy of `deltaglider_jobs.db` alone can miss them, and a file copy while the proxy runs can be inconsistent. Back up the three files with the proxy stopped, or take a snapshot of the whole volume at one instant ([How to back up and restore](back-up-and-restore.md)). The proxy user must be able to create these two files in the folder of the database.

The jobs database also commits without waiting for the disk, except for the commits that an effect which cannot be undone depends on, such as the claim of an outbox batch before the posts. So a power loss can lose the last job commits: a lost append to the outbox is a lost event, and a lost settle is a delivery that the proxy sends again.

## The session cookie has a new name over HTTPS (2.2.0)

Where the admin session cookie is `Secure` (TLS at the proxy, a trusted `X-Forwarded-Proto: https`, or `DGP_SECURE_COOKIES=true`), its name is now `__Host-dgp_session`. Where the cookie is not `Secure`, the name stays `dgp_session`. An explicit `DGP_SECURE_COOKIES` decides in both directions. Sessions live in memory, so every session ends at the restart of the upgrade anyway, and users sign in again. Only a tool that reads the cookie by its name, such as a script or a load balancer rule, needs the new name ([How to serve TLS](serve-tls.md)).

## A coordination bucket that the backend refuses stops the boot (2.2.1)

The boot probe of the coordination bucket (`config_sync_bucket`) separates two kinds of failure. When the backend answers the probe's first write with an S3 error that no retry changes (`NoSuchBucket`, `InvalidAccessKeyId`, `SignatureDoesNotMatch`, `AccessDenied` and similar), the proxy exits with code 1 and a message that names the bucket and `config_sync_bucket`. Fix the bucket name or the backend credentials, then start the proxy again. A bucket or an access key that was made a moment ago can take time to reach the whole store, so for `NoSuchBucket`, `InvalidAccessKeyId`, `SignatureDoesNotMatch` and `AccessDenied` the proxy tries again every 5 seconds for up to 30 seconds before it exits, and logs each try.

An answer that a later attempt can change does not stop the boot: no answer (a timeout or a connection error), a `5xx`, a throttling answer, a `408` or `429`, an expired token, a redirect that a retry resolves, a full store or an account over its quota (`QuotaExceeded`, `507`), and any `3xx` or `4xx` answer without an S3 error body (a gateway or a firewall in front of the store). The proxy then starts and keeps probing in the background. A running proxy never exits because of a later refusal: it stays up, and it logs each change of the verdict, and again about every 10 probes while the bucket stays unproven. The log lines name the bucket and `config_sync_bucket`.

While the probe has no answer, `GET /_/ready` shows `coordination: "unproven"`. This is information only. The node stays ready: reads and passthrough writes work, and a delta write answers a retryable `503`. A restart of every node during a short outage of the store therefore leaves a Service with endpoints.

## IAM users wait for the first good sync (2.2.1)

A node with a `config_sync_bucket` does not know whether its local IAM database is current until one sync ends well: a peer may have removed a user since the last run. Until then, every request that the proxy judges against that database gets `503` (retryable) with a neutral message. These are: a request that signs with an IAM user's key (header, presigned URL and browser form upload), a login with an IAM access key (`login-as`), a browser connect, an OAuth sign-in, and an admin session of an IAM user (it is not an admin session until the database is confirmed). Requests that read no IAM database are served: a public prefix, a CORS preflight, the `HEAD /` probe, a request with the bootstrap key pair on a node that has no IAM users, open access, and an admin session that was opened with the bootstrap password. On a node that has IAM users, the bootstrap key pair is not an S3 credential. A node whose IAM mode is `declarative` takes its users from the config file and is confirmed from the start.

`GET /_/ready` answers `503` with `iam_sync: "unconfirmed"` only while the node refuses IAM users in this way. A node with no IAM users, or with the override below, stays ready. The sync client is built without any request to the store, so a build error always comes from the config: a `config_sync_bucket` on a filesystem backend, a coordination backend without static keys, or an endpoint that the proxy refuses. The proxy then logs a warning that names `config_sync_bucket`, runs with the sync off (as when no bucket is set), and serves the local IAM database. It does not retry, because a retry would repeat the same error. Fix the backend and restart. The poll runs every 5 to 60 seconds until a pull ends well, then every 5 minutes.

A pull that the store answers but that this release cannot use also counts as confirmed, so that a whole fleet is not held out of service by a copy that no retry changes. This is the case for a copy from a newer release (after a downgrade, the copy in the bucket has a newer schema) and for a copy that the proxy refuses as a rollback by its sync generation. A copy from a release before the sync generation existed is judged by the age of its rows, with a tolerance of 5 minutes; that is a guess, so such a refusal leaves the database unconfirmed. The node serves its local IAM database, it logs an error that names the cause (about every 10 minutes), and `GET /_/api/admin/config/sync` shows the pull error. The local database and the bucket are then not in step: upgrade this node again, or restore the bucket from a backup. A copy that cannot be read or opened, a wrong `DGP_CONFIG_DB_KEY` and a store that does not answer leave the database unconfirmed.

To serve the local IAM database before a sync confirms it, set `DGP_IAM_SERVE_UNCONFIRMED=true` and restart. Use it for a single instance that must restart while the sync bucket is down. The trade-off: a user that a peer removed since the last run can still sign in on this node until a sync ends well. `/_/ready` then shows `iam_sync: "unconfirmed_override"` and answers `200`. A good sync ends the unconfirmed state, and the override then has no effect. The proxy logs this variable in a warning at start when the database is unconfirmed.

## An admin password restore ends the other admin sessions (2.2.1)

Every change of the bootstrap hash ends the admin sessions that were opened with the previous password. This includes a password change, a backup restore that brings back another admin password (mode `full`), and the roll-back of a failed restore. The session that makes the change stays signed in. Every other session gets `401` and must sign in again with the current password.

## A backend host that resolves only to refused addresses stops the start (2.2.1)

An S3 endpoint whose name resolves only to loopback needs `allow_local: true` on its backend (or `DGP_BACKEND_ALLOW_LOCAL=true`), and a link-local or metadata address is refused also with `allow_local`. At start the proxy now resolves the endpoint host of every S3 backend, all at once, under a deadline of 5 seconds. When every address of a host is refused, the proxy exits with a message that names the backend, the host and the setting `allow_local`. This includes a backend that is defined but that no bucket uses. A config apply does the same check for the backends that it adds or changes, and refuses the apply. A name that does not resolve in time says nothing here, because DNS can come up later. `allow_local` permits plain `http://` endpoints, and endpoints on private or internal hosts (loopback, RFC 1918 addresses and in-cluster names). It never permits link-local or cloud-metadata addresses.

## Webhook, Slack and OIDC requests can use the system proxy (2.2.1)

By default, webhook, Slack and OIDC requests connect directly and ignore `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`. A deployment whose only way out is an egress proxy can set `DGP_OUTBOUND_USE_SYSTEM_PROXY=true`. The trade-off: the egress proxy resolves the target name, so the proxy cannot check the address that the request reaches. Only the check of the URL text (no literal private, loopback or metadata address) still applies. `NO_PROXY` applies when the option is on, but the check of the address is off for every request of these clients, also for a host that `NO_PROXY` lists and for a host that no proxy serves. Use the option only when the egress proxy filters its own targets. The proxy reads the variable once at start, so restart it after a change.

## The spool directory is private after a re-creation (2.2.1)

When something removes the spool directory while the proxy runs, the proxy makes it again with mode `0700`, whatever the umask of the process.

## Deploys, readiness and jobs (2.2.1)

- **A deploy must stop the old process before it starts the new one.** The proxy keeps a lock on its state directory (the folder of `deltaglider_config.db`). A process that starts while the old one still runs waits up to `DGP_STATE_LOCK_WAIT_SECS` (60 seconds by default) and then exits. Its error now names the process id of the holder when it is known, and says how to fix the deploy. Docker Compose stops the old container before it starts the new one when you run `docker compose up -d` after a change. Some platforms start the new container before they stop the old one (a rolling update). That works while the overlap stays shorter than `DGP_STATE_LOCK_WAIT_SECS`; the new process waits for the lock. If the old container stays longer, raise the wait or make the platform stop the old container first. The file `.deltaglider.holder` next to the lock file records the holder as `pid`, host name and start time (the proxy replaces it by a rename, so it is never half written); the pid is the one inside the holder's container, often 1. On a file system without file locks (some network file systems) the proxy logs a warning and holds no lock, so it cannot tell whether another process uses the directory. On Kubernetes, use `strategy: Recreate` for a Deployment with one shared volume (the Helm chart does). The operator uses a StatefulSet in which every pod has its own volume, and Kubernetes stops a pod before it starts the pod with the same name, so the operator needs no change.
- **`--set-bootstrap-password` refuses while a server runs, when it must re-encrypt the config DB.** A config DB from before the separate key still opens with the old bootstrap hash. The command then re-encrypts the database with the config DB key. A re-encrypt replaces the file, so a running server would keep writing to the old file and lose those writes. The command now takes the same state directory lock as a server and exits with a message when a server holds it. Stop the server, run the command again, and start the server. A database that already opens with the config DB key needs no re-encrypt, so the command works while a server runs.
- **Set the readiness probe `timeoutSeconds` to 5 or more.** `GET /_/ready` now runs its checks at the same time and answers within 3 seconds at the latest, where it ran them one after the other. The Helm chart (`probes.readiness.timeoutSeconds`) and the operator now set 5, because the Kubernetes default of 1 second can fail a healthy pod. If you wrote your own probe, set 5 or more. A database lock or the spool check that is not done after 3 seconds counts as failed. The live backend probe (used when the health loop is off) runs to its end in the background, whatever `DGP_READY_TIMEOUT_SECS` and `DGP_READY_RETRIES` make it take, and `/_/ready` answers from its last verdict while it runs.
- **A locked node says how to reach the recovery wizard.** When no key opens the config DB, `/_/ready` stays `503` so that the Service sends no S3 traffic to a node that answers `503` to every request. The S3 `503` message and the Kubernetes guides now name the way in: `kubectl port-forward pod/<name> 9000:9000`, then open `http://localhost:9000/_/admin`.
- **A migrate job stops when it cannot save its progress.** The job saves its phase with a durable commit before the flip and before it deletes the source. When that save fails (for example because the disk is full) before the flip, the job stops with a transient failure, retries a few times, and then fails and releases the write gate of the bucket. When it fails after the flip, the job waits and runs again later, and the bucket stays write-gated until the jobs database recovers (the log has a warning, and an error at every 10th stop). It went on before, and a power loss after the flip could then resume the job at an earlier phase and delete the data on the target. A job that resumes after the flip now never deletes the staged copies on the target: no unwind runs while the live config routes the bucket to the target.
- **The start-up sweep keeps a file of an older release with metadata.** Releases before 2.0 accepted the key `.tmpAb12Cd` and stored it under that file name. The sweep that removes dead temporary files now removes a file with such a name only when it is older than an hour and has no proxy metadata. A file with such a name and with metadata stays. A foreign file without metadata (not stored by the proxy) that is named `.tmp` and six letters or digits, and is older than an hour, is still removed.

## A stop waits for detached work (2.2.1)

A `DeleteObjects` batch and the store of a `CompleteMultipartUpload` run on after their request ends, so that a client disconnect cannot cut an object in half. When the proxy gets the stop signal, it stops accepting requests and then waits for this work, up to 15 seconds. When the 15 seconds pass, it logs a warning with the number of tasks that it cut, and the clients retry those requests. A stop takes up to 55 seconds: 10 for the running requests, 15 for detached work, 10 for a maintenance job, 10 for the event outbox and `DGP_SHUTDOWN_TIMEOUT_SECS` (default 10) for blocking work. A `terminationGracePeriodSeconds` or `stop_grace_period` of 60 seconds, as the Helm chart, the operator and the compose example set, still holds. Keep it at 60 or more.

## The spool and upload limits follow the disk (2.2.1)

Before this release, `DGP_SPOOL_MAX_BYTES` was 16 GiB and `max_object_size` was 100 MB when you set nothing. Now both default to `auto`, and the proxy derives all the upload limits once, at start, from the free space of the disk that holds the spool directory (`DGP_SPOOL_DIR`). The rule is in [Auto-sized limits](../reference/configuration.md#auto-sized-limits). In short, the spool budget is 50% of the free space (25% when the spool shares its device with a filesystem backend or the data directory, and at least 1 GiB). The largest multipart object and the total of all open multipart uploads are each half of the budget, and `max_object_size` is the lowest of these bounds: the per-upload limit, the multipart total (explicit or derived), and 5 GiB when any configured backend is S3 (an S3 backend stores a client object, also a multipart object, with one PUT request, and that takes at most 5 GiB).

**A deployment with an explicit spool and the default `max_object_size` changes too.** Its `max_object_size` was 100 MiB and is now the derived limit. For example, with the Helm chart default (`tmp.sizeLimit: 2Gi`, so `DGP_SPOOL_MAX_BYTES` is 1.5 GiB), the per-upload limit and `max_object_size` go from 100 MiB to 768 MiB. To keep 100 MiB, set `max_object_size: 104857600` (or `DGP_MAX_OBJECT_SIZE=104857600`).

For example, take a host with 60 GiB of free space in the spool directory:

| Limit | Before | Now |
|---|---|---|
| `DGP_SPOOL_MAX_BYTES` | 16 GiB | 30 GiB |
| `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` (largest multipart object) | 8 GiB | 15 GiB |
| `DGP_MAX_TOTAL_MULTIPART_BYTES` | 8 GiB | 15 GiB |
| `max_object_size` | 100 MB | 15 GiB (5 GiB if any backend is S3) |
| Delta ceiling (objects above it are stored whole) | not a limit before | 10 GiB |

What to check:

1. **Look at the start-up log.** One line starts with `[limits]` and shows the values and where they come from. `GET /_/api/admin/config` shows the same in the field `limits`.
2. **A larger `max_object_size` changes what a client can send.** A single PutObject request with a body of up to the new limit is now accepted, and the proxy reads such a body into the spool directory. Make sure that the disk has room for it, or set `max_object_size` to a number. A request over the limit gets HTTP 400 `EntityTooLarge`.
3. **Check the delta ceiling.** The delta encode holds the body, the reference and the delta output in the spool at once, so an object above a third of the spool budget (the delta ceiling, 10 GiB in the example) is accepted but stored whole, not as a delta, with `dg-passthrough-reason: delta-unavailable`. The boot log line and `limits.delta_ceiling` show it. If you store delta-eligible objects near the ceiling, raise `DGP_SPOOL_MAX_BYTES`.
4. **A tmpfs or a Kubernetes pod without a value gets 1 GiB and a warning.** The free space there does not show the real limit. Set `DGP_SPOOL_MAX_BYTES` to the space that the directory really has. The Helm chart and the operator already set it, so they are not affected.
5. **Pin the old values if you need them.** Set `DGP_SPOOL_MAX_BYTES=17179869184` and `max_object_size: 104857600` (or `DGP_MAX_OBJECT_SIZE=104857600`). An explicit value always wins. A value that you set is never changed, but the proxy logs a warning when the spool budget is above 90% of the free space, or when `max_object_size` is above the lowest of the per-upload limit and the multipart total.
6. **The limits do not change while the proxy runs, and every start measures again.** The proxy measures the disk once, at start. A fuller disk at a restart gives smaller limits; reads of objects that are already stored do not depend on these limits. A config apply that adds or removes an S3 backend changes `max_object_size` (the 5 GiB bound) at once, and one that puts a filesystem backend on the device of the spool logs a warning that the 25% share applies at the next start. A `DGP_SPOOL_MAX_BYTES` that does not parse (for example `16GB`) is not fatal: the proxy logs a warning and uses `auto`.

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

The upgrade to v0.9 did not change the objects on disk. Only the config location changed. Current releases no longer read the objects that v0.9 to 2.0.14 encrypted (see [Encryption at rest, format v2](#encryption-at-rest-format-v2-2015)).

If you did not configure encryption before v0.9, you have nothing to do. Your backends default to `encryption: none`.

To move from proxy-AES to SSE-KMS as part of the upgrade, do the upgrade first and keep your existing key. Then move each bucket to a new SSE-KMS backend with the migrate job, because the re-encrypt job does not write to SSE backends. The steps are in [Recipe C: migrate from proxy-AES to SSE-KMS](rotate-encryption-keys.md#recipe-c-migrate-from-proxy-aes-to-sse-kms).

## Verify

After any upgrade or migration:

- [ ] `/_/health` returns HTTP 200, and `/_/ready` returns HTTP 200 (it checks the backends, the config DB, the jobs DB and the spool directory).
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
